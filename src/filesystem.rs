use fuser::{
    BsdFileFlags, FileType, Filesystem, FopenFlags, KernelConfig, OpenAccMode, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyLock, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use fuser::{Errno, FileHandle, Generation, INodeNo, LockOwner};
use libc::{O_APPEND, XATTR_CREATE, XATTR_REPLACE};

#[cfg(not(target_os = "macos"))]
use libc::O_NOATIME;

// POSIX permission bits, typed as u16 to match DBFileAttr.perm directly.
// libc::S_ISGID/S_ISVTX are mode_t, which is u16 on macOS and u32 on Linux;
// defining them here avoids cross-platform cast warnings.
const S_ISGID: u16 = 0o2000;
const S_ISVTX: u16 = 0o1000;

// POSIX advisory-lock type constants, typed as i32 to match the lock-type fields
// throughout this module. libc::F_RDLCK/F_WRLCK/F_UNLCK are i16 on macOS and
// i32 on Linux; the allow suppresses the redundant-cast warning on Linux only.
#[allow(clippy::unnecessary_cast)]
const F_RDLCK: i32 = libc::F_RDLCK as i32;
#[allow(clippy::unnecessary_cast)]
const F_WRLCK: i32 = libc::F_WRLCK as i32;
#[allow(clippy::unnecessary_cast)]
const F_UNLCK: i32 = libc::F_UNLCK as i32;

use crate::db_module::sqlite::Sqlite;
use crate::db_module::{DBFileAttr, DEntry, DbModule};
use crate::sqerror::Error;
use log::{debug, warn};
use nix::sys::statvfs;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const ONE_SEC: Duration = Duration::from_secs(1);

#[allow(dead_code)]
struct OpenFileStat {
    readonly: bool,
    append: bool,
    noatime: bool,
}

struct OpenFileHandler {
    next_fh: u64,
    list: HashMap<u64, OpenFileStat>,
}

impl OpenFileStat {
    fn new() -> Self {
        Self {
            readonly: false,
            append: false,
            noatime: false,
        }
    }
}

impl OpenFileHandler {
    fn new() -> Self {
        Self {
            next_fh: 0,
            list: HashMap::<u64, OpenFileStat>::new(),
        }
    }
}

struct OpenDirHandler {
    next_fh: u64,
    list: HashMap<u64, Vec<DEntry>>,
}

impl OpenDirHandler {
    fn new() -> Self {
        Self {
            next_fh: 0,
            list: HashMap::<u64, Vec<DEntry>>::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct PosixLock {
    owner: u64, // LockOwner.0
    pid: u32,   // returned in getlk replies
    start: u64, // inclusive start of byte range
    end: u64,   // inclusive end; u64::MAX = to EOF
    typ: i32,   // F_RDLCK or F_WRLCK
}

pub struct SqliteFs {
    db: Mutex<Sqlite>,
    lookup_count: Arc<Mutex<HashMap<u32, u64>>>,
    open_file_handler: Arc<Mutex<HashMap<u32, OpenFileHandler>>>,
    open_dir_handler: Arc<Mutex<HashMap<u32, OpenDirHandler>>>,
    locks: Arc<Mutex<HashMap<u32, Vec<PosixLock>>>>,
}

impl SqliteFs {
    pub fn new(path: &str) -> Result<SqliteFs, Error> {
        let mut db = Sqlite::new(Path::new(path))?;
        db.init()?;
        let lookup_count = Arc::new(Mutex::new(HashMap::<u32, u64>::new()));
        let open_file_handler = Arc::new(Mutex::new(HashMap::<u32, OpenFileHandler>::new()));
        let open_dir_handler = Arc::new(Mutex::new(HashMap::<u32, OpenDirHandler>::new()));
        let locks = Arc::new(Mutex::new(HashMap::<u32, Vec<PosixLock>>::new()));
        Ok(SqliteFs {
            db: Mutex::new(db),
            lookup_count,
            open_file_handler,
            open_dir_handler,
            locks,
        })
    }

    pub fn new_with_db(db: Sqlite) -> Result<SqliteFs, Error> {
        let lookup_count = Arc::new(Mutex::new(HashMap::<u32, u64>::new()));
        let open_file_handler = Arc::new(Mutex::new(HashMap::<u32, OpenFileHandler>::new()));
        let open_dir_handler = Arc::new(Mutex::new(HashMap::<u32, OpenDirHandler>::new()));
        let locks = Arc::new(Mutex::new(HashMap::<u32, Vec<PosixLock>>::new()));
        Ok(SqliteFs {
            db: Mutex::new(db),
            lookup_count,
            open_file_handler,
            open_dir_handler,
            locks,
        })
    }
}

fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start <= b_end && b_start <= a_end
}

fn find_conflicting_lock(
    locks: &[PosixLock],
    owner: u64,
    start: u64,
    end: u64,
    typ: i32,
) -> Option<&PosixLock> {
    if typ == F_UNLCK {
        return None;
    }
    for lock in locks {
        if lock.owner == owner {
            continue;
        }
        if !ranges_overlap(lock.start, lock.end, start, end) {
            continue;
        }
        if typ == F_RDLCK && lock.typ == F_RDLCK {
            continue; // shared reads never conflict
        }
        return Some(lock);
    }
    None
}

fn apply_lock(locks: &mut Vec<PosixLock>, owner: u64, pid: u32, start: u64, end: u64, typ: i32) {
    if start > end {
        return;
    }

    // Step 1: Remove or trim the [start, end] range from all owned locks.
    let old = std::mem::take(locks);
    for lock in old {
        if lock.owner != owner || !ranges_overlap(lock.start, lock.end, start, end) {
            locks.push(lock);
        } else {
            if lock.start < start {
                locks.push(PosixLock {
                    end: start - 1,
                    ..lock.clone()
                });
            }
            if lock.end > end {
                locks.push(PosixLock {
                    start: end + 1,
                    ..lock
                });
            }
        }
    }

    if typ == F_UNLCK {
        return;
    }

    // Step 2: Insert and coalesce adjacent/overlapping owned locks of the same type.
    locks.push(PosixLock {
        owner,
        pid,
        start,
        end,
        typ,
    });

    let (mut same, other): (Vec<_>, Vec<_>) = std::mem::take(locks)
        .into_iter()
        .partition(|l| l.owner == owner && l.typ == typ);
    same.sort_by_key(|l| l.start);

    let mut merged: Vec<PosixLock> = Vec::new();
    for lock in same {
        if let Some(last) = merged.last_mut() {
            let adjacent = last.end == u64::MAX || last.end + 1 >= lock.start;
            if adjacent || ranges_overlap(last.start, last.end, lock.start, lock.end) {
                last.end = last.end.max(lock.end);
                continue;
            }
        }
        merged.push(lock);
    }
    locks.extend(merged);
    locks.extend(other);
}

fn remove_locks_for_owner(locks: &mut Vec<PosixLock>, owner: u64) {
    locks.retain(|l| l.owner != owner);
}

impl Filesystem for SqliteFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        let mut db = self.db.lock().unwrap();
        match db.delete_all_noref_inode() {
            Ok(n) => n,
            Err(err) => warn!("init: delete_all_noref_inode: {}", err),
        };
        Ok(())
    }

    fn destroy(&mut self) {
        let lc_list = self.lookup_count.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        for key in lc_list.keys() {
            match db.delete_inode_if_noref(*key) {
                Ok(n) => n,
                Err(err) => warn!("destroy: delete_inode_if_noref({}): {}", key, err),
            }
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = parent.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let db = self.db.lock().unwrap();
        let child = match db.lookup(parent, name) {
            Ok(n) => match n {
                Some(v) => {
                    reply.entry(&ONE_SEC, &v.get_file_attr(), Generation(0));
                    debug!("filesystem:lookup, return:{:?}", v.get_file_attr());
                    v.ino
                }
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(err) => {
                warn!("lookup: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        drop(db);
        let mut lc_list = self.lookup_count.lock().unwrap();
        let lc = lc_list.entry(child).or_insert(0);
        *lc += 1;
        debug!("filesystem:lookup, lookup count:{:?}", *lc);
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let ino = ino.0 as u32;
        let mut lc_list = self.lookup_count.lock().unwrap();
        let lc = lc_list.entry(ino).or_insert(0);
        *lc = lc.saturating_sub(nlookup);
        debug!("filesystem:forget, lookup count:{:?}", *lc);
        if *lc == 0 {
            lc_list.remove(&ino);
            drop(lc_list);
            let mut db = self.db.lock().unwrap();
            match db.delete_inode_if_noref(ino) {
                Ok(n) => n,
                Err(err) => warn!("forget: delete_inode_if_noref({}): {}", ino, err),
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let db = self.db.lock().unwrap();
        match db.get_inode(ino.0 as u32) {
            Ok(n) => match n {
                Some(v) => {
                    reply.attr(&ONE_SEC, &v.get_file_attr());
                    debug!("filesystem:getattr, return:{:?}", v.get_file_attr());
                }
                None => reply.error(Errno::ENOENT),
            },
            Err(err) => {
                warn!("getattr: {}", err);
                reply.error(err.to_errno());
            }
        };
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut db = self.db.lock().unwrap();
        let mut attr = match db.get_inode(ino.0 as u32) {
            Ok(n) => match n {
                Some(v) => v,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(err) => {
                warn!("setattr get_inode: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        let old_size = attr.size;
        if let Some(n) = mode {
            attr.perm = n as u16
        };
        if let Some(n) = uid {
            attr.uid = n
        };
        if let Some(n) = gid {
            attr.gid = n
        };
        if let Some(n) = size {
            attr.size = n
        };
        if let Some(n) = atime {
            attr.atime = match n {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => SystemTime::now(),
            }
        };
        if let Some(n) = mtime {
            attr.mtime = match n {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => SystemTime::now(),
            }
        };
        if let Some(n) = crtime {
            attr.crtime = n
        };
        match db.update_inode(&attr, old_size > attr.size) {
            Ok(_n) => (),
            Err(err) => {
                warn!("setattr update_inode: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        reply.attr(&ONE_SEC, &attr.get_file_attr());
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let ino = ino.0 as u32;
        let db = self.db.lock().unwrap();
        let attr = match db.get_inode(ino) {
            Ok(n) => match n {
                Some(attr) => attr,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(err) => {
                warn!("readlink get_inode: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };

        if attr.kind != FileType::Symlink {
            reply.error(Errno::EINVAL);
            return;
        }
        let size = attr.size;
        match db.read_data(ino, 0, size as u32) {
            Ok(data) => reply.data(&data),
            Err(err) => {
                warn!("readlink read_data: {}", err);
                reply.error(err.to_errno());
            }
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let now = SystemTime::now();
        let parent = parent.0 as u32;
        let mut attr = DBFileAttr {
            ino: 0,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: mode as u16,
            nlink: 0,
            uid: req.uid(),
            gid: req.gid(),
            rdev: 0,
            flags: 0,
        };
        let mut db = self.db.lock().unwrap();
        let parent_attr = match db.get_inode(parent) {
            Ok(n) => match n {
                Some(n) => n,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(err) => {
                warn!("mkdir get_inode: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        if parent_attr.perm & S_ISGID > 0 {
            attr.perm |= S_ISGID;
            attr.gid = parent_attr.gid;
        }
        if parent_attr.perm & S_ISVTX > 0 {
            attr.perm |= S_ISVTX;
        }
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let ino = match db.add_inode_and_dentry(parent, name, &attr) {
            Ok(n) => n,
            Err(err) => {
                warn!("mkdir add_inode_and_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        attr.ino = ino;
        reply.entry(&ONE_SEC, &attr.get_file_attr(), Generation(0));
        drop(db);
        let mut lc_list = self.lookup_count.lock().unwrap();
        let lc = lc_list.entry(ino).or_insert(0);
        *lc += 1;
        debug!("filesystem:mkdir, inode: {:?} lookup count:{:?}", ino, *lc);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut db = self.db.lock().unwrap();
        let ino = match db.delete_dentry(parent.0 as u32, name) {
            Ok(n) => n,
            Err(err) => {
                warn!("unlink delete_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        let lc_list = self.lookup_count.lock().unwrap();
        if !lc_list.contains_key(&ino) {
            drop(lc_list);
            match db.delete_inode_if_noref(ino) {
                Ok(n) => n,
                Err(err) => {
                    warn!("unlink delete_inode_if_noref: {}", err);
                    reply.error(err.to_errno());
                    return;
                }
            };
        }
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = parent.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut db = self.db.lock().unwrap();
        let attr = match db.lookup(parent, name) {
            Ok(n) => match n {
                Some(v) => v,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(err) => {
                warn!("rmdir lookup: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        let empty = match db.check_directory_is_empty(attr.ino) {
            Ok(n) => n,
            Err(err) => {
                warn!("rmdir check_directory_is_empty: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        if !empty {
            reply.error(Errno::ENOTEMPTY);
            return;
        }
        let ino = match db.delete_dentry(parent, name) {
            Ok(n) => n,
            Err(err) => {
                warn!("rmdir delete_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        let lc_list = self.lookup_count.lock().unwrap();
        if !lc_list.contains_key(&ino) {
            drop(lc_list);
            match db.delete_inode_if_noref(ino) {
                Ok(n) => n,
                Err(err) => {
                    warn!("rmdir delete_inode_if_noref: {}", err);
                    reply.error(err.to_errno());
                    return;
                }
            };
        }
        reply.ok();
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let now = SystemTime::now();
        let mut attr = DBFileAttr {
            ino: 0,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Symlink,
            perm: 0o777, // never used
            nlink: 0,
            uid: req.uid(),
            gid: req.gid(),
            rdev: 0,
            flags: 0,
        };
        let link_name = match link_name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let target_str = match target.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let data = target_str.as_bytes();
        if data.len() > 4096 {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }
        let mut db = self.db.lock().unwrap();
        let ino = match db.add_inode_and_dentry(parent.0 as u32, link_name, &attr) {
            Ok(n) => n,
            Err(err) => {
                warn!("symlink add_inode_and_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        match db.write_data(ino, 0, data) {
            Ok(n) => n,
            Err(err) => {
                warn!("symlink write_data: {}", err);
                reply.error(err.to_errno());
                return;
            }
        }
        attr.ino = ino;
        reply.entry(&ONE_SEC, &attr.get_file_attr(), Generation(0));
        drop(db);
        let mut lc_list = self.lookup_count.lock().unwrap();
        let lc = lc_list.entry(ino).or_insert(0);
        *lc += 1;
        debug!(
            "filesystem:symlink, inode: {:?} lookup count:{:?}",
            ino, *lc
        );
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = parent.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let newparent = newparent.0 as u32;
        let newname = match newname.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let _ = &flags; // only meaningful on Linux; suppress unused warning on other platforms
        #[cfg(target_os = "linux")]
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let mut db = self.db.lock().unwrap();
        #[cfg(target_os = "linux")]
        if flags.contains(RenameFlags::RENAME_NOREPLACE) {
            match db.lookup(newparent, newname) {
                Ok(Some(_)) => {
                    reply.error(Errno::EEXIST);
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    warn!("rename lookup: {}", err);
                    reply.error(err.to_errno());
                    return;
                }
            }
        }
        let entry = match db.move_dentry(parent, name, newparent, newname) {
            Ok(n) => n,
            Err(err) => {
                warn!("rename move_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        if let Some(ino) = entry {
            let lc_list = self.lookup_count.lock().unwrap();
            if !lc_list.contains_key(&ino) {
                drop(lc_list);
                match db.delete_inode_if_noref(ino) {
                    Ok(n) => n,
                    Err(err) => {
                        warn!("rename delete_inode_if_noref: {}", err);
                        reply.error(err.to_errno());
                        return;
                    }
                };
            }
        }
        reply.ok();
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let newname = match newname.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut db = self.db.lock().unwrap();
        let attr = match db.link_dentry(ino.0 as u32, newparent.0 as u32, newname) {
            Ok(n) => n,
            Err(err) => {
                warn!("link link_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        reply.entry(&ONE_SEC, &attr.get_file_attr(), Generation(0));
        drop(db);
        let mut lc_list = self.lookup_count.lock().unwrap();
        let lc = lc_list.entry(ino.0 as u32).or_insert(0);
        *lc += 1;
        debug!("filesystem:link, lookup count:{:?}", *lc);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = ino.0 as u32;
        let mut stat = OpenFileStat::new();
        if flags.0 & O_APPEND > 0 {
            stat.append = true;
        }
        if flags.acc_mode() == OpenAccMode::O_RDONLY {
            stat.readonly = true;
        }
        #[cfg(not(target_os = "macos"))]
        if flags.0 & O_NOATIME > 0 {
            stat.noatime = true;
        }
        let mut handler = self.open_file_handler.lock().unwrap();
        let handle_list = handler.entry(ino).or_insert_with(OpenFileHandler::new);
        let fh = handle_list.next_fh;
        handle_list.list.insert(fh, stat);
        handle_list.next_fh += 1;
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let db = self.db.lock().unwrap();
        match db.read_data(ino.0 as u32, offset, size) {
            Ok(data) => reply.data(&data),
            Err(err) => {
                warn!("read read_data: {}", err);
                reply.error(err.to_errno());
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if data.is_empty() {
            reply.written(0);
            return;
        }
        let mut db = self.db.lock().unwrap();
        // One transaction per write request: all chunks and the size, or nothing.
        if let Err(err) = db.write_data(ino.0 as u32, offset, data) {
            warn!("write write_data: {}", err);
            reply.error(err.to_errno());
            return;
        }
        reply.written(data.len() as u32);
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0 as u32;
        let fh = fh.0;
        let mut handler = self.open_file_handler.lock().unwrap();
        let handle_list = handler.entry(ino).or_insert_with(OpenFileHandler::new);
        handle_list.list.remove(&fh);
        if handle_list.list.is_empty() {
            handler.remove(&ino);
        }
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Writes are already committed; with synchronous=NORMAL only a checkpoint
        // makes them durable. It covers the whole DB, not just this inode.
        match self.db.lock().unwrap().checkpoint() {
            Ok(()) => reply.ok(),
            Err(err) => {
                warn!("fsync checkpoint: {}", err);
                reply.error(err.to_errno());
            }
        }
    }

    fn fsyncdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.fsync(req, ino, fh, datasync, reply);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino = ino.0 as u32;
        let db = self.db.lock().unwrap();
        let dentries = match db.get_dentry(ino) {
            Ok(n) => n,
            Err(err) => {
                warn!("opendir get_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        drop(db);
        let mut handler = self.open_dir_handler.lock().unwrap();
        let handle_list = handler.entry(ino).or_insert_with(OpenDirHandler::new);
        let fh = handle_list.next_fh;
        handle_list.list.insert(fh, dentries);
        handle_list.next_fh += 1;
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    #[cfg(not(target_os = "macos"))]
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = ino.0 as u32;
        let fh = fh.0;
        let handler = self.open_dir_handler.lock().unwrap();
        let db_entries: &Vec<DEntry> = match match handler.get(&ino) {
            Some(n) => n.list.get(&fh),
            None => None,
        } {
            Some(n) => n,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        for (i, entry) in db_entries.iter().enumerate().skip(offset as usize) {
            let full = reply.add(
                INodeNo(entry.child_ino as u64),
                (i + 1) as u64,
                entry.file_type,
                &entry.filename,
            );
            if full {
                break;
            }
            debug!(
                "filesystem:readdir, ino: {:?} offset: {:?} kind: {:?} name: {}",
                entry.child_ino as u64,
                (i + 1) as i64,
                entry.file_type,
                entry.filename
            );
        }
        reply.ok();
    }

    #[cfg(target_os = "macos")]
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = ino.0 as u32;
        let db = self.db.lock().unwrap();
        let db_entries = match db.get_dentry(ino) {
            Ok(n) => n,
            Err(err) => {
                warn!("readdir get_dentry: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };

        for (i, entry) in db_entries.iter().enumerate().skip(offset as usize) {
            let full = reply.add(
                INodeNo(entry.child_ino as u64),
                (i + 1) as u64,
                entry.file_type,
                &entry.filename,
            );
            if full {
                break;
            }
            debug!(
                "filesystem:readdir, ino: {:?} offset: {:?} kind: {:?} name: {}",
                entry.child_ino as u64,
                (i + 1) as i64,
                entry.file_type,
                entry.filename
            );
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0 as u32;
        let fh = fh.0;
        let mut handler = self.open_dir_handler.lock().unwrap();
        let handle_list = handler.entry(ino).or_insert_with(OpenDirHandler::new);
        handle_list.list.remove(&fh);
        if handle_list.list.is_empty() {
            handler.remove(&ino);
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let stat = match statvfs::statvfs("/") {
            Ok(s) => s,
            Err(err) => {
                warn!("statfs: {}", err);
                reply.error(Errno::EIO);
                return;
            }
        };
        reply.statfs(
            stat.blocks() as u64,
            stat.blocks_free() as u64,
            stat.blocks_available() as u64,
            stat.files() as u64,
            stat.files_free() as u64,
            stat.block_size() as u32,
            stat.name_max() as u32,
            stat.fragment_size() as u32,
        );
        debug!("statfs {:?}", stat);
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut db = self.db.lock().unwrap();
        if flags & XATTR_CREATE > 0 || flags & XATTR_REPLACE > 0 {
            match db.get_xattr(ino, name) {
                Ok(_) => {
                    if flags & XATTR_CREATE > 0 {
                        reply.error(Errno::EEXIST);
                        return;
                    }
                }
                Err(err) => match &err {
                    Error::FsNoEnt { description: _ } => {
                        if flags & XATTR_REPLACE > 0 {
                            reply.error(Errno::NO_XATTR);
                            return;
                        }
                    }
                    _ => {
                        warn!("setxattr get_xattr ino={} name={}: {}", ino, name, err);
                        reply.error(err.to_errno());
                        return;
                    }
                },
            };
        }
        match db.set_xattr(ino, name, value) {
            Ok(n) => n,
            Err(err) => {
                warn!("setxattr set_xattr ino={} name={}: {}", ino, name, err);
                reply.error(err.to_errno());
                return;
            }
        };
        reply.ok();
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let ino = ino.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let db = self.db.lock().unwrap();
        let value = match db.get_xattr(ino, name) {
            Ok(n) => n,
            Err(err) => {
                warn!("getxattr ino={} name={}: {}", ino, name, err);
                let errno = match &err {
                    Error::FsNoEnt { .. } => Errno::NO_XATTR,
                    _ => err.to_errno(),
                };
                reply.error(errno);
                return;
            }
        };
        if size == 0 {
            reply.size(value.len() as u32);
        } else if size < value.len() as u32 {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(value.as_slice());
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let ino = ino.0 as u32;
        let db = self.db.lock().unwrap();
        let names = match db.list_xattr(ino) {
            Ok(n) => n,
            Err(err) => {
                warn!("listxattr ino={}: {}", ino, err);
                reply.error(err.to_errno());
                return;
            }
        };
        let mut data: Vec<u8> = Vec::new();
        for v in names {
            data.extend(v.bytes());
            data.push(0);
        }
        if size == 0 {
            reply.size(data.len() as u32);
        } else if size < data.len() as u32 {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(data.as_slice());
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let ino = ino.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut db = self.db.lock().unwrap();
        match db.delete_xattr(ino, name) {
            Ok(n) => n,
            Err(err) => {
                warn!("removexattr ino={} name={}: {}", ino, name, err);
                let errno = match &err {
                    Error::FsNoEnt { .. } => Errno::NO_XATTR,
                    _ => err.to_errno(),
                };
                reply.error(errno);
                return;
            }
        };
        reply.ok();
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let ino;
        let parent = parent.0 as u32;
        let name = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut db = self.db.lock().unwrap();
        let lookup_result = match db.lookup(parent, name) {
            Ok(n) => n,
            Err(err) => {
                warn!("create lookup: {}", err);
                reply.error(err.to_errno());
                return;
            }
        };
        let mut attr: DBFileAttr;
        match lookup_result {
            None => {
                let parent_attr = match db.get_inode(parent) {
                    Ok(n) => match n {
                        Some(n) => n,
                        None => {
                            reply.error(Errno::ENOENT);
                            return;
                        }
                    },
                    Err(err) => {
                        warn!("create get_inode: {}", err);
                        reply.error(err.to_errno());
                        return;
                    }
                };
                let now = SystemTime::now();
                attr = DBFileAttr {
                    ino: 0,
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::RegularFile,
                    perm: mode as u16,
                    nlink: 0,
                    uid: req.uid(),
                    gid: if parent_attr.perm & S_ISGID > 0 {
                        parent_attr.gid
                    } else {
                        req.gid()
                    },
                    rdev: 0,
                    flags: 0,
                };
                ino = match db.add_inode_and_dentry(parent, name, &attr) {
                    Ok(n) => n,
                    Err(err) => {
                        warn!("create add_inode_and_dentry: {}", err);
                        reply.error(err.to_errno());
                        return;
                    }
                };
                attr.ino = ino;
                debug!("filesystem:create, created:{:?}", attr);
            }
            Some(n) => {
                attr = n;
                ino = attr.ino;
                debug!("filesystem:create, existed:{:?}", attr);
            }
        };
        drop(db);
        let mut lc_list = self.lookup_count.lock().unwrap();
        let lc = lc_list.entry(ino).or_insert(0);
        *lc += 1;
        drop(lc_list);
        let mut handler = self.open_file_handler.lock().unwrap();
        let handle_list = handler.entry(ino).or_insert_with(OpenFileHandler::new);
        let fh = handle_list.next_fh;
        handle_list.list.insert(fh, OpenFileStat::new());
        handle_list.next_fh += 1;
        drop(handler);
        reply.created(
            &ONE_SEC,
            &attr.get_file_attr(),
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let mut lock_table = self.locks.lock().unwrap();
        let inode = ino.0 as u32;
        if let Some(locks) = lock_table.get_mut(&inode) {
            remove_locks_for_owner(locks, lock_owner.0);
            if locks.is_empty() {
                lock_table.remove(&inode);
            }
        }
        reply.ok();
    }

    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        _pid: u32,
        reply: ReplyLock,
    ) {
        let lock_table = self.locks.lock().unwrap();
        let empty = Vec::new();
        let locks = lock_table.get(&(ino.0 as u32)).unwrap_or(&empty);
        match find_conflicting_lock(locks, lock_owner.0, start, end, typ) {
            Some(c) => reply.locked(c.start, c.end, c.typ, c.pid),
            None => reply.locked(0, 0, F_UNLCK, 0),
        }
    }

    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        if typ != F_RDLCK && typ != F_WRLCK && typ != F_UNLCK {
            reply.error(Errno::EINVAL);
            return;
        }
        let mut lock_table = self.locks.lock().unwrap();
        let inode = ino.0 as u32;

        if typ == F_UNLCK {
            if let Some(locks) = lock_table.get_mut(&inode) {
                apply_lock(locks, lock_owner.0, pid, start, end, typ);
                if locks.is_empty() {
                    lock_table.remove(&inode);
                }
            }
            reply.ok();
            return;
        }

        // Check for conflicts before acquiring.
        {
            let empty = Vec::new();
            let locks = lock_table.get(&inode).unwrap_or(&empty);
            if find_conflicting_lock(locks, lock_owner.0, start, end, typ).is_some() {
                // Blocking (sleep=true / F_SETLKW) would deadlock a single-threaded FUSE
                // daemon. Return EAGAIN and let the caller retry, matching sshfs behaviour.
                reply.error(Errno::EAGAIN);
                return;
            }
        }

        let locks = lock_table.entry(inode).or_default();
        apply_lock(locks, lock_owner.0, pid, start, end, typ);
        reply.ok();
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    fn rdlk(owner: u64, start: u64, end: u64) -> PosixLock {
        PosixLock {
            owner,
            pid: 1,
            start,
            end,
            typ: F_RDLCK,
        }
    }
    fn wrlk(owner: u64, start: u64, end: u64) -> PosixLock {
        PosixLock {
            owner,
            pid: 1,
            start,
            end,
            typ: F_WRLCK,
        }
    }

    #[test]
    fn test_ranges_overlap() {
        assert!(ranges_overlap(0, 10, 5, 15));
        assert!(ranges_overlap(5, 15, 0, 10));
        assert!(ranges_overlap(0, 10, 10, 20)); // touching boundary
        assert!(ranges_overlap(5, 10, 5, 10)); // identical
        assert!(ranges_overlap(0, 20, 5, 10)); // containment
        assert!(!ranges_overlap(0, 4, 5, 10)); // gap
        assert!(!ranges_overlap(11, 20, 0, 10)); // gap other side
    }

    #[test]
    fn test_no_conflict_same_owner() {
        let locks = vec![wrlk(1, 0, 100)];
        assert!(find_conflicting_lock(&locks, 1, 0, 100, F_WRLCK).is_none());
    }

    #[test]
    fn test_rdlck_rdlck_no_conflict() {
        let locks = vec![rdlk(1, 0, 100)];
        assert!(find_conflicting_lock(&locks, 2, 0, 100, F_RDLCK).is_none());
    }

    #[test]
    fn test_wrlck_blocks_rdlck() {
        let locks = vec![wrlk(1, 0, 100)];
        assert!(find_conflicting_lock(&locks, 2, 0, 100, F_RDLCK).is_some());
    }

    #[test]
    fn test_rdlck_blocks_wrlck() {
        let locks = vec![rdlk(1, 0, 100)];
        assert!(find_conflicting_lock(&locks, 2, 0, 100, F_WRLCK).is_some());
    }

    #[test]
    fn test_no_conflict_non_overlapping_ranges() {
        let locks = vec![wrlk(1, 0, 49)];
        assert!(find_conflicting_lock(&locks, 2, 50, 100, F_WRLCK).is_none());
    }

    #[test]
    fn test_apply_lock_basic_acquire() {
        let mut locks: Vec<PosixLock> = Vec::new();
        apply_lock(&mut locks, 1, 100, 0, 99, F_WRLCK);
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].start, 0);
        assert_eq!(locks[0].end, 99);
    }

    #[test]
    fn test_apply_lock_unlock_removes() {
        let mut locks = vec![wrlk(1, 0, 100)];
        apply_lock(&mut locks, 1, 100, 0, 100, F_UNLCK);
        assert!(locks.is_empty());
    }

    #[test]
    fn test_apply_lock_hole_punch() {
        let mut locks = vec![wrlk(1, 0, 100)];
        apply_lock(&mut locks, 1, 1, 30, 60, F_UNLCK);
        let mut owned: Vec<_> = locks.iter().filter(|l| l.owner == 1).collect();
        owned.sort_by_key(|l| l.start);
        assert_eq!(owned.len(), 2);
        assert_eq!((owned[0].start, owned[0].end), (0, 29));
        assert_eq!((owned[1].start, owned[1].end), (61, 100));
    }

    #[test]
    fn test_apply_lock_coalesce_adjacent() {
        let mut locks = vec![wrlk(1, 0, 49), wrlk(1, 51, 100)];
        apply_lock(&mut locks, 1, 1, 50, 50, F_WRLCK);
        let owned: Vec<_> = locks.iter().filter(|l| l.owner == 1).collect();
        assert_eq!(owned.len(), 1);
        assert_eq!((owned[0].start, owned[0].end), (0, 100));
    }

    #[test]
    fn test_apply_lock_does_not_affect_other_owners() {
        let mut locks = vec![rdlk(2, 0, 100)];
        apply_lock(&mut locks, 1, 1, 0, 100, F_WRLCK);
        assert_eq!(locks.len(), 2);
        assert!(locks.iter().any(|l| l.owner == 2));
        assert!(locks.iter().any(|l| l.owner == 1));
    }

    #[test]
    fn test_remove_locks_for_owner() {
        let mut locks = vec![wrlk(1, 0, 100), rdlk(2, 0, 100), wrlk(1, 200, 300)];
        remove_locks_for_owner(&mut locks, 1);
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].owner, 2);
    }
}
