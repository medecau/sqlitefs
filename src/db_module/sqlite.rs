use crate::db_module::{DBFileAttr, DEntry, DbModule};
use crate::sqerror::{Error, Result};
use fuser::FileType;
use log::{debug, warn};
use rusqlite::types::ToSql;
use rusqlite::{params, Connection, DatabaseName, OptionalExtension, Statement};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DB_IFIFO: u32 = 0o0_010_000;
const DB_IFCHR: u32 = 0o0_020_000;
const DB_IFDIR: u32 = 0o0_040_000;
const DB_IFBLK: u32 = 0o0_060_000;
const DB_IFREG: u32 = 0o0_100_000;
const DB_IFLNK: u32 = 0o0_120_000;
const DB_IFSOCK: u32 = 0o0_140_000;

/// Chunk size for new databases. Databases created before the `config` table
/// existed keep 4096 (the size their chunks were written with).
const NEW_DB_BLOCK_SIZE: u32 = 65536;
const LEGACY_BLOCK_SIZE: u32 = 4096;

/// Timestamps are stored as i64 nanoseconds since the Unix epoch. Times outside
/// that range (before 1677 or after 2262) saturate instead of failing.
fn to_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

fn from_ns(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

fn now_ns() -> i64 {
    to_ns(SystemTime::now())
}

/// Databases created before integer timestamps stored text plus a separate
/// nanosecond column; convert them in place. Unparseable text becomes the epoch.
const MIGRATE_TEXT_TIMESTAMPS: &str = "
    ALTER TABLE metadata ADD COLUMN atime_ns int not null default 0;
    ALTER TABLE metadata ADD COLUMN mtime_ns int not null default 0;
    ALTER TABLE metadata ADD COLUMN ctime_ns int not null default 0;
    ALTER TABLE metadata ADD COLUMN crtime_ns int not null default 0;
    UPDATE metadata SET
        atime_ns = coalesce(unixepoch(atime), 0) * 1000000000 + coalesce(atime_nsec, 0),
        mtime_ns = coalesce(unixepoch(mtime), 0) * 1000000000 + coalesce(mtime_nsec, 0),
        ctime_ns = coalesce(unixepoch(ctime), 0) * 1000000000 + coalesce(ctime_nsec, 0),
        crtime_ns = coalesce(unixepoch(crtime), 0) * 1000000000 + coalesce(crtime_nsec, 0);
    ALTER TABLE metadata DROP COLUMN atime;
    ALTER TABLE metadata DROP COLUMN atime_nsec;
    ALTER TABLE metadata DROP COLUMN mtime;
    ALTER TABLE metadata DROP COLUMN mtime_nsec;
    ALTER TABLE metadata DROP COLUMN ctime;
    ALTER TABLE metadata DROP COLUMN ctime_nsec;
    ALTER TABLE metadata DROP COLUMN crtime;
    ALTER TABLE metadata DROP COLUMN crtime_nsec;";

fn file_type_to_const(kind: FileType) -> u32 {
    match kind {
        FileType::RegularFile => DB_IFREG,
        FileType::Socket => DB_IFSOCK,
        FileType::Directory => DB_IFDIR,
        FileType::Symlink => DB_IFLNK,
        FileType::BlockDevice => DB_IFBLK,
        FileType::CharDevice => DB_IFCHR,
        FileType::NamedPipe => DB_IFIFO,
    }
}

fn const_to_file_type(kind: u32) -> FileType {
    match kind {
        DB_IFREG => FileType::RegularFile,
        DB_IFSOCK => FileType::Socket,
        DB_IFDIR => FileType::Directory,
        DB_IFLNK => FileType::Symlink,
        DB_IFBLK => FileType::BlockDevice,
        DB_IFCHR => FileType::CharDevice,
        DB_IFIFO => FileType::NamedPipe,
        other => {
            warn!(
                "unknown file type constant {} in database, treating as regular file",
                other
            );
            FileType::RegularFile
        }
    }
}

/// Release all data in "inode" after "offset" byte: drop the chunks past it and
/// cut the boundary chunk short (never padded).
fn release_data(inode: u32, offset: u64, bs: u32, tx: &Connection) -> Result<()> {
    let bs = bs as u64;
    let last = offset.div_ceil(bs) as i64; // chunks 1..=last hold bytes below offset
    tx.execute(
        "DELETE FROM data WHERE file_id=$1 AND block_num > $2",
        params![inode, last],
    )?;
    let keep = offset % bs;
    if keep != 0 {
        tx.execute(
            "UPDATE data SET data = substr(data, 1, ?3) \
             WHERE file_id=?1 AND block_num=?2 AND length(data) > ?3",
            params![inode, last, keep as i64],
        )?;
    }
    Ok(())
}

fn update_mtime(inode: u32, ns: i64, tx: &Connection) -> Result<()> {
    tx.execute(
        "UPDATE metadata SET mtime_ns=$1 WHERE id=$2",
        params![ns, inode],
    )?;
    Ok(())
}

fn update_ctime(inode: u32, ns: i64, tx: &Connection) -> Result<()> {
    tx.execute(
        "UPDATE metadata SET ctime_ns=$1 WHERE id=$2",
        params![ns, inode],
    )?;
    Ok(())
}

fn add_dentry(entry: DEntry, tx: &Connection) -> Result<()> {
    let sql = "INSERT INTO dentry VALUES($1, $2, $3, $4)";
    tx.execute(
        sql,
        params![
            entry.parent_ino,
            entry.child_ino,
            file_type_to_const(entry.file_type),
            entry.filename
        ],
    )?;
    Ok(())
}

fn parse_attr(mut stmt: Statement, params: &[&dyn ToSql]) -> Result<Option<DBFileAttr>> {
    let rows = stmt.query_map(params, |row| {
        Ok(DBFileAttr {
            ino: row.get(0)?,
            size: row.get(1)?,
            atime: from_ns(row.get(2)?),
            mtime: from_ns(row.get(3)?),
            ctime: from_ns(row.get(4)?),
            crtime: from_ns(row.get(5)?),
            kind: const_to_file_type(row.get(6)?),
            perm: row.get(7)?,
            nlink: row.get(8)?,
            uid: row.get(9)?,
            gid: row.get(10)?,
            rdev: row.get(11)?,
            flags: row.get(12)?,
            blocks: row.get(13)?,
        })
    })?;
    let mut attrs = Vec::new();
    for row in rows {
        attrs.push(row?);
    }
    if attrs.is_empty() {
        Ok(None)
    } else {
        Ok(Some(attrs[0]))
    }
}

fn get_inode_local(inode: u32, bs: u32, tx: &Connection) -> Result<Option<DBFileAttr>> {
    // Scalar subqueries always yield a count (0, never NULL), so an unlinked but
    // still-open inode reports nlink 0; dentry_child_id keeps the count indexed.
    // st_blocks (512-byte units): stored chunks, capped by the size rounded up, so
    // a short tail chunk is not counted as a whole chunk. ?N, not $N: SQLite
    // numbers $N parameters by first appearance, and ?2 comes before ?1 here.
    let sql = "SELECT id, size, atime_ns, mtime_ns, ctime_ns, crtime_ns, kind, mode, \
            (SELECT count(*) FROM dentry WHERE child_id = metadata.id), \
            uid, gid, rdev, flags, \
            min((SELECT count(*) FROM data WHERE file_id = metadata.id) * ?2, \
                (size + 511) / 512 * 512) / 512 \
            FROM metadata WHERE id=?1";
    let stmt = tx.prepare(sql)?;
    let params = params![inode, bs];
    parse_attr(stmt, params)
}

fn get_dentry_single(parent: u32, name: &str, tx: &Connection) -> Result<Option<DEntry>> {
    let sql = "SELECT child_id, file_type FROM dentry WHERE  parent_id=$1 and name=$2";
    let mut stmt = tx.prepare(sql)?;
    let res: Option<DEntry> = match stmt.query_row(params![parent, name], |row| {
        Ok(Some(DEntry {
            parent_ino: parent,
            child_ino: row.get(0)?,
            file_type: const_to_file_type(row.get(1)?),
            filename: name.to_string(),
        }))
    }) {
        Ok(n) => n,
        Err(err) => {
            if err == rusqlite::Error::QueryReturnedNoRows {
                None
            } else {
                return Err(Error::from(err));
            }
        }
    };
    Ok(res)
}

fn delete_dentry_local(parent: u32, name: &str, tx: &Connection) -> Result<()> {
    let sql = "DELETE FROM dentry WHERE parent_id=$1 and name=$2";
    tx.execute(sql, params![parent, name])?;
    Ok(())
}

fn delete_sub_dentry(id: u32, tx: &Connection) -> Result<()> {
    let sql = "DELETE FROM dentry WHERE parent_id=$1";
    tx.execute(sql, params![id])?;
    Ok(())
}

fn check_directory_is_empty_local(inode: u32, tx: &Connection) -> Result<bool> {
    let sql = "SELECT name FROM dentry where parent_id=$1";
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map(params![inode], |row| {
        Ok({
            let name: String = row.get(0)?;
            name
        })
    })?;
    for row in rows {
        let name = row?;
        if &name != "." && &name != ".." {
            return Ok(false);
        }
    }
    Ok(true)
}

fn add_inode_local(attr: &DBFileAttr, tx: &Connection) -> Result<u32> {
    let sql = "INSERT INTO metadata \
            (size, atime_ns, mtime_ns, ctime_ns, crtime_ns, kind, mode, nlink, uid, gid, rdev, flags) \
            VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)";
    {
        tx.execute(
            sql,
            params![
                attr.size,
                to_ns(attr.atime),
                to_ns(attr.mtime),
                to_ns(attr.ctime),
                to_ns(attr.crtime),
                file_type_to_const(attr.kind),
                attr.perm,
                0,
                attr.uid,
                attr.gid,
                attr.rdev,
                attr.flags,
            ],
        )?;
    }
    let sql = "SELECT last_insert_rowid()";
    let child: u32;
    {
        let mut stmt = tx.prepare(sql)?;
        child = stmt.query_row(params![], |row| row.get(0))?;
    }
    Ok(child)
}

pub struct Sqlite {
    conn: Connection,
    /// Chunk size of the `data` table; read from `config` by init().
    block_size: u32,
}

impl Sqlite {
    pub fn new(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // enable foreign key. Sqlite ignores foreign key by default.
        // WAL + synchronous=NORMAL: commits skip the fsync, which is deferred to
        // checkpoints; fsync()/fsyncdir() force one via DbModule::checkpoint.
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
        )?;
        Ok(Sqlite {
            conn,
            block_size: NEW_DB_BLOCK_SIZE,
        })
    }

    pub fn new_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // enable foreign key. Sqlite ignores foreign key by default.
        conn.execute("PRAGMA foreign_keys=ON", [])?;
        Ok(Sqlite {
            conn,
            block_size: NEW_DB_BLOCK_SIZE,
        })
    }
}

