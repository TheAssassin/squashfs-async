//! Reader wrapper, adding a fixed offset to all seek operations.
//!
//! This is used to implement [`crate::Options::fs_offset`]: when a squashfs image is prepended
//! by a header (e.g. when stored in a container format), the raw positions in the underlying
//! file are shifted with respect to the image-relative positions stored in the squashfs
//! structures. Wrapping the underlying readers translates between the two coordinate systems:
//!
//! - Absolute seeks ([`std::io::SeekFrom::Start`]) with image-relative positions are translated
//!   to raw file positions.
//! - Positions (e.g. as reported by [`tokio::io::AsyncSeekExt::stream_position`]) are reported
//!   image-relative, i.e. `raw_position - offset`.
//!
//! Relative positions ([`std::io::SeekFrom::Current`]) are passed through unchanged (a relative
//! move is the same in both coordinate systems). [`std::io::SeekFrom::End`] is unresolved with
//! respect to the raw stream end (offset added); this is not used by this crate.
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, AsyncSeek};

/// Wrapper around a reader, adding `offset` to all absolute seeks.
pub struct OffsetReader<R> {
    inner: R,
    offset: u64,
}

impl<R> OffsetReader<R> {
    pub fn new(inner: R, offset: u64) -> Self {
        Self { inner, offset }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for OffsetReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<R: AsyncBufRead + Unpin> AsyncBufRead for OffsetReader<R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Pin::new(&mut self.get_mut().inner).poll_fill_buf(cx)
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amt)
    }
}

impl<R: AsyncSeek + Unpin> AsyncSeek for OffsetReader<R> {
    fn start_seek(self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        match position {
            io::SeekFrom::Start(n) => {
                Pin::new(&mut this.inner).start_seek(io::SeekFrom::Start(n + this.offset))
            }
            other => Pin::new(&mut this.inner).start_seek(other),
        }
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let raw = std::task::ready!(Pin::new(&mut this.inner).poll_complete(cx))?;
        // Report positions relative to the image, i.e. raw - offset. When reading before the
        // first translated seek (raw position < offset, e.g. after a pool recycle), saturate
        // to 0, as no meaningful image-relative position exists.
        Poll::Ready(Ok(raw.saturating_sub(this.offset)))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    const HEADER: &[u8] = b"PREPENDED JUNK HEADER NOT A MAGIC";
    const IMAGE: &[u8] = b"BYTES_ACCESSED_VIA_IMAGE_RELATIVE_POSITIONS";

    fn padded() -> std::io::Cursor<bytes::Bytes> {
        let mut bytes = bytes::BytesMut::new();
        bytes.extend_from_slice(HEADER);
        bytes.extend_from_slice(IMAGE);
        std::io::Cursor::new(bytes.freeze())
    }

    #[tokio::test]
    async fn offset_seek_position_read() -> anyhow::Result<()> {
        let mut r = OffsetReader::new(padded(), HEADER.len() as u64);
        r.seek(std::io::SeekFrom::Start(0)).await?;
        assert_eq!(r.stream_position().await? as usize, 0);
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await?;
        assert_eq!(buf, IMAGE);
        Ok(())
    }

    #[tokio::test]
    async fn offset_position_after_partial_read() -> anyhow::Result<()> {
        let mut r = OffsetReader::new(padded(), HEADER.len() as u64);
        r.seek(std::io::SeekFrom::Start(0)).await?;
        let mut buf = [0u8; 5];
        r.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"BYTES");
        assert_eq!(r.stream_position().await? as usize, 5);
        r.seek(std::io::SeekFrom::Start(6)).await?;
        assert_eq!(r.stream_position().await? as usize, 6);
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await?;
        assert_eq!(buf, &IMAGE[6..]);
        Ok(())
    }

    #[tokio::test]
    async fn relative_seek() -> anyhow::Result<()> {
        let mut r = OffsetReader::new(padded(), HEADER.len() as u64);
        r.seek(std::io::SeekFrom::Start(3)).await?;
        r.seek(std::io::SeekFrom::Current(2)).await?;
        assert_eq!(r.stream_position().await? as usize, 5);
        r.seek(std::io::SeekFrom::Start(0)).await?;
        assert_eq!(r.stream_position().await? as usize, 0);
        Ok(())
    }

    #[tokio::test]
    async fn saturate_before_first_seek() -> anyhow::Result<()> {
        // Raw position 0 (below the offset) is reported as 0, rather than panicking on
        // underflow.
        let mut r = OffsetReader::new(padded(), HEADER.len() as u64);
        assert_eq!(r.stream_position().await? as usize, 0);
        Ok(())
    }
}
