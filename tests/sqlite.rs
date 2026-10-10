use fuser::FileType;
use rusqlite::Connection;
use sqlite_fs::db_module::{sqlite, DBFileAttr, DbModule};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod helpers;
#[test]
fn sqlite_create_db() {
    sqlite::Sqlite::new_in_memory().expect("failed to create db in memory");
}

#[test]
fn sqlite_create_db_file() {
    let mut dbf = helpers::DBWithTempFile::new();
    dbf.db.init().expect("failed to create db file");
}

#[test]
fn sqlite_init_db() {
    let mut db = sqlite::Sqlite::new_in_memory().expect("failed to create db in memory");
    db.init().expect("failed to init db");
}

fn fresh_db() -> sqlite::Sqlite {
    let mut db = sqlite::Sqlite::new_in_memory().unwrap();
    db.init().unwrap();
    db
}

fn file_attr() -> DBFileAttr {
    let now = SystemTime::now();
    DBFileAttr {
        ino: 0,
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
    }
}

#[test]
fn getattr_after_unlink_reports_zero_links() {
    let mut db = fresh_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    db.delete_dentry(1, "f").unwrap();
    // The inode survives until forget; getattr on the open fd must still work.
    let attr = db.get_inode(ino).unwrap().expect("inode kept until forget");
    assert_eq!(attr.nlink, 0);
}

#[test]
fn reads_do_not_touch_atime() {
    let mut db = fresh_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
    for i in [1, ino] {
        let mut attr = db.get_inode(i).unwrap().unwrap();
        attr.atime = old;
        db.update_inode(&attr, false).unwrap();
    }
    db.lookup(1, "f").unwrap();
    db.read_data(ino, 0, 4096).unwrap();
    assert_eq!(db.get_inode(1).unwrap().unwrap().atime, old);
    assert_eq!(db.get_inode(ino).unwrap().unwrap().atime, old);
}

#[test]
fn timestamps_round_trip_with_nanoseconds() {
    let mut db = fresh_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    let t = UNIX_EPOCH + Duration::new(1_234_567_890, 123_456_789);
    let mut attr = db.get_inode(ino).unwrap().unwrap();
    attr.atime = t;
    attr.crtime = t;
    db.update_inode(&attr, false).unwrap();
    let back = db.get_inode(ino).unwrap().unwrap();
    assert_eq!((back.atime, back.crtime), (t, t));
}

#[test]
fn out_of_range_timestamps_are_clamped_not_fatal() {
    let mut db = fresh_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    for secs in [253_402_300_800u64, 9_000_000_000_000] {
        // year 10000, and ~285,000 years out
        let mut attr = db.get_inode(ino).unwrap().unwrap();
        attr.mtime = UNIX_EPOCH + Duration::from_secs(secs);
        db.update_inode(&attr, false).unwrap();
        let back = db.get_inode(ino).unwrap().expect("inode readable");
        assert!(back.mtime > UNIX_EPOCH + Duration::from_secs(9_000_000_000)); // ~year 2255+
    }
}

#[test]
fn legacy_text_timestamps_are_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite");
    {
        // Schema written by sqlite-fs before integer timestamps.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata(id integer primary key autoincrement, size int default 0 not null, \
             atime text, atime_nsec int, mtime text, mtime_nsec int, ctime text, ctime_nsec int, \
             crtime text, crtime_nsec int, kind int, mode int, nlink int default 0 not null, \
             uid int default 0, gid int default 0, rdev int default 0, flags int default 0);
             INSERT INTO metadata VALUES(1,0,'2001-09-09 01:46:40',123456789,'2001-09-09 01:46:40',123456789,
               '2001-09-09 01:46:40',123456789,'2001-09-09 01:46:40',123456789,16384,16895,0,0,0,0,0);
             INSERT INTO metadata VALUES(2,0,'not a time',0,'2001-09-09 01:46:40',5,
               '2001-09-09 01:46:40',5,'2001-09-09 01:46:40',5,32768,33188,0,0,0,0,0);",
        )
        .unwrap();
    }
    let mut db = sqlite::Sqlite::new(&path).unwrap();
    db.init().unwrap();
    assert_eq!(
        db.get_db_block_size(),
        4096,
        "existing databases keep their 4 KiB block layout"
    );

    let root = db.get_inode(1).unwrap().unwrap();
    assert_eq!(
        root.mtime,
        UNIX_EPOCH + Duration::new(1_000_000_000, 123_456_789)
    );
    let file = db.get_inode(2).unwrap().unwrap();
    assert_eq!(
        file.atime, UNIX_EPOCH,
        "unparseable text falls back to the epoch"
    );
    let raw: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT mtime_ns FROM metadata WHERE id = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(raw, 1_000_000_000_000_000_005);
}

