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

# Security checks (CI `security` job): RustSec advisories, then the bundled
# SQLite against sqlite.org/cves.html (RustSec doesn't track SQLite's own CVEs)
cargo audit
python3 scripts/check-sqlite-cves.py [CVE-ID ...]   # listed IDs are ignored

# Undo chunk sharing in an unmounted DB so builds before dedup can read it
sqlite3 <db_path> < scripts/undo-dedup.sql

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
`Sqlite` implements `DbModule` using `rusqlite`. The database schema has six tables:
- `metadata` — inode attributes (size, timestamps, permissions, uid/gid)
- `dentry` — directory entries mapping `(parent_id, name)` → `child_id`; index `dentry_child_id` on `child_id` backs link counts and the cascade check
- `data` — file content in chunks of up to `block_size` bytes keyed by `(file_id, block_num)` (1-based); partial chunks are stored unpadded and reads zero-fill, missing rows are holes. A row holds either its own bytes (`data`) or a `chunk_hash` pointing into `chunk`
- `chunk` — shared chunk bytes, one row per distinct content, keyed by BLAKE3 hash and never modified; triggers `chunk_gc_delete`/`chunk_gc_update` delete a chunk with the last `data` row pointing at it (FK cascades included), and the partial index `data_chunk_hash` backs that check
- `xattr` — extended attributes as key/value pairs per inode
- `config` — `block_size` row: 65536 for new databases, 4096 for databases created before the table existed (kept as-is, no migration)

**Key design decisions:**
- Inode deletion is deferred: an inode is only deleted when its `dentry` reference count drops to zero AND the kernel has issued `forget` (tracked via `lookup_count`). This correctly handles the POSIX case of deleting an open file.
- `macOS` vs Linux: `readdir` has two `#[cfg]` implementations because the macOS FUSE API doesn't cache dentries at `opendir` time the same way.
- Foreign keys are explicitly enabled on every connection (`PRAGMA foreign_keys=ON`) since SQLite disables them by default.
- Timestamps are stored as integer nanoseconds since the epoch (`*_ns` columns), saturating outside 1677–2262. `init()` migrates databases that still have the old text + `*_nsec` columns in place.
- `nlink` and `blocks` are computed with `count(*)` scalar subqueries (the `metadata.nlink` column is unused), so an unlinked-but-open inode reports `nlink` 0. `blocks` is st_blocks in 512-byte units: stored chunks x `block_size`, capped at the size rounded up to 512.
- File-backed DBs use `journal_mode=WAL` with `synchronous=NORMAL`: commits are not fsync'd until a checkpoint, which the `fsync`/`fsyncdir` handlers force via `DbModule::checkpoint`. `destroy()` calls `DbModule::unmount`, which switches to `journal_mode=DELETE` so an unmounted DB is one file (header no longer marks it WAL); `Sqlite::new` re-enables WAL on the next mount.
- Reads never write: no atime updates on lookup or read (noatime semantics). Each `write()` request stores all its chunks, the new size, and mtime/ctime in one transaction. Byte ranges are cut and merged by SQLite, not in Rust: reads take each chunk's overlap with `substr()`; a write that lands inside a chunk's stored bytes patches them via incremental blob I/O (`blob_open`/`write_at`); one that creates or extends a chunk is an UPSERT building `substr(head) || zeroblob(gap) || new bytes`, `CAST` back to BLOB because `||` yields TEXT (on which `substr`/`length` count characters). Writes or truncates past `u32::MAX * block_size` fail with `EFBIG`.
- Dedup: chunk content is shared by BLAKE3 hash, trusted without a byte comparison. A chunk is shared when a write covers it whole, when it becomes full, or when a writable handle is released (`DbModule::dedup_data`, which catches short tails and chunks overwritten in place). Changing a shared chunk (write or truncate) first copies it back into the row (`unshare_chunk`), so a shared chunk is never edited and other files keep their bytes. Only rows with their own bytes are patched in place. Databases from before dedup gain the `chunk_hash` column on mount; a file's rows stay private until it is next released from a writable handle. Builds before dedup fail with `EIO` on shared chunks until `scripts/undo-dedup.sql` copies them back. `st_blocks` counts a shared chunk in every file using it.

## Error Handling

Errors flow through `src/sqerror.rs`: `Error` variants (using `thiserror`) carry their POSIX errno via `Error::to_errno()`. All filesystem handlers call `err.to_errno()` and log at `warn!` level — no per-handler errno mapping. To add a new error case: add a variant to `Error` and add a corresponding arm to `to_errno()`.

Errors in fire-and-forget callbacks (`init`, `destroy`, `forget`, and the dedup step in `release`) are logged at `warn!` but cannot be returned to the kernel. The default log filter is `warn` (`RUST_LOG` overrides it; `fuser` is held at `error` because it warns on every call to an unimplemented op such as `copy_file_range` and after every external unmount, while its reply failures are errors), so expected misses such as a missing xattr reply without logging. `main` prints one line once mounted and one when the session ends (unmounted, or stopped with the error). Non-UTF-8 filenames reply `EINVAL`. SQLite errors map to `EIO`.
