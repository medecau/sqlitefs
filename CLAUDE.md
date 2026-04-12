# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
# Build
cargo build

# Run all tests
cargo test

# Run a single test by name
cargo test sqlite_init_db

# Run with debug logging
RUST_LOG=debug cargo run -- <mount_point> [<db_path>]

# Mount using in-memory DB (data lost on unmount)
cargo run -- ~/mount

# Mount with a file-backed DB
cargo run -- ~/mount ~/filesystem.sqlite
```

Unmount on Linux: `fusermount -u <mount_point>`  
Unmount on macOS: `umount <mount_point>`

## Architecture

The codebase has three layers:

**1. FUSE interface (`src/filesystem.rs`)**  
`SqliteFs` implements the `fuser::Filesystem` trait. Every filesystem syscall (lookup, read, write, mkdir, rename, etc.) is a method on this struct. It maintains four in-memory `Arc<Mutex<HashMap>>` caches:
- `lookup_count`: reference counts for open inodes (drives deferred deletion)
- `open_file_handler`: per-inode file handle tracking (flags: readonly, append, noatime)
- `open_dir_handler`: per-inode directory entry snapshots taken at `opendir` time
- `locks`: per-inode POSIX advisory lock lists (`Vec<PosixLock>`); `getlk`/`setlk`/`flush` operate on this; `F_SETLKW` returns `EAGAIN` (blocking would deadlock the single-threaded FUSE event loop)

**2. DB abstraction (`src/db_module.rs`)**  
`DbModule` is a trait defining all storage operations. `DBFileAttr` is the internal inode struct. All filesystem methods call through this trait.

**3. SQLite implementation (`src/db_module/sqlite.rs`)**  
`Sqlite` implements `DbModule` using `rusqlite`. The database schema has four tables:
- `metadata` — inode attributes (size, timestamps, permissions, uid/gid)
- `dentry` — directory entries mapping `(parent_id, name)` → `child_id`
- `data` — file content stored as fixed 4096-byte blobs keyed by `(file_id, block_num)`
- `xattr` — extended attributes as key/value pairs per inode

**Key design decisions:**
- Inode deletion is deferred: an inode is only deleted when its `dentry` reference count drops to zero AND the kernel has issued `forget` (tracked via `lookup_count`). This correctly handles the POSIX case of deleting an open file.
- `macOS` vs Linux: `readdir` has two `#[cfg]` implementations because the macOS FUSE API doesn't cache dentries at `opendir` time the same way.
- Foreign keys are explicitly enabled on every connection (`PRAGMA foreign_keys=ON`) since SQLite disables them by default.
- Timestamps are stored as text (`%Y-%m-%d %H:%M:%S`) with a separate `*_nsec` integer column for sub-second precision.

## Error Handling

Errors flow through `src/sqerror.rs`: `Error` variants (using `thiserror`) map to specific `libc` errno values in `filesystem.rs`. New filesystem error cases should add a variant to `Error` and match it in the relevant `filesystem.rs` handler.
