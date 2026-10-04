//! Regression tests for [`squashfs_async::Options::fs_offset`]: squashfs images prepended by
//! a header, requiring all reads to be offset accordingly.
//!
//! For each compression spec, a squashfs image covering a variety of file shapes (fragmented
//! small file, multi-block file with tail, sparse file, nested directory) is created, then
//! opened as-is and as a copy prepended by a junk header. Reads (full and partial) through the
//! filesystem API must be identical in both cases, and match the original contents.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;

use fuser_async::Filesystem;
use squashfs_async::pools::LocalReadersPoolTokio;
use squashfs_async::{Error, Options, SquashFs};

/// Deliberately odd, non-aligned (prime) header size, so that no comparison passes because of
/// accidentally aligned accesses.
const HEADER_LEN: u64 = 4013;

/// Default block size of the test images.
const BLOCK: usize = 131_072;

/// Same as in `tests/testdata.rs`: (name, mksquashfs options). The `nocomp` spec has no
/// fragments, which triggers the direct/O_DIRECT small-file reading path.
const SPECS: [(&str, &[&str]); 3] = [
    ("nocomp", &["-noI", "-noId", "-noD", "-noF", "-noX"]),
    ("gzip", &["-comp", "gzip", "-Xcompression-level", "1"]),
    ("zstd", &["-comp", "zstd", "-Xcompression-level", "1"]),
];

/// Deterministic pseudo-random pattern of the given size.
fn pattern(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|i| (i as u8).wrapping_mul(13 + seed).wrapping_add(seed))
        .collect()
}

/// Path→contents of the test files, created by [`create_image`].
fn test_files() -> BTreeMap<String, Vec<u8>> {
    [
        ("small".to_string(), pattern(4321, 1)),
        ("medium".to_string(), pattern(300_003, 2)),
        ("large".to_string(), pattern(200_001, 3)),
        ("zeros".to_string(), vec![0u8; 400_000]),
        ("subdir/nested".to_string(), pattern(70_000, 4)),
    ]
    .into_iter()
    .collect()
}

fn mksquashfs(input: &Path, dest: &Path, options: &[&str]) -> anyhow::Result<()> {
    let cmd = std::process::Command::new("mksquashfs")
        .args([input, dest])
        .args(["-mkfs-time", "0", "-reproducible", "-no-xattrs"])
        .args(options)
        .output()?;
    if !cmd.status.success() {
        anyhow::bail!(
            "Failed to run mksquashfs: {}",
            std::str::from_utf8(&cmd.stderr)?
        );
    }
    Ok(())
}

/// Create a squashfs image with the given options, containing the files of [`test_files`].
fn create_image(dir: &Path, spec: &str, options: &[&str]) -> anyhow::Result<std::path::PathBuf> {
    let contents = dir.join("contents");
    if contents.exists() {
        std::fs::remove_dir_all(&contents)?;
    }
    for (path, bytes) in test_files() {
        let path = contents.join(&path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, &bytes)?;
    }
    let image = dir.join(format!("{spec}.squashfs"));
    mksquashfs(&contents, &image, options)?;
    std::fs::remove_dir_all(&contents)?;
    Ok(image)
}

/// Create a copy of `image`, prepended by a deterministic junk header.
fn padded_copy(image: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(dest)?);
    let mut header = Vec::with_capacity(HEADER_LEN as usize);
    for i in 0..HEADER_LEN {
        header.push((i as u8).wrapping_mul(29).wrapping_add(7));
    }
    // Sanity: the squashfs magic ("hsqs", little-endian) must not appear in the header.
    assert!(!header.windows(4).any(|w| w == b"hsqs"));
    f.write_all(&header)?;
    std::io::copy(&mut std::fs::File::open(image)?, &mut f)?;
    Ok(())
}

fn options(fs_offset: u64, direct_limit: usize) -> Options {
    Options {
        cache_mb: 32,
        readers: 2,
        direct_limit,
        fs_offset,
    }
}

/// Lookup a file by path (from the FUSE filesystem root).
async fn lookup_path(
    fs: &SquashFs<LocalReadersPoolTokio>,
    path: &str,
) -> anyhow::Result<fuser::FileAttr> {
    let parts: Vec<&OsStr> = path.split('/').map(OsStr::new).collect();
    let mut attr = fs.lookup(fuser::FUSE_ROOT_ID, parts[0]).await?;
    for part in &parts[1..] {
        attr = fs.lookup(attr.ino, part).await?;
    }
    Ok(attr)
}

/// Read the file at `path` through the filesystem API: full read, a few slices, and the Exif
/// -style tail.
async fn read_file(
    fs: &SquashFs<LocalReadersPoolTokio>,
    path: &str,
    flags: i32,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let attr = lookup_path(fs, path).await?;
    let fh = fs.open(attr.ino, flags).await?;
    let size = attr.size as usize;
    let mut reads = vec![
        fs.read(attr.ino, fh, 0, attr.size as u32).await,
        fs.read(attr.ino, fh, 117, 731).await,
        fs.read(attr.ino, fh, (size - 9) as i64, 9).await,
    ];
    if size > BLOCK {
        reads.push(fs.read(attr.ino, fh, (BLOCK - 72) as i64, 400).await);
    }
    fs.release(attr.ino, fh).await?;
    reads
        .into_iter()
        .map(|b| b.map_err(|e| anyhow::anyhow!("{e}")).map(|b| b.to_vec()))
        .collect()
}

