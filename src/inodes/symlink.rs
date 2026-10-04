use async_trait::async_trait;
use serde::Deserialize;

use super::super::data::BlockSize;
use super::super::error::InodeTableError;
use super::super::fragments::FragmentLocation;
use super::InodeHeader;
use crate::deser;

#[derive(Debug, Default, Deserialize)]
pub struct Symlink {
    #[allow(dead_code)]
    link_count: u32,
    target_size: u32,
    #[serde(skip)]
    target: String,
    #[serde(skip)]
    block_sizes: Vec<BlockSize>,
    #[serde(skip)]
    permissions: u16,
    #[serde(skip)]
    uid_idx: u16,
    #[serde(skip)]
    gid_idx: u16,
    #[serde(skip)]
    modified_time: u32,
}
impl Symlink {
    pub async fn from_reader(
        mut r: impl crate::AsyncRead,
        header: &InodeHeader,
    ) -> Result<Self, InodeTableError> {
        let mut link: Self = deser::bincode_deser_from(&mut r, 8)
            .await
            .map_err(|_| InodeTableError::InvalidEntry)?;
        link.target = deser::bincode_deser_string_from(r, link.target_size as usize)
            .await
            .map_err(|_| InodeTableError::InvalidEntry)?;
        link.permissions = header.permissions;
        link.uid_idx = header.uid_idx;
        link.gid_idx = header.gid_idx;
        link.modified_time = header.modified_time;
        Ok(link)
    }
}
#[async_trait]
impl super::file::FileInode for Symlink {
    fn blocks_start(&self) -> u64 {
        0
    }
    fn add_block_size(&mut self, _size: BlockSize) {}
    fn block_sizes(&self) -> &Vec<BlockSize> {
        &self.block_sizes
    }
    fn file_size(&self) -> u64 {
        self.target.len() as u64
    }
    fn fragment(&self) -> FragmentLocation {
        FragmentLocation {
            index: 0xFFFF_FFFF,
            offset: 0,
        }
    }
    fn permissions(&self) -> u16 {
        self.permissions
    }
    fn uid_idx(&self) -> u16 {
        self.uid_idx
    }
    fn gid_idx(&self) -> u16 {
        self.gid_idx
    }
    fn modified_time(&self) -> u32 {
        self.modified_time
    }
    fn is_symlink(&self) -> bool {
        true
    }
    fn target(&self) -> Option<&str> {
        Some(&self.target)
    }
}
