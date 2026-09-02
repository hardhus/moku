//! FUSE `Filesystem` implementation over `moku-vault-fs`'s `VolumeEngine`
//! (plan Faz 6). Developed and cross-compile-checked (`cargo check
//! --target x86_64-unknown-linux-gnu -p moku-vault-mount`) from a Windows
//! dev machine — there is no FUSE/Linux environment available this
//! session, so this is **not runtime-tested**. Architecturally mirrors
//! `winfsp_shim.rs`: only the operations a real read/write filesystem
//! needs are implemented; everything else keeps fuser's default
//! `ENOSYS`/no-op behavior (no ACLs, xattrs, symlinks, locks — matches
//! the vault format's own no-metadata-beyond-size/mtime design).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, Request, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, TimeOrNow, WriteFlags,
};

use moku_vault_fs::{Attr, FileKind, VaultFsError, VirtualPath, VolumeEngine};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;

/// Maps `VirtualPath`s (moku-vault-fs's own addressing scheme) to the
/// stable `u64` inode numbers FUSE requires. `VolumeEngine` itself is
/// entirely path-addressed and knows nothing about inodes — this table
/// lives only in the mount shim, same division of concerns as
/// `winfsp_shim.rs`'s `VaultFileHandle { path, fh }`.
struct InodeTable {
    by_ino: HashMap<u64, VirtualPath>,
    by_path: HashMap<VirtualPath, u64>,
    next: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(ROOT_INO, VirtualPath::root());
        by_path.insert(VirtualPath::root(), ROOT_INO);
        Self { by_ino, by_path, next: ROOT_INO + 1 }
    }

    fn ino_for(&mut self, path: &VirtualPath) -> u64 {
        if let Some(&ino) = self.by_path.get(path) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, path.clone());
        self.by_path.insert(path.clone(), ino);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<VirtualPath> {
        self.by_ino.get(&ino).cloned()
    }

    fn forget_path(&mut self, path: &VirtualPath) {
        if let Some(ino) = self.by_path.remove(path) {
            self.by_ino.remove(&ino);
        }
    }

    fn rename_path(&mut self, old: &VirtualPath, new: VirtualPath) {
        if let Some(ino) = self.by_path.remove(old) {
            self.by_ino.insert(ino, new.clone());
            self.by_path.insert(new, ino);
        }
    }
}

fn map_err(e: VaultFsError) -> Errno {
    match e {
        VaultFsError::NotFound => Errno::ENOENT,
        VaultFsError::AlreadyExists => Errno::EEXIST,
        VaultFsError::NotADirectory => Errno::ENOTDIR,
        VaultFsError::IsADirectory => Errno::EISDIR,
        VaultFsError::NotEmpty => Errno::ENOTEMPTY,
        VaultFsError::NameTooLong => Errno::ENAMETOOLONG,
        VaultFsError::QuotaExceeded => Errno::ENOSPC,
        VaultFsError::BadFileHandle => Errno::EBADF,
        VaultFsError::Other(_) => Errno::EIO,
    }
}