#[test]
fn file_db_uses_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal.sqlite");
    let mut db = sqlite::Sqlite::new(&path).unwrap();
    db.init().unwrap();
    let mode: String = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn link_count_queries_use_the_child_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.sqlite");
    let mut db = sqlite::Sqlite::new(&path).unwrap();
    db.init().unwrap();
    let conn = Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare("EXPLAIN QUERY PLAN SELECT count(*) FROM dentry WHERE child_id = 1")
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map([], |r| r.get(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        plan.iter().any(|d| d.contains("dentry_child_id")),
        "{plan:?}"
    );
}

/// File-backed fresh DB plus a raw connection for inspecting the stored chunks.
fn file_db() -> (tempfile::TempDir, std::path::PathBuf, sqlite::Sqlite) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunks.sqlite");
    let mut db = sqlite::Sqlite::new(&path).unwrap();
    db.init().unwrap();
    (dir, path, db)
}

fn chunk_lengths(path: &std::path::Path, ino: u32) -> Vec<i64> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT length(data) FROM data WHERE file_id = ? ORDER BY block_num")
        .unwrap();
    stmt.query_map([ino], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn new_db_stores_64k_chunks_unpadded() {
    let (_dir, path, mut db) = file_db();
    assert_eq!(db.get_db_block_size(), 65536);
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    for (i, piece) in data.chunks(128 * 1024).enumerate() {
        db.write_data(ino, i as u64 * 128 * 1024, piece).unwrap();
    }
    assert_eq!(db.read_data(ino, 0, 200_000).unwrap(), data);
    assert_eq!(
        db.read_data(ino, 65_000, 1_000).unwrap(),
        data[65_000..66_000]
    );
    // 3 full chunks and a 3,392-byte tail, stored as written (not padded to 64 KiB).
    assert_eq!(chunk_lengths(&path, ino), vec![65536, 65536, 65536, 3392]);
    let attr = db.get_inode(ino).unwrap().unwrap();
    assert_eq!((attr.size, attr.blocks), (200_000, 391)); // st_blocks: 512-byte units
}

#[test]
fn small_writes_patch_in_place_or_grow_the_chunk() {
    let (_dir, path, mut db) = file_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    let rowid = |path: &std::path::Path| -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT rowid FROM data", [], |r| r.get(0))
            .unwrap()
    };
    db.write_data(ino, 0, b"0123456789").unwrap();
    let before = rowid(&path);
    db.write_data(ino, 2, b"xy").unwrap(); // inside the stored bytes
    assert_eq!(db.read_data(ino, 0, 10).unwrap(), b"01xy456789");
    assert_eq!(chunk_lengths(&path, ino), vec![10]);
    assert_eq!(rowid(&path), before, "patched in place, not REPLACEd");
    db.write_data(ino, 20, b"z").unwrap(); // past the stored bytes: grows, gap reads as zeros
    assert_eq!(
        db.read_data(ino, 8, 13).unwrap(),
        b"89\0\0\0\0\0\0\0\0\0\0z"
    );
    assert_eq!(chunk_lengths(&path, ino), vec![21]);
    assert_eq!(db.get_inode(ino).unwrap().unwrap().size, 21);
}

#[test]
fn sparse_files_store_only_written_chunks() {
    let (_dir, path, mut db) = file_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    db.write_data(ino, 1 << 20, b"tail").unwrap();
    assert_eq!(db.read_data(ino, 0, 8).unwrap(), vec![0; 8]);
    assert_eq!(db.read_data(ino, 1 << 20, 4).unwrap(), b"tail");
    assert_eq!(chunk_lengths(&path, ino), vec![4]);
    let attr = db.get_inode(ino).unwrap().unwrap();
    assert_eq!(attr.size, (1 << 20) + 4);
    assert_eq!(
        attr.blocks, 128,
        "one allocated 64 KiB chunk, not the whole 1 MiB"
    );
}

#[test]
fn truncate_cuts_the_boundary_chunk_without_padding() {
    let (_dir, path, mut db) = file_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    db.write_data(ino, 0, &vec![7u8; 100_000]).unwrap();
    let mut attr = db.get_inode(ino).unwrap().unwrap();
    attr.size = 70_000;
    db.update_inode(&attr, true).unwrap();
    assert_eq!(chunk_lengths(&path, ino), vec![65536, 4464]);
    attr.size = 200_000; // growing again exposes zeros, not the old bytes
    db.update_inode(&attr, false).unwrap();
    assert_eq!(db.read_data(ino, 69_998, 4).unwrap(), vec![7, 7, 0, 0]);
}

#[test]
fn writes_beyond_the_addressable_size_fail_with_efbig() {
    let mut db = fresh_db();
    let ino = db.add_inode_and_dentry(1, "f", &file_attr()).unwrap();
    let limit = u32::MAX as u64 * db.get_db_block_size() as u64;
    let err = db.write_data(ino, limit, b"x").unwrap_err();
    assert_eq!(err.to_errno().code(), fuser::Errno::EFBIG.code());
    assert_eq!(db.read_data(ino, limit, 1).unwrap(), vec![0]);
}
