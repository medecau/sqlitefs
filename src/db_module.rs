pub mod sqlite;
use crate::sqerror::Result;
use fuser::{FileAttr, FileType, INodeNo};
use std::time::SystemTime;

pub trait DbModule {
    /// Create tables (if not found), migrate legacy text timestamps to integer
    /// nanoseconds, and add root directory (if not found)
    fn init(&mut self) -> Result<()>;
    /// Get metadata. If not found, return None
    fn get_inode(&self, inode: u32) -> Result<Option<DBFileAttr>>;
    /// Add a file or a directory.
    /// Update atime, mtime, ctime. Update mtime and ctime of the parent directory.
    fn add_inode_and_dentry(&mut self, parent: u32, name: &str, attr: &DBFileAttr) -> Result<u32>;
    /// Update file metadata.
    /// Update ctime. Update mtime if filesize is changed.
    fn update_inode(&mut self, attr: &DBFileAttr, truncate: bool) -> Result<()>;
    // Delete an inode if the link count is zero.
    fn delete_inode_if_noref(&mut self, inode: u32) -> Result<()>;
    /// Get directory entries
    fn get_dentry(&self, inode: u32) -> Result<Vec<DEntry>>;
    /// Add a new directory entry which is hard link
    /// Update mtime, Update mtime and ctime of the parent directory.
    fn link_dentry(&mut self, inode: u32, parent: u32, name: &str) -> Result<DBFileAttr>;
    /// Delete a dentry. returns target inode.
    /// Update ctime. Update mtime and ctime of the parent directory.
    fn delete_dentry(&mut self, parent: u32, name: &str) -> Result<u32>;
    /// Move dentry to another parent or name. Return inode number if a new file is overwrote.
    /// Update ctime, and mtime and ctime of the parent directories.
    fn move_dentry(
        &mut self,
        parent: u32,
        name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<Option<u32>>;
    /// check a directory if it is empty.
    fn check_directory_is_empty(&self, inode: u32) -> Result<bool>;
    /// lookup a directory entry table and get a file attribute.
    /// If not found, return None.
    /// Read-only: atime is never updated (noatime semantics).
    fn lookup(&self, parent: u32, name: &str) -> Result<Option<DBFileAttr>>;
    /// Read `size` bytes at `offset`; ranges with no stored data read as zeros.
    /// Read-only: no atime update.
    fn read_data(&self, inode: u32, offset: u64, size: u32) -> Result<Vec<u8>>;
    /// Write `data` at `offset` in one transaction; grow the file size to the
    /// end of the write if larger. Update mtime and ctime. Fails with EFBIG past
    /// the largest addressable size.
    fn write_data(&mut self, inode: u32, offset: u64, data: &[u8]) -> Result<()>;
    /// Make all committed transactions durable (forces a WAL checkpoint).
    fn checkpoint(&self) -> Result<()>;
    /// Release all data related to an inode number.
    fn release_data(&self, inode: u32) -> Result<()>;
    /// Delete all inodes which nlink is 0.
    fn delete_all_noref_inode(&mut self) -> Result<()>;
    /// Chunk size of stored file data (per database: 65536 new, 4096 legacy)
    fn get_db_block_size(&self) -> u32;
    /// Set xattr value.
    fn set_xattr(&mut self, inode: u32, key: &str, value: &[u8]) -> Result<()>;
    /// Get xattr value.
    fn get_xattr(&self, inode: u32, key: &str) -> Result<Vec<u8>>;
    /// List xattr name.
    fn list_xattr(&self, inode: u32) -> Result<Vec<String>>;
    /// Delete xattr
    fn delete_xattr(&mut self, inode: u32, key: &str) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Hash, PartialEq)]
pub struct DBFileAttr {
    /// Inode number
    pub ino: u32,
    /// Size in bytes
    pub size: u64,
    /// Allocated size in 512-byte units (st_blocks)
    pub blocks: u64,
    /// Time of last access
    pub atime: SystemTime,
    /// Time of last modification
    pub mtime: SystemTime,
    /// Time of last change
    pub ctime: SystemTime,
    /// Time of creation (macOS only)
    pub crtime: SystemTime,
    /// file type
    pub kind: FileType,
    /// Permissions
    pub perm: u16,
    /// Number of hard links
    pub nlink: u32,
    /// User id
    pub uid: u32,
    /// Group id
    pub gid: u32,
    /// Rdev
    pub rdev: u32,
    /// Flags (macOS only, see chflags(2))
    pub flags: u32,
}

impl DBFileAttr {
    pub fn get_file_attr(&self) -> FileAttr {
        FileAttr {
            ino: INodeNo(self.ino as u64),
            size: self.size,
            blocks: self.blocks,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            crtime: self.crtime,
            kind: self.kind,
            perm: self.perm,
            nlink: self.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: self.rdev,
            blksize: 4096,
            flags: self.flags,
        }
    }
}

pub struct DEntry {
    pub parent_ino: u32,
    pub child_ino: u32,
    pub filename: String,
    pub file_type: FileType,
}
