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
`Sqlite` implements `DbModule` using `rusqlite`. The database schema has five tables:
- `metadata` — inode attributes (size, timestamps, permissions, uid/gid)
- `dentry` — directory entries mapping `(parent_id, name)` → `child_id`; index `dentry_child_id` on `child_id` backs link counts and the cascade check
- `data` — file content in chunks of up to `block_size` bytes keyed by `(file_id, block_num)` (1-based); partial chunks are stored unpadded and reads zero-fill, missing rows are holes
- `xattr` — extended attributes as key/value pairs per inode
- `config` — `block_size` row: 65536 for new databases, 4096 for databases created before the table existed (kept as-is, no migration)

**Key design decisions:**
- Inode deletion is deferred: an inode is only deleted when its `dentry` reference count drops to zero AND the kernel has issued `forget` (tracked via `lookup_count`). This correctly handles the POSIX case of deleting an open file.
- `macOS` vs Linux: `readdir` has two `#[cfg]` implementations because the macOS FUSE API doesn't cache dentries at `opendir` time the same way.
- Foreign keys are explicitly enabled on every connection (`PRAGMA foreign_keys=ON`) since SQLite disables them by default.
- Timestamps are stored as integer nanoseconds since the epoch (`*_ns` columns), saturating outside 1677–2262. `init()` migrates databases that still have the old text + `*_nsec` columns in place.
- `nlink` and `blocks` are computed with `count(*)` scalar subqueries (the `metadata.nlink` column is unused), so an unlinked-but-open inode reports `nlink` 0. `blocks` is st_blocks in 512-byte units: stored chunks x `block_size`, capped at the size rounded up to 512.
- File-backed DBs use `journal_mode=WAL` with `synchronous=NORMAL`: commits are not fsync'd until a checkpoint, which the `fsync`/`fsyncdir` handlers force via `DbModule::checkpoint`. `destroy()` calls `DbModule::unmount`, which switches to `journal_mode=DELETE` so an unmounted DB is one file (header no longer marks it WAL); `Sqlite::new` re-enables WAL on the next mount.
- Reads never write: no atime updates on lookup or read (noatime semantics). Each `write()` request stores all its chunks, the new size, and mtime/ctime in one transaction. A write that lands inside a chunk's stored bytes patches them via incremental blob I/O (`blob_open`/`write_at`); one that extends a chunk rewrites it. Writes or truncates past `u32::MAX * block_size` fail with `EFBIG`.

## Error Handling

Errors flow through `src/sqerror.rs`: `Error` variants (using `thiserror`) carry their POSIX errno via `Error::to_errno()`. All filesystem handlers call `err.to_errno()` and log at `warn!` level — no per-handler errno mapping. To add a new error case: add a variant to `Error` and add a corresponding arm to `to_errno()`.

Errors in fire-and-forget callbacks (`init`, `destroy`, `forget`) are logged at `warn!` but cannot be returned to the kernel. Non-UTF-8 filenames reply `EINVAL`. SQLite errors map to `EIO`.
