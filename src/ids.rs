//! The id lookup table, used to resolve the uid/gid indices stored in inodes.
//!
//! See <https://dr-emann.github.io/squashfs/squashfs.html#_id_lookup_table>
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::deser;
use super::error::IdTableError;
use super::metadata::MetadataBlock;
use super::superblock::SuperBlock;

/// Id lookup table (a simple list of u32 ids, indexed by the `uid`/`gid` fields of inodes).
#[derive(Debug, Default)]
pub struct IdTable {
    ids: Vec<u32>,
}
impl std::fmt::Display for IdTable {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Id table with {} entries", self.ids.len())
    }
}
impl IdTable {
    /// Get the id (uid or gid) at the given index.
    pub fn id(&self, idx: u16) -> Result<u32, crate::Error> {
        self.ids
            .get(idx as usize)
            .copied()
            .ok_or(crate::Error::InvalidIdIndex)
    }
    /// Read the id table. Must be given an (offset-translating) reader positioned anywhere,
    /// as the table is accessed by absolute, image-relative offsets.
    pub async fn from_reader(
        superblock: &SuperBlock,
        mut r: impl crate::AsyncSeekBufRead,
    ) -> Result<Self, IdTableError> {
        if superblock.id_lookupcount == 0 {
            return Ok(Self::default());
        }
        r.seek(std::io::SeekFrom::Start(superblock.id_table_start))
            .await
            .map_err(IdTableError::ReadFailure)?;
        // Each metadata block is 8192 bytes, holding up to 2048 four-byte ids.
        let n_pointers = (superblock.id_lookupcount as f64 / 2048.0).ceil() as usize;
        let mut locations = Vec::<u64>::with_capacity(n_pointers);
        for _ in 0..n_pointers {
            locations.push(
                r.read_u64_le()
                    .await
                    .map_err(|_| IdTableError::InvalidLocation)?,
            )
        }
        let mut ids = Vec::<u32>::with_capacity(superblock.id_lookupcount as usize);
        for l in locations {
            r.seek(std::io::SeekFrom::Start(l))
                .await
                .map_err(IdTableError::ReadFailure)?;
            let block = MetadataBlock::from_reader(&mut r, superblock.compression).await?;
            ids.extend(
                block
                    .data
                    .chunks_exact(4)
                    .map(deser::bincode_deser::<u32>)
                    .collect::<Result<Vec<u32>, _>>()
                    .map_err(|_| IdTableError::InvalidEntry)?,
            );
        }
        if ids.len() < superblock.id_lookupcount as usize {
            return Err(IdTableError::InvalidLocation);
        }
        Ok(Self { ids })
    }
}
