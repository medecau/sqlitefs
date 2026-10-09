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
    db.get_data(ino, 1, 4096).unwrap();
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
