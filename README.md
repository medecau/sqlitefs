# sqlite-fs

## About

sqlite-fs allows Linux and MacOS to mount a sqlite database file as a normal filesystem.

## Requirements

- Rust 1.97 or newer (rusqlite 0.40 supports the stable release current when it was published)
- libfuse3 (Linux) or macFUSE (macOS) is required by [fuser](https://github.com/cberner/fuser)

## Usage
### Mount a filesystem

```
$ sqlite-fs <mount_point> [<db_path>]
```

If a database file doesn't exist, sqlite-fs create db file and tables.

Pass mount options with `-o`, one per flag (`-o rdonly -o volname=Docs`). On macOS the volume is
named after the database file (`sqlitefs` for an in-memory database) unless `-o volname=<name>` is given.

Failed requests are logged with their cause. To trace every request (millisecond timestamps), run with
`RUST_LOG=debug` and keep stderr: `RUST_LOG=debug sqlite-fs <mount_point> <db_path> 2> sqlitefs.log`.

If a database file name isn't specified, sqlite-fs use in-memory-db instead of a file.
All data will be deleted when the filesystem is closed.

A file-backed database runs in SQLite's WAL mode: while mounted, `<db_path>-wal` and
`<db_path>-shm` sit next to it. On unmount sqlite-fs switches the file back to rollback-journal
mode, leaving one self-contained database file. Unmount before copying the database, or copy
all three files. If the process is killed without unmounting, the `-wal` file stays and is
recovered on the next mount.
Databases created by older versions are migrated to integer timestamps on first mount.
File data is stored in 64 KiB chunks; databases created by older versions keep their 4 KiB chunks.
Identical chunks are stored once, keyed by their BLAKE3 hash, so copies of a file take no extra space.
Older versions of sqlite-fs fail with I/O errors on shared chunks: before mounting a database with
one, unmount it and run `sqlite3 <db_path> < scripts/undo-dedup.sql`, which gives every file its own
copy of its chunks again.

### Unmount a filesystem

- Linux

```
$ fusermount -u <mount_point>
```

- Mac

```
$ umount <mount_point>
```

## example
```
$ sqlite-fs ~/mount ~/filesystem.sqlite &
$ echo "Hello world\!" > ~/mount/hello.txt
$ cat ~/mount/hello.txt
Hello world!
```

## functions

- [x] Create/Read/Delete directories
- [x] Create/Read/Write/Delete files
- [x] Change attributions
- [x] Copy/Move files
- [x] Create Hard Link and Symbolic Link
- [x] Read/Write extended attributes
- [x] Store identical file chunks once (BLAKE3-keyed deduplication)
- [x] File lock operations (POSIX advisory locks; `F_SETLKW` returns `EAGAIN` — blocking locks unsupported in single-threaded FUSE)
- [x] Strict error handling