impl Sqlite {
    /// Largest file size whose last chunk number still fits block_num (u32).
    fn max_file_size(&self) -> u64 {
        u32::MAX as u64 * self.block_size as u64
    }
}

impl DbModule for Sqlite {
    fn init(&mut self) -> Result<()> {
        let table_search_sql =
            "SELECT count(name) FROM sqlite_master WHERE type='table' AND name=$1";
        let fresh: bool;
        {
            let row_count: u32 =
                self.conn
                    .query_row(table_search_sql, params!["metadata"], |row| row.get(0))?;
            fresh = row_count == 0;
            if row_count == 0 {
                let sql = "CREATE TABLE metadata(\
                    id integer primary key autoincrement,\
                    size int default 0 not null,\
                    atime_ns int not null default 0,\
                    mtime_ns int not null default 0,\
                    ctime_ns int not null default 0,\
                    crtime_ns int not null default 0,\
                    kind int,\
                    mode int,\
                    nlink int default 0 not null,\
                    uid int default 0,\
                    gid int default 0,\
                    rdev int default 0,\
                    flags int default 0 \
                    )";
                let res = self.conn.execute(sql, params![])?;
                debug!("metadata table: {}", res);
            }
            let legacy: u32 = self.conn.query_row(
                "SELECT count(*) FROM pragma_table_info('metadata') WHERE name = 'atime_nsec'",
                [],
                |row| row.get(0),
            )?;
            if legacy > 0 {
                let tx = self.conn.transaction()?;
                tx.execute_batch(MIGRATE_TEXT_TIMESTAMPS)?;
                tx.commit()?;
            }
        }
        {
            let row_count: u32 =
                self.conn
                    .query_row(table_search_sql, params!["dentry"], |row| row.get(0))?;
            if row_count == 0 {
                let sql = "CREATE TABLE dentry(\
                    parent_id int,\
                    child_id int,\
                    file_type int,\
                    name text,\
                    foreign key (parent_id) references metadata(id) on delete cascade,\
                    foreign key (child_id) references metadata(id) on delete cascade,\
                    primary key (parent_id, name) \
                    )";
                self.conn.execute(sql, params![])?;
            }
            // nlink is COUNT(*) over child_id; also serves the FK cascade check.
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS dentry_child_id ON dentry(child_id)",
                [],
            )?;
        }
        {
            let row_count: u32 = self
                .conn
                .query_row(table_search_sql, params!["data"], |row| row.get(0))?;
            if row_count == 0 {
                let sql = "CREATE TABLE data(\
                    file_id int,\
                    block_num int,\
                    data blob,\
                    foreign key (file_id) references metadata(id) on delete cascade,\
                    primary key (file_id, block_num) \
                    )";
                self.conn.execute(sql, params![])?;
            }
        }
        {
            let row_count: u32 =
                self.conn
                    .query_row(table_search_sql, params!["xattr"], |row| row.get(0))?;
            if row_count == 0 {
                let sql = "CREATE TABLE xattr(\
                    file_id int,\
                    name text,\
                    value text,\
                    foreign key (file_id) references metadata(id) on delete cascade,\
                    primary key (file_id, name) \
                    )";
                self.conn.execute(sql, params![])?;
            }
        }
        {
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS config(name text primary key, value)",
                [],
            )?;
            let default = if fresh {
                NEW_DB_BLOCK_SIZE
            } else {
                LEGACY_BLOCK_SIZE
            };
            self.conn.execute(
                "INSERT OR IGNORE INTO config VALUES('block_size', $1)",
                params![default],
            )?;
            let bs: i64 = self.conn.query_row(
                "SELECT value FROM config WHERE name='block_size'",
                [],
                |row| row.get(0),
            )?;
            // A crafted database must not make chunk arithmetic divide by zero.
            self.block_size = match u32::try_from(bs) {
                Ok(bs) if (512..=1 << 24).contains(&bs) => bs,
                _ => {
                    return Err(Error::FsParm {
                        description: format!("invalid block_size {} in config", bs),
                    })
                }
            };
        }
        {
            let sql = "SELECT count(id) FROM metadata WHERE id=1";
            let row_count: u32 = self.conn.query_row(sql, params![], |row| row.get(0))?;
            if row_count == 0 {
                let now = SystemTime::now();
                let root_dir = DBFileAttr {
                    ino: 1,
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::Directory,
                    perm: 0o40777,
                    nlink: 0,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    flags: 0,
                };
                add_inode_local(&root_dir, &self.conn)?;
            }
        }
        {
            let sql = "SELECT count(parent_id) FROM dentry WHERE parent_id=1 and name='.'";
            let row_count: u32 = self.conn.query_row(sql, params![], |row| row.get(0))?;
            if row_count == 0 {
                let root_dir = DEntry {
                    parent_ino: 1,
                    child_ino: 1,
                    file_type: FileType::Directory,
                    filename: ".".to_string(),
                };
                add_dentry(root_dir, &self.conn)?;
            }
        }
        {
            let sql = "SELECT count(parent_id) FROM dentry WHERE parent_id=1 and name='..'";
            let row_count: u32 = self.conn.query_row(sql, params![], |row| row.get(0))?;
            if row_count == 0 {
                let root_dir = DEntry {
                    parent_ino: 1,
                    child_ino: 1,
                    file_type: FileType::Directory,
                    filename: "..".to_string(),
                };
                add_dentry(root_dir, &self.conn)?;
            }
        }
        Ok(())
    }

    fn get_inode(&self, inode: u32) -> Result<Option<DBFileAttr>> {
        get_inode_local(inode, self.block_size, &self.conn)
    }

    fn add_inode_and_dentry(&mut self, parent: u32, name: &str, attr: &DBFileAttr) -> Result<u32> {
        let tx = self.conn.transaction()?;
        let child = add_inode_local(attr, &tx)?;
        let dentry = DEntry {
            parent_ino: parent,
            child_ino: child,
            filename: String::from(name),
            file_type: attr.kind,
        };
        add_dentry(dentry, &tx)?;
        if attr.kind == FileType::Directory {
            let dentry = DEntry {
                parent_ino: child,
                child_ino: parent,
                filename: String::from(".."),
                file_type: attr.kind,
            };
            add_dentry(dentry, &tx)?;
            let dentry = DEntry {
                parent_ino: child,
                child_ino: child,
                filename: String::from("."),
                file_type: attr.kind,
            };
            add_dentry(dentry, &tx)?;
        }
        let now = now_ns();
        update_mtime(parent, now, &tx)?;
        update_ctime(parent, now, &tx)?;
        tx.commit()?;
        Ok(child)
    }

    fn update_inode(&mut self, attr: &DBFileAttr, truncate: bool) -> Result<()> {
        let sql = "UPDATE metadata SET \
            size=$1,\
            atime_ns=$2,\
            mtime_ns=$3,\
            ctime_ns=$4,\
            crtime_ns=$5,\
            mode=$6,\
            uid=$7,\
            gid=$8,\
            rdev=$9,\
            flags=$10 \
             WHERE id=$11";
        if truncate && attr.size > self.max_file_size() {
            return Err(Error::FsFileTooBig {
                description: format!("{} exceeds {}", attr.size, self.max_file_size()),
            });
        }
        let tx = self.conn.transaction()?;
        let oldattr = get_inode_local(attr.ino, self.block_size, &tx)?;
        let oldattr = match oldattr {
            Some(n) => n,
            None => {
                return Err(Error::FsNoEnt {
                    description: format!("{} is not exist", attr.ino),
                });
            }
        };
        let now = now_ns();
        let mtime = if oldattr.size != attr.size {
            now
        } else {
            to_ns(attr.mtime)
        };
        {
            let mut stmt = tx.prepare(sql)?;
            stmt.execute(params![
                attr.size,
                to_ns(attr.atime),
                mtime,
                now,
                to_ns(attr.crtime),
                attr.perm,
                attr.uid,
                attr.gid,
                attr.rdev,
                attr.flags,
                attr.ino
            ])?;
        }
        if truncate {
            release_data(attr.ino, attr.size, self.block_size, &tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn delete_inode_if_noref(&mut self, inode: u32) -> Result<()> {
        let sql = "SELECT count(child_id) FROM dentry WHERE child_id=$1";
        let tx = self.conn.transaction()?;
        let nlink: u32;
        {
            let mut stmt = tx.prepare(sql)?;
            nlink = stmt.query_row(params![inode], |row| row.get(0))?;
        }
        if nlink == 0 {
            let sql = "DELETE FROM metadata WHERE id=$1";
            tx.execute(sql, params![inode])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn get_dentry(&self, inode: u32) -> Result<Vec<DEntry>> {
        let sql = "SELECT child_id, file_type, name FROM dentry WHERE parent_id=$1 ORDER BY name";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![inode], |row| {
            Ok(DEntry {
                parent_ino: inode,
                child_ino: row.get(0)?,
                file_type: const_to_file_type(row.get(1)?),
                filename: row.get(2)?,
            })
        })?;
        let mut entries: Vec<DEntry> = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    fn link_dentry(&mut self, inode: u32, parent: u32, name: &str) -> Result<DBFileAttr> {
        let now = now_ns();
        let tx = self.conn.transaction()?;
        let attr = match get_inode_local(inode, self.block_size, &tx)? {
            Some(n) => n,
            None => {
                return Err(Error::FsNoEnt {
                    description: format!("old path {} is not exist", inode),
                });
            }
        };
        if attr.kind == FileType::Directory {
            return Err(Error::FsParm {
                description: format!("hardlink to directory {} is not permitted", inode),
            });
        };
        let new_inode = get_dentry_single(parent, name, &tx)?;
        if new_inode.is_some() {
            return Err(Error::FsFileExist {
                description: format!("new path {}/{} exist", parent, name),
            });
        }
        let entry = DEntry {
            parent_ino: parent,
            child_ino: inode,
            file_type: attr.kind,
            filename: name.to_string(),
        };
        add_dentry(entry, &tx)?;
        update_mtime(inode, now, &tx)?;
        update_mtime(parent, now, &tx)?;
        update_ctime(parent, now, &tx)?;
        tx.commit()?;
        // Re-fetch after commit so nlink reflects the new dentry count.
        // get_inode_local computes nlink via COUNT(child_id) in dentry, so it
        // must run after the transaction that adds the hard-link dentry is committed.
        let fresh_attr = get_inode_local(inode, self.block_size, &self.conn)?.unwrap_or(attr);
        Ok(fresh_attr)
    }

    fn delete_dentry(&mut self, parent: u32, name: &str) -> Result<u32> {
        let sql = "SELECT child_id FROM dentry WHERE parent_id=$1 and name=$2";
        let now = now_ns();
        let tx = self.conn.transaction()?;
        let child: u32;
        {
            let mut stmt = tx.prepare(sql)?;
            child = stmt.query_row(params![parent, name], |row| row.get(0))?;
        }
        delete_dentry_local(parent, name, &tx)?;
        delete_sub_dentry(child, &tx)?;
        update_ctime(child, now, &tx)?;
        update_mtime(parent, now, &tx)?;
        update_ctime(parent, now, &tx)?;
        tx.commit()?;
        Ok(child)
    }

    fn move_dentry(
        &mut self,
        parent: u32,
        name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<Option<u32>> {
        let sql = "UPDATE dentry SET parent_id=$1, name=$2 where parent_id=$3 and name=$4";
        let now = now_ns();
        let tx = self.conn.transaction()?;
        let dentry = match get_dentry_single(parent, name, &tx)? {
            Some(n) => n,
            None => {
                return Err(Error::FsNoEnt {
                    description: format!("parent: {} name:{}", parent, name),
                });
            }
        };
        let mut res = None;
        let exist_entry = get_dentry_single(new_parent, new_name, &tx)?;
        if let Some(v) = exist_entry {
            let exist_id = v.child_ino;
            let exist_file_type = v.file_type;
            let src_is_dir = dentry.file_type == FileType::Directory;
            let dst_is_dir = exist_file_type == FileType::Directory;
            if src_is_dir && !dst_is_dir {
                return Err(Error::FsIsNotDir {
                    description: format!("parent: {} name:{}", new_parent, new_name),
                });
            }
            if !src_is_dir && dst_is_dir {
                return Err(Error::FsIsDir {
                    description: format!("parent: {} name:{}", new_parent, new_name),
                });
            }
            if exist_file_type == FileType::Directory {
                let empty = check_directory_is_empty_local(exist_id, &tx)?;
                if !empty {
                    return Err(Error::FsNotEmpty {
                        description: format!(
                            "parent: {} name:{} is not empty",
                            new_parent, new_name
                        ),
                    });
                }
                delete_sub_dentry(exist_id, &tx)?;
            }
            delete_dentry_local(new_parent, new_name, &tx)?;
            res = Some(v.child_ino);
        }
        tx.execute(sql, params![new_parent, new_name, parent, name])?;
        if parent != new_parent && dentry.file_type == FileType::Directory {
            let sql = "UPDATE dentry set child_id=$1 WHERE parent_id=$2 and name='..'";
            tx.execute(sql, params![new_parent, dentry.child_ino])?;
        }
        update_ctime(dentry.child_ino, now, &tx)?;
        update_mtime(parent, now, &tx)?;
        update_ctime(parent, now, &tx)?;
        if parent != new_parent {
            update_mtime(new_parent, now, &tx)?;
            update_ctime(new_parent, now, &tx)?;
        }
        tx.commit()?;
        Ok(res)
    }

    fn check_directory_is_empty(&self, inode: u32) -> Result<bool> {
        check_directory_is_empty_local(inode, &self.conn)
    }

    fn lookup(&self, parent: u32, name: &str) -> Result<Option<DBFileAttr>> {
        match get_dentry_single(parent, name, &self.conn)? {
            Some(entry) => get_inode_local(entry.child_ino, self.block_size, &self.conn),
            None => Ok(None),
        }
    }

    fn read_data(&self, inode: u32, offset: u64, size: u32) -> Result<Vec<u8>> {
        let bs = self.block_size as u64;
        let end = offset + size as u64;
        let mut out = vec![0; size as usize];
        if size == 0 {
            return Ok(out);
        }
        // Per stored chunk: where its bytes land in `out`, and the bytes themselves
        // (substr past a short chunk's end yields fewer or none). Holes stay zero.
        // ponytail: SQLite loads each whole chunk before substr(); blob read_at if
        // small random reads become hot.
        let mut stmt = self.conn.prepare_cached(
            "SELECT max((block_num - 1) * ?4 - ?2, 0), \
                    substr(data, max(?2 - (block_num - 1) * ?4, 0) + 1, \
                           ?3 - max(?2, (block_num - 1) * ?4)) \
             FROM data WHERE file_id = ?1 AND block_num BETWEEN ?5 AND ?6",
        )?;
        let mut rows = stmt.query(params![
            inode,
            offset as i64,
            end as i64,
            bs as i64,
            (offset / bs + 1) as i64,
            ((end - 1) / bs + 1) as i64,
        ])?;
        while let Some(row) = rows.next()? {
            let pos = row.get::<_, i64>(0)? as usize;
            let bytes = row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?;
            out[pos..pos + bytes.len()].copy_from_slice(bytes);
        }
        Ok(out)
    }

    fn write_data(&mut self, inode: u32, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let bs = self.block_size as u64;
        let end = offset + data.len() as u64;
        if end > self.max_file_size() {
            return Err(Error::FsFileTooBig {
                description: format!("write to {} exceeds {}", end, self.max_file_size()),
            });
        }
        let tx = self.conn.transaction()?;
        for block in (offset / bs + 1)..=((end - 1) / bs + 1) {
            let chunk_start = (block - 1) * bs;
            let s = offset.max(chunk_start) - chunk_start;
            let e = end.min(chunk_start + bs) - chunk_start;
            let src =
                &data[(chunk_start + s - offset) as usize..(chunk_start + e - offset) as usize];
            let stored: Option<(i64, u64)> = tx
                .query_row(
                    "SELECT rowid, length(data) FROM data WHERE file_id=$1 AND block_num=$2",
                    params![inode, block as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match stored {
                // Inside the stored bytes: patch them without rewriting the chunk.
                Some((rowid, len)) if e <= len => {
                    let mut blob =
                        tx.blob_open(DatabaseName::Main, "data", "data", rowid, false)?;
                    blob.write_at(src, s as usize)?;
                }
                // New or growing chunk: SQLite builds head || zero gap || src. `||`
                // yields TEXT, so CAST back to BLOB (substr/length count bytes).
                _ => {
                    tx.execute(
                        "INSERT INTO data (file_id, block_num, data) \
                         VALUES (?1, ?2, CAST(zeroblob(?3) || ?4 AS BLOB)) \
                         ON CONFLICT (file_id, block_num) DO UPDATE SET data = CAST( \
                             substr(data, 1, ?3) || zeroblob(max(?3 - length(data), 0)) || ?4 \
                             AS BLOB)",
                        params![inode, block as i64, s as i64, src],
                    )?;
                }
            }
        }
        tx.execute(
            "UPDATE metadata SET size=max(size, $1) WHERE id=$2",
            params![end, inode],
        )?;
        let time = now_ns();
        update_mtime(inode, time, &tx)?;
        update_ctime(inode, time, &tx)?;
        tx.commit()?;
        Ok(())
    }

    fn checkpoint(&self) -> Result<()> {
        // Returns one row (busy, log frames, checkpointed frames); only success matters.
        self.conn
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()))?;
        Ok(())
    }

    fn unmount(&self) -> Result<()> {
        // Returns the resulting mode; it stays "wal" if the switch was refused
        // (e.g. another connection has the file open). In-memory DBs report "memory".
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        if mode == "wal" {
            return Err(Error::SqliteError {
                description: "could not leave WAL mode; -wal/-shm files remain".to_string(),
            });
        }
        Ok(())
    }

    fn release_data(&self, inode: u32) -> Result<()> {
        self.conn
            .execute("DELETE FROM data WHERE file_id=$1", params![inode])?;
        Ok(())
    }

    fn delete_all_noref_inode(&mut self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM metadata WHERE NOT EXISTS (SELECT 'x' FROM dentry WHERE metadata.id = dentry.child_id)",
            params![]
        )?;
        Ok(())
    }

    fn get_db_block_size(&self) -> u32 {
        self.block_size
    }

    fn set_xattr(&mut self, inode: u32, key: &str, value: &[u8]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            tx.execute(
                "REPLACE INTO xattr \
            (file_id, name, value)
            VALUES($1, $2, $3)",
                params![inode, key, value],
            )?;
        }
        let time = now_ns();
        update_ctime(inode, time, &tx)?;
        tx.commit()?;
        Ok(())
    }

    fn get_xattr(&self, inode: u32, key: &str) -> Result<Vec<u8>> {
        let mut stmt = self.conn.prepare(
            "SELECT \
            value FROM xattr WHERE file_id=$1 AND name=$2",
        )?;
        let row: Vec<u8> = match stmt.query_row(params![inode, key], |row| row.get(0)) {
            Ok(n) => n,
            Err(err) => {
                if err == rusqlite::Error::QueryReturnedNoRows {
                    return Err(Error::FsNoEnt {
                        description: format!("inode: {} name:{}", inode, key),
                    });
                } else {
                    return Err(Error::from(err));
                }
            }
        };
        Ok(row)
    }

    fn list_xattr(&self, inode: u32) -> Result<Vec<String>> {
        let sql = "SELECT name FROM xattr WHERE file_id=$1 ORDER BY name";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![inode], |row| row.get(0))?;
        let mut name_list: Vec<String> = Vec::new();
        for row in rows {
            name_list.push(row?);
        }
        Ok(name_list)
    }

    fn delete_xattr(&mut self, inode: u32, key: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let rows_deleted = tx.execute(
                "DELETE FROM xattr \
            WHERE file_id = $1 AND name = $2",
                params![inode, key],
            )?;
            if rows_deleted == 0 {
                return Err(Error::FsNoEnt {
                    description: format!("xattr inode:{} name:{}", inode, key),
                });
            }
        }
        let time = now_ns();
        update_ctime(inode, time, &tx)?;
        tx.commit()?;
        Ok(())
    }
}
