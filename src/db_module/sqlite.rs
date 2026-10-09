use crate::db_module::{DBFileAttr, DEntry, DbModule};
use crate::sqerror::{Error, Result};
use fuser::FileType;
use log::{debug, warn};
use rusqlite::types::ToSql;
use rusqlite::{params, Connection, OptionalExtension, Statement};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DB_IFIFO: u32 = 0o0_010_000;
const DB_IFCHR: u32 = 0o0_020_000;
const DB_IFDIR: u32 = 0o0_040_000;
const DB_IFBLK: u32 = 0o0_060_000;
const DB_IFREG: u32 = 0o0_100_000;
const DB_IFLNK: u32 = 0o0_120_000;
const DB_IFSOCK: u32 = 0o0_140_000;

const BLOCK_SIZE: u32 = 4096;

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

/// Release all data in "inode", after "offset" byte.
fn release_data(inode: u32, offset: u64, tx: &Connection) -> Result<()> {
    if offset == 0 {
        tx.execute("DELETE FROM data WHERE file_id=$1", params![inode])?;
    } else {
        let mut block = (offset / BLOCK_SIZE as u64) as u32;
        if !offset.is_multiple_of(BLOCK_SIZE as u64) {
            block = (offset / BLOCK_SIZE as u64) as u32 + 1;
            let sql = "SELECT data FROM data WHERE file_id=$1 and block_num = $2";
            let mut stmt = tx.prepare(sql)?;
            let mut data: Vec<u8> = match stmt.query_row(params![inode, block], |row| row.get(0)) {
                Ok(n) => n,
                Err(err) => {
                    if err == rusqlite::Error::QueryReturnedNoRows {
                        vec![0; BLOCK_SIZE as usize]
                    } else {
                        return Err(Error::from(err));
                    }
                }
            };
            data.resize((offset % BLOCK_SIZE as u64) as usize, 0);
            tx.execute(
                "REPLACE INTO data \
            (file_id, block_num, data)
            VALUES($1, $2, $3)",
                params![inode, block, data],
            )?;
        }
        tx.execute(
            "DELETE FROM data WHERE file_id=$1 and block_num > $2",
            params![inode, block],
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

fn get_inode_local(inode: u32, tx: &Connection) -> Result<Option<DBFileAttr>> {
    // Scalar subqueries always yield a count (0, never NULL), so an unlinked but
    // still-open inode reports nlink 0; dentry_child_id keeps the count indexed.
    let sql = "SELECT id, size, atime_ns, mtime_ns, ctime_ns, crtime_ns, kind, mode, \
            (SELECT count(*) FROM dentry WHERE child_id = metadata.id), \
            uid, gid, rdev, flags, \
            (SELECT count(*) FROM data WHERE file_id = metadata.id) \
            FROM metadata WHERE id=$1";
    let stmt = tx.prepare(sql)?;
    let params = params![inode];
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
        Ok(Sqlite { conn })
    }

    pub fn new_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // enable foreign key. Sqlite ignores foreign key by default.
        conn.execute("PRAGMA foreign_keys=ON", [])?;
        Ok(Sqlite { conn })
    }
}

impl DbModule for Sqlite {
    fn init(&mut self) -> Result<()> {
        let table_search_sql =
            "SELECT count(name) FROM sqlite_master WHERE type='table' AND name=$1";
        {
            let row_count: u32 =
                self.conn
                    .query_row(table_search_sql, params!["metadata"], |row| row.get(0))?;
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
        get_inode_local(inode, &self.conn)
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
        let tx = self.conn.transaction()?;
        let oldattr = get_inode_local(attr.ino, &tx)?;
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
            release_data(attr.ino, attr.size, &tx)?;
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
        let attr = match get_inode_local(inode, &tx)? {
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
        let fresh_attr = get_inode_local(inode, &self.conn)?.unwrap_or(attr);
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
            Some(entry) => get_inode_local(entry.child_ino, &self.conn),
            None => Ok(None),
        }
    }

    fn get_data(&self, inode: u32, block: u32, length: u32) -> Result<Vec<u8>> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM data WHERE file_id=$1 AND block_num=$2",
                params![inode, block],
                |row| row.get(0),
            )
            .optional()?;
        Ok(row.unwrap_or_else(|| vec![0; length as usize]))
    }

    fn write_data<B: AsRef<[u8]>>(
        &mut self,
        inode: u32,
        blocks: &[(u32, B)],
        size: u64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("REPLACE INTO data (file_id, block_num, data) VALUES($1, $2, $3)")?;
            for (block, data) in blocks {
                stmt.execute(params![inode, block, data.as_ref()])?;
            }
        }
        tx.execute(
            "UPDATE metadata SET size=max(size, $1) WHERE id=$2",
            params![size, inode],
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
        BLOCK_SIZE
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