fn to_file_attr(ino: u64, attr: &Attr, uid: u32, gid: u32) -> FileAttr {
    let mtime = UNIX_EPOCH + Duration::from_secs(attr.modified_at);
    let crtime = UNIX_EPOCH + Duration::from_secs(attr.created_at);
    FileAttr {
        ino: INodeNo(ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime,
        kind: match attr.kind {
            FileKind::Directory => FileType::Directory,
            FileKind::File => FileType::RegularFile,
        },
        perm: match attr.kind {
            FileKind::Directory => 0o755,
            FileKind::File => 0o644,
        },
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

struct VaultFsFilesystem {
    engine: VolumeEngine,
    inodes: Mutex<InodeTable>,
}

impl Filesystem for VaultFsFilesystem {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut table = self.inodes.lock().unwrap();
        let Some(parent_path) = table.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_path = parent_path.join(name);
        match self.engine.getattr(&child_path) {
            Ok(attr) => {
                let ino = table.ino_for(&child_path);
                drop(table);
                reply.entry(&TTL, &to_file_attr(ino, &attr, req.uid(), req.gid()), Generation(0));
            }
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let path = self.inodes.lock().unwrap().path_of(ino.0);
        let Some(path) = path else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.getattr(&path) {
            Ok(attr) => reply.attr(&TTL, &to_file_attr(ino.0, &attr, req.uid(), req.gid())),
            Err(e) => reply.error(map_err(e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let path = self.inodes.lock().unwrap().path_of(ino.0);
        let Some(path) = path else {
            reply.error(Errno::ENOENT);
            return;
        };
        // Only size changes are persisted — the vault format tracks no
        // other metadata beyond size and on-disk mtime (same v1 scope cut
        // as winfsp_shim.rs's set_basic_info).
        let result = match size {
            Some(new_size) => self.engine.setattr_size(&path, new_size),
            None => self.engine.getattr(&path),
        };
        match result {
            Ok(attr) => reply.attr(&TTL, &to_file_attr(ino.0, &attr, req.uid(), req.gid())),
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut table = self.inodes.lock().unwrap();
        let Some(parent_path) = table.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.mkdir(&parent_path, name) {
            Ok(attr) => {
                let ino = table.ino_for(&parent_path.join(name));
                drop(table);
                reply.entry(&TTL, &to_file_attr(ino, &attr, req.uid(), req.gid()), Generation(0));
            }
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut table = self.inodes.lock().unwrap();
        let Some(parent_path) = table.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.unlink(&parent_path, name) {
            Ok(()) => {
                table.forget_path(&parent_path.join(name));
                reply.ok();
            }
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut table = self.inodes.lock().unwrap();
        let Some(parent_path) = table.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.rmdir(&parent_path, name) {
            Ok(()) => {
                table.forget_path(&parent_path.join(name));
                reply.ok();
            }
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut table = self.inodes.lock().unwrap();
        let (Some(old_parent), Some(new_parent)) = (table.path_of(parent.0), table.path_of(newparent.0)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.rename(&old_parent, name, &new_parent, newname) {
            Ok(()) => {
                table.rename_path(&old_parent.join(name), new_parent.join(newname));
                reply.ok();
            }
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let path = self.inodes.lock().unwrap().path_of(ino.0);
        let Some(path) = path else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.open(&path) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(map_err(e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut table = self.inodes.lock().unwrap();
        let Some(parent_path) = table.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.engine.create(&parent_path, name) {
            Ok((fh, attr)) => {
                let ino = table.ino_for(&parent_path.join(name));
                drop(table);
                reply.created(&TTL, &to_file_attr(ino, &attr, req.uid(), req.gid()), Generation(0), FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0u8; size as usize];
        match self.engine.read(fh.0, offset, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => reply.error(map_err(e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.engine.write(fh.0, offset, data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(map_err(e)),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let _ = self.engine.release(fh.0);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // Directory ops are stateless on VolumeEngine (always take a
        // path), so there's nothing to open — a dummy handle is fine.
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let mut table = self.inodes.lock().unwrap();
        let Some(path) = table.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let entries = match self.engine.read_dir(&path) {
            Ok(e) => e,
            Err(e) => {
                reply.error(map_err(e));
                return;
            }
        };

        let parent_ino = table.ino_for(&path.parent().unwrap_or_else(VirtualPath::root));
        let mut full: Vec<(u64, FileType, String)> = Vec::with_capacity(entries.len() + 2);
        full.push((ino.0, FileType::Directory, ".".to_string()));
        full.push((parent_ino, FileType::Directory, "..".to_string()));
        for entry in &entries {
            let child_ino = table.ino_for(&path.join(&entry.name));
            let kind = match entry.kind {
                FileKind::Directory => FileType::Directory,
                FileKind::File => FileType::RegularFile,
            };
            full.push((child_ino, kind, entry.name.clone()));
        }
        drop(table);

        for (i, (child_ino, kind, name)) in full.iter().enumerate().skip(offset as usize) {
            let next_offset = (i + 1) as u64;
            if reply.add(INodeNo(*child_ino), next_offset, *kind, name) {
                break; // buffer full — kernel will call again with this offset
            }
        }
        reply.ok();
    }
}

/// Mounts `engine` at `mountpoint`, blocks until `stop_rx` receives a
/// signal, then unmounts cleanly. Same signature as the WinFsp shim's
/// `mount_and_wait`, so `moku-vault-daemon::worker::run` needs no
/// platform-specific code at all.
pub fn mount_and_wait(engine: VolumeEngine, mountpoint: &str, stop_rx: Receiver<()>) -> Result<()> {
    let fs = VaultFsFilesystem { engine, inodes: Mutex::new(InodeTable::new()) };
    // `Config` is `#[non_exhaustive]`, so it can't be built with a struct
    // literal outside fuser's own crate — start from its `Default` and
    // mutate the one field that matters.
    let mut options = Config::default();
    options.mount_options = vec![MountOption::FSName("mokuvault".to_string()), MountOption::DefaultPermissions];

    let session = fuser::spawn_mount(fs, mountpoint, &options).map_err(|e| anyhow!("failed to mount at '{mountpoint}': {e}"))?;

    let _ = stop_rx.recv();

    session.umount_and_join().map_err(|e| anyhow!("failed to unmount: {e}"))
}