/// Recursively list the files of the filesystem, via readdir.
async fn list_files(
    fs: &SquashFs<LocalReadersPoolTokio>,
    dir: u64,
    dir_name: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let mut res = BTreeSet::new();
    let entries = fs.readdir(dir, 0).await?;
    for e in entries {
        let name = if dir_name.is_empty() {
            e.name
        } else {
            format!("{dir_name}/{}", e.name)
        };
        match e.file_type {
            fuser::FileType::Directory => {
                let children = Box::pin(list_files(fs, e.inode, &name)).await?;
                res.extend(children);
            }
            fuser::FileType::RegularFile => {
                res.insert(name);
            }
            _ => {}
        }
    }
    Ok(res)
}

/// Check every read shape (full/slices) against the ground truth file contents.
async fn check_reads(
    fs: &SquashFs<LocalReadersPoolTokio>,
    direct_flags: i32,
) -> anyhow::Result<()> {
    for (path, content) in test_files() {
        let reads = read_file(fs, &path, direct_flags).await?;
        let check = |i: usize, offset: usize, other: &[u8], what: &str| {
            assert_eq!(
                reads[i],
                content[offset..offset + other.len()],
                "{what} mismatch for {path}"
            );
        };
        check(0, 0, &content, "full read");
        check(1, 117, &content[117..848], "interior slice");
        check(
            2,
            content.len() - 9,
            &content[content.len() - 9..],
            "tail slice",
        );
        if content.len() > BLOCK {
            check(
                3,
                BLOCK - 72,
                &content[BLOCK - 72..BLOCK + 328],
                "block-crossing slice",
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn fs_offset() -> anyhow::Result<()> {
    for (spec, mksquashfs_options) in &SPECS {
        let dir = tempfile::tempdir()?;
        println!("Testing spec {spec:?}");
        let image = create_image(dir.path(), spec, mksquashfs_options)?;
        let padded = dir.path().join("padded.squashfs");
        padded_copy(&image, &padded)?;

        // Without the fs_offset option, the padded image cannot be opened; ditto with a wrong
        // offset.
        let res = SquashFs::<LocalReadersPoolTokio>::open(&padded, &options(0, 0)).await;
        assert!(
            matches!(res, Err(Error::InvalidSuperblock)),
            "unexpected result opening padded image without offset: {:?}",
            res.err().map(|e| e.to_string())
        );
        let res =
            SquashFs::<LocalReadersPoolTokio>::open(&padded, &options(HEADER_LEN + 64, 0)).await;
        assert!(
            matches!(res, Err(Error::InvalidSuperblock)),
            "unexpected result opening padded image with wrong offset: {:?}",
            res.err().map(|e| e.to_string())
        );

        let original = SquashFs::<LocalReadersPoolTokio>::open(&image, &options(0, 0)).await?;
        let with_offset =
            SquashFs::<LocalReadersPoolTokio>::open(&padded, &options(HEADER_LEN, 0)).await?;

        // Listing must be identical and match the expected file set.
        let expected: BTreeSet<String> = test_files().keys().cloned().collect();
        assert_eq!(
            list_files(&original, fuser::FUSE_ROOT_ID, "").await?,
            expected
        );
        assert_eq!(
            list_files(&with_offset, fuser::FUSE_ROOT_ID, "").await?,
            expected
        );

        // All read shapes must both be correct and identical.
        check_reads(&original, 0).await?;
        check_reads(&with_offset, 0).await?;
        for path in test_files() {
            let a = read_file(&original, path.0.as_str(), 0).await?;
            let b = read_file(&with_offset, path.0.as_str(), 0).await?;
            assert_eq!(a, b);
        }
    }
    Ok(())
}

/// Exercise the direct_limit/O_DIRECT small-file reading path with an offset.
#[tokio::test]
async fn fs_offset_direct_limit() -> anyhow::Result<()> {
    // nocomp images have no fragments, required by the small-file direct path.
    let (spec, mksquashfs_options) = &SPECS[0];
    let dir = tempfile::tempdir()?;
    let image = create_image(dir.path(), spec, mksquashfs_options)?;
    let padded = dir.path().join("padded.squashfs");
    padded_copy(&image, &padded)?;

    let original = SquashFs::<LocalReadersPoolTokio>::open(&image, &options(0, 300_000)).await?;
    let with_offset =
        SquashFs::<LocalReadersPoolTokio>::open(&padded, &options(HEADER_LEN, 300_000)).await?;

    check_reads(&original, libc::O_DIRECT).await?;
    check_reads(&with_offset, libc::O_DIRECT).await?;
    for path in test_files() {
        let a = read_file(&original, path.0.as_str(), libc::O_DIRECT).await?;
        let b = read_file(&with_offset, path.0.as_str(), libc::O_DIRECT).await?;
        assert_eq!(a, b);
    }
    Ok(())
}
