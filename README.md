# sqlite-fs

## About

sqlite-fs allows Linux and MacOS to mount a sqlite database file as a normal filesystem.

## Requirements

- Latest Rust Programming Language (≥ 1.75)
- libfuse3 (Linux) or macFUSE (macOS) is required by [fuser](https://github.com/cberner/fuser)

## Usage
### Mount a filesystem

```
$ sqlite-fs <mount_point> [<db_path>]
```

If a database file doesn't exist, sqlite-fs create db file and tables.

If a database file name isn't specified, sqlite-fs use in-memory-db instead of a file.
All data will be deleted when the filesystem is closed.

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
- [x] File lock operations (POSIX advisory locks; `F_SETLKW` returns `EAGAIN` — blocking locks unsupported in single-threaded FUSE)
- [ ] Strict error handling

