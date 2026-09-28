//! WinFsp `FileSystemContext` implementation over `moku-volume-fs`'s
//! `VolumeEngine`. Only the operations a real read/write filesystem needs
//! are implemented; everything else keeps the trait's default
//! `STATUS_INVALID_DEVICE_REQUEST` (ACL get/set, reparse points, named
//! streams, extended attributes — none of which the volume format
//! supports, matching moku's existing no-ACL, no-EA storage model).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Receiver;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL, STATUS_END_OF_FILE, STATUS_INVALID_HANDLE,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNSUCCESSFUL,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE,
};
use winfsp::U16CStr;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};
use winfsp::notify::{Notifier, NotifyInfo, NotifyingFileSystemContext};

use moku_volume_fs::{Attr, DirEntry, FileKind, VolumeFsError, VirtualPath, VolumeEngine};

const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// FILETIME epoch (1601-01-01) offset from the Unix epoch, in seconds.
const FILETIME_UNIX_EPOCH_DIFF_SECS: u64 = 11_644_473_600;

fn unix_secs_to_filetime(secs: u64) -> u64 {
    (secs + FILETIME_UNIX_EPOCH_DIFF_SECS) * 10_000_000
}

/// Converts a WinFsp wide, backslash-separated path (`\notes\a.md`, or
/// `\` for the root) into our engine's forward-slash `VirtualPath`.
fn to_virtual_path(file_name: &U16CStr) -> VirtualPath {
    let s = file_name.to_string_lossy().replace('\\', "/");
    VirtualPath::parse(&s)
}

fn map_err(e: VolumeFsError) -> winfsp::FspError {
    match e {
        VolumeFsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND.into(),
        VolumeFsError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION.into(),
        VolumeFsError::NotADirectory => STATUS_NOT_A_DIRECTORY.into(),
        VolumeFsError::IsADirectory => STATUS_NOT_A_DIRECTORY.into(),
        VolumeFsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY.into(),
        VolumeFsError::NameTooLong => STATUS_OBJECT_NAME_INVALID.into(),
        VolumeFsError::QuotaExceeded => STATUS_DISK_FULL.into(),
        VolumeFsError::BadFileHandle => STATUS_INVALID_HANDLE.into(),
        // `Locked` only comes from `VolumeLock::acquire`, before a mount
        // exists; no filesystem callback can produce it.
        VolumeFsError::Locked | VolumeFsError::Other(_) => STATUS_UNSUCCESSFUL.into(),
    }
}

fn write_file_info(dst: &mut FileInfo, attr: &Attr, index_number: u64) {
    dst.file_attributes = match attr.kind {
        FileKind::Directory => FILE_ATTRIBUTE_DIRECTORY.0,
        FileKind::File => FILE_ATTRIBUTE_NORMAL.0,
    };
    dst.reparse_tag = 0;
    dst.allocation_size = attr.size;
    dst.file_size = attr.size;
    dst.creation_time = unix_secs_to_filetime(attr.created_at);
    let modified = unix_secs_to_filetime(attr.modified_at);
    dst.last_access_time = modified;
    dst.last_write_time = modified;
    dst.change_time = modified;
    dst.index_number = index_number;
    dst.hard_links = 0;
    dst.ea_size = 0;
}

/// Folds the volume format's 16-byte per-file/per-directory identity value
/// (a file's random `file_id` header, or a directory's `.moku_dir_iv`) down
/// to the `u64` WinFsp's `index_number` expects. Degrades to 0 on error
/// (e.g. a racing delete) rather than failing the caller's stat operation
/// — matches this file's existing skip/degrade idiom (see `read_directory`).
fn index_number_for(engine: &VolumeEngine, path: &VirtualPath) -> u64 {
    match engine.file_id(path) {
        Ok(id) => {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&id[0..8]);
            u64::from_le_bytes(buf)
        }
        Err(_) => 0,
    }
}

/// One open file or directory handle. Directories carry no engine file
/// handle (`fh: None`) — `VolumeEngine`'s directory operations are
/// stateless and always take a path, so there's nothing to keep open.
pub struct VolumeFileHandle {
    path: VirtualPath,
    fh: Option<u64>,
}

/// A queued OS change-notification event, drained and delivered by
/// `NotifyingFileSystemContext`'s timer-driven `should_notify`/`notify`.
struct PendingNotify {
    path: String,
    filter: u32,
    action: u32,
}

/// Caps `pending_notify`'s growth if the notify timer thread ever stalls —
/// dispatch threads drop the oldest event rather than block or grow
/// unbounded memory.
const MAX_PENDING_NOTIFY: usize = 4096;

pub struct VolumeFsContext {
    engine: Arc<VolumeEngine>,
    pending_notify: Mutex<VecDeque<PendingNotify>>,
}

impl VolumeFsContext {
    /// Queues a change-notification event for the next notify-timer tick.
    /// `path` is this crate's forward-slash `VirtualPath`; WinFsp expects
    /// backslash-separated names, same convention `to_virtual_path` parses
    /// in reverse.
    fn queue_notify(&self, path: &VirtualPath, filter: u32, action: u32) {
        let mut q = self.pending_notify.lock().unwrap();
        if q.len() >= MAX_PENDING_NOTIFY {
            q.pop_front();
        }
        q.push_back(PendingNotify { path: path.as_str().replace('/', "\\"), filter, action });
    }

    /// Test-only inspection hook: lets a real-mount test poll for the
    /// pending-notify queue draining after an operation, proving the full
    /// dispatch-thread-push -> timer-thread-drain pipeline actually runs.
    #[cfg(test)]
    pub(crate) fn pending_notify_len(&self) -> usize {
        self.pending_notify.lock().unwrap().len()
    }
}

impl NotifyingFileSystemContext<Vec<PendingNotify>> for VolumeFsContext {
    fn should_notify(&self) -> Option<Vec<PendingNotify>> {
        let mut q = self.pending_notify.lock().unwrap();
        if q.is_empty() {
            None
        } else {
            Some(q.drain(..).collect())
        }
    }

    fn notify(&self, events: Vec<PendingNotify>, notifier: &Notifier) {
        for ev in events {
            let mut info: NotifyInfo<255> = NotifyInfo::new();
            info.filter = ev.filter;
            info.action = ev.action;
            // Skip (don't abort the rest of the batch) if a path doesn't
            // fit the fixed-size wide-name buffer.
            if info.set_name(&ev.path).is_ok() {
                notifier.notify(&info);
            }
        }
    }
}

impl FileSystemContext for VolumeFsContext {
    type FileContext = VolumeFileHandle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = to_virtual_path(file_name);
        let attr = self.engine.getattr(&path).map_err(map_err)?;
        let attributes = match attr.kind {
            FileKind::Directory => FILE_ATTRIBUTE_DIRECTORY.0,
            FileKind::File => FILE_ATTRIBUTE_NORMAL.0,
        };
        // No ACL support (matches the volume format's no-metadata-beyond-
        // size/mtime design) — always report a zero-length descriptor.
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = to_virtual_path(file_name);
        let attr = self.engine.getattr(&path).map_err(map_err)?;
        let fh = match attr.kind {
            FileKind::Directory => None,
            FileKind::File => Some(self.engine.open(&path).map_err(map_err)?),
        };
        write_file_info(file_info.as_mut(), &attr, index_number_for(&self.engine, &path));
        Ok(VolumeFileHandle { path, fh })
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let is_directory = (create_options & FILE_DIRECTORY_FILE) != 0;
        let path = to_virtual_path(file_name);
        let parent = path
            .parent()
            .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_INVALID))?;
        let name = path
            .file_name()
            .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_INVALID))?
            .to_string();

        if is_directory {
            let attr = self.engine.mkdir(&parent, &name).map_err(map_err)?;
            write_file_info(file_info.as_mut(), &attr, index_number_for(&self.engine, &path));
            self.queue_notify(&path, FILE_NOTIFY_CHANGE_DIR_NAME.0, FILE_ACTION_ADDED.0);
            Ok(VolumeFileHandle { path, fh: None })
        } else {
            let (fh, attr) = self.engine.create(&parent, &name).map_err(map_err)?;
            write_file_info(file_info.as_mut(), &attr, index_number_for(&self.engine, &path));
            self.queue_notify(&path, FILE_NOTIFY_CHANGE_FILE_NAME.0, FILE_ACTION_ADDED.0);
            Ok(VolumeFileHandle { path, fh: Some(fh) })
        }
    }

    fn close(&self, context: Self::FileContext) {
        if let Some(fh) = context.fh {
            let _ = self.engine.release(fh);
        }
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        const FSP_CLEANUP_DELETE: u32 = 0x01;
        if flags & FSP_CLEANUP_DELETE == 0 {
            return;
        }
        let Some(parent) = context.path.parent() else {
            return;
        };
        let Some(name) = context.path.file_name() else {
            return;
        };
        if context.fh.is_some() {
            if self.engine.unlink(&parent, name).is_ok() {
                self.queue_notify(&context.path, FILE_NOTIFY_CHANGE_FILE_NAME.0, FILE_ACTION_REMOVED.0);
            }
        } else if self.engine.rmdir(&parent, name).is_ok() {
            self.queue_notify(&context.path, FILE_NOTIFY_CHANGE_DIR_NAME.0, FILE_ACTION_REMOVED.0);
        }
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self.engine.getattr(&context.path).map_err(map_err)?;
        write_file_info(file_info, &attr, index_number_for(&self.engine, &context.path));
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // The volume format tracks only size and on-disk mtime — custom
        // attribute/timestamp writes are accepted but not persisted
        // (v1 scope cut, matches moku-volume-fs's Attr shape).
        let attr = self.engine.getattr(&context.path).map_err(map_err)?;
        write_file_info(file_info, &attr, index_number_for(&self.engine, &context.path));
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self
            .engine
            .setattr_size(&context.path, new_size)
            .map_err(map_err)?;
        write_file_info(file_info, &attr, index_number_for(&self.engine, &context.path));
        self.queue_notify(&context.path, FILE_NOTIFY_CHANGE_SIZE.0, FILE_ACTION_MODIFIED.0);
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self
            .engine
            .setattr_size(&context.path, 0)
            .map_err(map_err)?;
        write_file_info(file_info, &attr, index_number_for(&self.engine, &context.path));
        self.queue_notify(
            &context.path,
            FILE_NOTIFY_CHANGE_SIZE.0 | FILE_NOTIFY_CHANGE_LAST_WRITE.0,
            FILE_ACTION_MODIFIED.0,
        );
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if !delete_file {
            return Ok(());
        }
        if context.fh.is_none() {
            let entries = self.engine.read_dir(&context.path).map_err(map_err)?;
            if !entries.is_empty() {
                return Err(STATUS_DIRECTORY_NOT_EMPTY.into());
            }
        }
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let fh = context
            .fh
            .ok_or(winfsp::FspError::from(STATUS_NOT_A_DIRECTORY))?;
        let attr = self.engine.getattr(&context.path).map_err(map_err)?;
        if offset >= attr.size {
            return Err(STATUS_END_OF_FILE.into());
        }
        let n = self.engine.read(fh, offset, buffer).map_err(map_err)?;
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let fh = context
            .fh
            .ok_or(winfsp::FspError::from(STATUS_NOT_A_DIRECTORY))?;
        let attr_before = self.engine.getattr(&context.path).map_err(map_err)?;

        let start = if write_to_eof {
            attr_before.size
        } else {
            offset
        };
        let n = if constrained_io {
            if start >= attr_before.size {
                write_file_info(file_info, &attr_before, index_number_for(&self.engine, &context.path));
                return Ok(0);
            }
            let end = (start + buffer.len() as u64).min(attr_before.size);
            let len = (end - start) as usize;
            self.engine
                .write(fh, start, &buffer[..len])
                .map_err(map_err)?
        } else {
            self.engine.write(fh, start, buffer).map_err(map_err)?
        };

        let attr_after = self.engine.getattr(&context.path).map_err(map_err)?;
        write_file_info(file_info, &attr_after, index_number_for(&self.engine, &context.path));
        if n > 0 {
            self.queue_notify(
                &context.path,
                FILE_NOTIFY_CHANGE_LAST_WRITE.0 | FILE_NOTIFY_CHANGE_SIZE.0,
                FILE_ACTION_MODIFIED.0,
            );
        }
        Ok(n as u32)
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // VolumeEngine holds no OS-level write buffering of its own -- every
        // write already lands on the backing file synchronously -- so this
        // is a pass-through success. Left unimplemented, this falls to
        // WinFsp's default STATUS_INVALID_DEVICE_REQUEST, which Win32
        // surfaces as ERROR_INVALID_FUNCTION ("Incorrect function") -- what
        // broke an editor's atomic save (write temp file, flush, rename
        // over the destination) right after the flush call.
        if let Some(ctx) = context {
            let attr = self.engine.getattr(&ctx.path).map_err(map_err)?;
            write_file_info(file_info, &attr, index_number_for(&self.engine, &ctx.path));
        }
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let mut cursor = 0u32;
        let mut dir_info: DirInfo<255> = DirInfo::new();

        if marker.is_none() {
            let self_attr = self.engine.getattr(&context.path).map_err(map_err)?;
            dir_info.reset();
            write_file_info(dir_info.file_info_mut(), &self_attr, index_number_for(&self.engine, &context.path));
            dir_info.set_name_raw([b'.' as u16].as_slice())?;
            if !dir_info.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }

            let parent_path = context.path.parent();
            let parent_attr = parent_path
                .as_ref()
                .and_then(|p| self.engine.getattr(p).ok())
                .unwrap_or_else(|| self_attr.clone());
            let parent_index = parent_path
                .as_ref()
                .map(|p| index_number_for(&self.engine, p))
                .unwrap_or(0);
            dir_info.reset();
            write_file_info(dir_info.file_info_mut(), &parent_attr, parent_index);
            dir_info.set_name_raw([b'.' as u16, b'.' as u16].as_slice())?;
            if !dir_info.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }
        }

        let entries: Vec<DirEntry> = self.engine.read_dir(&context.path).map_err(map_err)?;
        let marker_name = marker.inner_as_cstr().map(|m| m.to_string_lossy());
        let skip_dots = matches!(marker_name.as_deref(), Some(".") | Some(".."));
        let start_after = if skip_dots { None } else { marker_name };

        for entry in &entries {
            if let Some(after) = &start_after
                && entry.name.as_str() <= after.as_str()
            {
                continue;
            }
            let child_path = context.path.join(&entry.name);
            let Ok(attr) = self.engine.getattr(&child_path) else {
                continue;
            };
            dir_info.reset();
            write_file_info(dir_info.file_info_mut(), &attr, index_number_for(&self.engine, &child_path));
            dir_info.set_name(entry.name.as_str())?;
            if !dir_info.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }
        }

        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let old_parent = context
            .path
            .parent()
            .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_INVALID))?;
        let old_name = context
            .path
            .file_name()
            .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_INVALID))?
            .to_string();
        let new_path = to_virtual_path(new_file_name);
        let new_parent = new_path
            .parent()
            .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_INVALID))?;
        let new_name = new_path
            .file_name()
            .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_INVALID))?
            .to_string();

        self.engine
            .rename(&old_parent, &old_name, &new_parent, &new_name, replace_if_exists)
            .map_err(map_err)?;

        let filter = if context.fh.is_some() {
            FILE_NOTIFY_CHANGE_FILE_NAME.0
        } else {
            FILE_NOTIFY_CHANGE_DIR_NAME.0
        };
        self.queue_notify(&context.path, filter, FILE_ACTION_RENAMED_OLD_NAME.0);
        self.queue_notify(&new_path, filter, FILE_ACTION_RENAMED_NEW_NAME.0);
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let total = self.engine.size_limit_bytes();
        let used = self.engine.usage_bytes();
        out_volume_info.total_size = total;
        out_volume_info.free_size = total.saturating_sub(used);
        out_volume_info.set_volume_label("Moku Volume");
        Ok(())
    }
}

/// Mounts `engine` at `mountpoint`, calls `on_mounted` once the mount is
/// actually live (before blocking), then blocks until `stop_rx` receives a
/// signal, then unmounts cleanly.
///
/// `mountpoint` should be a drive letter ("X:") — verified end-to-end
/// (mount, full CRUD, clean unmount) against a real WinFsp install. An
/// empty-directory mountpoint is also accepted by WinFsp in principle,
/// but in manual testing `host.mount()` returned `STATUS_OBJECT_NAME_COLLISION`
/// for a directory target even though the mount then worked correctly for
/// every filesystem operation — an unexplained quirk worth investigating
/// before that form is exposed as a supported option.
pub fn mount_and_wait(
    engine: Arc<VolumeEngine>,
    mountpoint: &str,
    stop_rx: Receiver<()>,
    on_mounted: impl FnOnce(),
) -> Result<()> {
    winfsp::winfsp_init()
        .map_err(|e| anyhow!("WinFsp initialization failed: {e:?} (is WinFsp installed?)"))?;

    let mut volume_params = VolumeParams::new();
    volume_params
        .filesystem_name("MokuVolume")
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .read_only_volume(false)
        .flush_and_purge_on_cleanup(true);

    let context = VolumeFsContext {
        engine,
        pending_notify: Mutex::new(VecDeque::new()),
    };
    let params = FileSystemParams::default_params(volume_params);
    // 250ms notify-timer interval: a reasonable latency/CPU tradeoff for an
    // external-change watcher (e.g. Obsidian) to see create/write/delete/
    // rename events without polling too aggressively.
    let mut host: FileSystemHost<VolumeFsContext> =
        FileSystemHost::new_with_timer::<Vec<PendingNotify>, 250>(params, context)
            .map_err(|e| anyhow!("failed to create WinFsp filesystem host: {e:?}"))?;

    host.mount(mountpoint)
        .map_err(|e| anyhow!("failed to mount at '{mountpoint}': {e:?}"))?;
    if let Err(e) = host.start() {
        // Mount already attached the volume to the OS; without this it
        // would be left stuck (drive letter reserved, nothing servicing
        // I/O) until the whole worker process exits.
        host.unmount();
        return Err(anyhow!("failed to start WinFsp dispatcher: {e:?}"));
    }

    on_mounted();

    let _ = stop_rx.recv();

    host.stop();
    host.unmount();
    // Persisting the in-memory usage counter is the caller's job
    // (`moku-volume-daemon::worker::run` keeps its own Arc clone and
    // flushes after this returns, on every exit path, and reports a
    // failure instead of swallowing it).
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_free_drive_letter() -> Option<String> {
        ('E'..='Z')
            .rev()
            .map(|c| format!("{c}:"))
            .find(|letter| !std::path::Path::new(&format!("{letter}\\")).exists())
    }

    /// Mounts `engine` at a fresh free drive letter, runs `body` against the
    /// mounted root path, then always stops/unmounts and asserts the drive
    /// is gone -- even if `body` panics (its panic is caught, cleanup still
    /// runs, then re-raised as a failed `.expect(...)`). Shared by every
    /// phase of the real-mount tests below so each phase gets its own
    /// clean mount/unmount cycle without duplicating the thread/channel
    /// plumbing.
    /// Serializes every real-mount test in this process. Without this, two
    /// tests running concurrently (the default within one `cargo test`
    /// binary) can both call `find_free_drive_letter()` before either has
    /// actually mounted, pick the same "free" letter, and race — one
    /// mount then fails with `STATUS_OBJECT_NAME_COLLISION`/disconnects.
    static MOUNT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_real_mount(engine: VolumeEngine, body: impl FnOnce(&std::path::Path) + std::panic::UnwindSafe) {
        let _guard = MOUNT_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mountpoint =
            find_free_drive_letter().expect("no free drive letter available for the test");
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let (mounted_tx, mounted_rx) = std::sync::mpsc::channel();
        let mount_mountpoint = mountpoint.clone();
        let mount_thread = std::thread::spawn(move || {
            mount_and_wait(Arc::new(engine), &mount_mountpoint, stop_rx, move || {
                let _ = mounted_tx.send(());
            })
        });

        mounted_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("on_mounted should fire once the WinFsp mount actually succeeds");

        let root = std::path::Path::new(&mountpoint).to_path_buf();
        let result = std::panic::catch_unwind(|| body(&root));

        let _ = stop_tx.send(());
        mount_thread
            .join()
            .expect("mount thread panicked")
            .expect("mount_and_wait failed");

        assert!(
            !std::path::Path::new(&format!("{mountpoint}\\")).exists(),
            "drive must be gone after a clean unmount"
        );
        result.expect("filesystem operations against the mounted drive failed");
    }

    /// End-to-end proof that a real encrypted volume actually mounts as a
    /// Windows drive and behaves like a normal filesystem, covering more
    /// than the happy path: CRUD + nested dirs (phase 1), an editor-style
    /// atomic save that writes a temp file, `fsync`s it, then renames it
    /// over an *already-existing* destination (the exact sequence that
    /// broke Helix's `:w` -- first with STATUS_OBJECT_NAME_COLLISION before
    /// `replace_if_exists`, then with ERROR_INVALID_FUNCTION before `flush`
    /// was implemented), a rename that must fail when the destination is an
    /// existing directory, persistence of on-disk data across a real
    /// unmount + fresh remount, and a real write past the volume's quota
    /// failing with a proper OS error rather than hanging or corrupting
    /// state. Requires WinFsp installed and a free drive letter -- not
    /// `#[ignore]`d, since this repo has no CI and the one developer
    /// working on it always has WinFsp installed; `cargo test` will fail
    /// hard on a machine without WinFsp.
    #[test]
    fn test_real_mount_full_crud_roundtrip() {
        let volume_tmp = tempfile::tempdir().expect("tempdir");
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let master_key = rt.block_on(async {
            let security =
                moku_core::SecurityManager::new_with_root(volume_tmp.path().to_path_buf());
            security
                .initialize_vault(zeroize::Zeroizing::new("smoke-test-password".to_string()))
                .await
                .expect("init vault")
        });
        let open_engine = || {
            let keys = moku_volume_fs::derive_volume_keys(&master_key);
            VolumeEngine::open_volume(
                volume_tmp.path().join("data"),
                keys,
                volume_tmp.path().join("usage.json"),
                50_000_000,
                moku_volume_fs::VolumeLock::acquire(volume_tmp.path()).expect("volume lock"),
            )
            .expect("open_volume")
        };

        // Phase 1: CRUD roundtrip, atomic-save-over-existing, rename-over-
        // directory-fails. Leaves `persisted.md` behind (not deleted) so
        // phase 2 can confirm it survives an unmount + fresh remount.
        with_real_mount(open_engine(), |root| {
            assert!(
                std::fs::read_dir(root).unwrap().next().is_none(),
                "freshly mounted volume should be empty"
            );

            std::fs::write(root.join("hello.txt"), b"hello from moku vault").expect("write file");
            assert_eq!(
                std::fs::read_to_string(root.join("hello.txt")).unwrap(),
                "hello from moku vault"
            );

            std::fs::create_dir(root.join("notes")).expect("mkdir");
            std::fs::write(root.join("notes").join("a.md"), b"note content")
                .expect("write nested file");
            assert_eq!(
                std::fs::read_to_string(root.join("notes").join("a.md")).unwrap(),
                "note content"
            );

            let entries: Vec<_> = std::fs::read_dir(root)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .collect();
            assert!(entries.contains(&"hello.txt".to_string()));
            assert!(entries.contains(&"notes".to_string()));

            std::fs::rename(root.join("hello.txt"), root.join("renamed.txt")).expect("rename");
            assert!(root.join("renamed.txt").exists());
            assert!(!root.join("hello.txt").exists());

            // Editor-style atomic save: destination already exists, write a
            // sibling temp file, fsync it, then rename it over the
            // destination. Regression test for both the replace-on-rename
            // fix and the `flush` fix.
            std::fs::write(root.join("dest.md"), b"old content").expect("write dest");
            {
                use std::io::Write;
                let mut tmp = std::fs::File::create(root.join(".dest.md.tmp")).expect("create tmp");
                tmp.write_all(b"new content").expect("write tmp");
                tmp.sync_all()
                    .expect("fsync (WinFsp flush) on the mounted volume must succeed");
            }
            std::fs::rename(root.join(".dest.md.tmp"), root.join("dest.md"))
                .expect("atomic rename over an existing destination must succeed");
            assert_eq!(
                std::fs::read_to_string(root.join("dest.md")).unwrap(),
                "new content"
            );
            assert!(!root.join(".dest.md.tmp").exists());
            std::fs::remove_file(root.join("dest.md")).expect("cleanup dest.md");

            // Renaming a file over an existing directory must fail, not
            // silently clobber the directory.
            std::fs::write(root.join("movable.md"), b"x").expect("write movable");
            std::fs::create_dir(root.join("adir")).expect("mkdir adir");
            std::fs::rename(root.join("movable.md"), root.join("adir"))
                .expect_err("renaming a file over an existing directory must fail");
            std::fs::remove_file(root.join("movable.md")).expect("cleanup movable.md");
            std::fs::remove_dir(root.join("adir")).expect("cleanup adir");

            std::fs::write(root.join("persisted.md"), b"still here after remount")
                .expect("write persisted.md");

            std::fs::remove_file(root.join("renamed.txt")).expect("delete file");
            std::fs::remove_file(root.join("notes").join("a.md")).expect("delete nested file");
            std::fs::remove_dir(root.join("notes")).expect("rmdir");
        });

        // Phase 2: reopen the SAME on-disk data root (same derived keys) in
        // a fresh VolumeEngine and a fresh mount, and confirm persisted.md
        // -- written in phase 1, never deleted -- is still there with the
        // right content. Proves data survives a real unmount/remount
        // cycle, not just staying alive in one process's memory.
        with_real_mount(open_engine(), |root| {
            assert_eq!(
                std::fs::read_to_string(root.join("persisted.md")).unwrap(),
                "still here after remount",
                "data written before an unmount must still be there after a fresh remount"
            );
            std::fs::remove_file(root.join("persisted.md")).expect("cleanup persisted.md");
        });

        // Phase 3: a separate, deliberately tiny-quota volume. A real write
        // past the quota must fail with a proper OS error (not hang, not
        // silently truncate, not panic the filesystem process).
        let quota_tmp = tempfile::tempdir().expect("tempdir");
        let quota_master_key = rt.block_on(async {
            let security =
                moku_core::SecurityManager::new_with_root(quota_tmp.path().to_path_buf());
            security
                .initialize_vault(zeroize::Zeroizing::new("quota-test-password".to_string()))
                .await
                .expect("init vault")
        });
        let quota_keys = moku_volume_fs::derive_volume_keys(&quota_master_key);
        let quota_engine = VolumeEngine::open_volume(
            quota_tmp.path().join("data"),
            quota_keys,
            quota_tmp.path().join("usage.json"),
            200,
            moku_volume_fs::VolumeLock::acquire(quota_tmp.path()).expect("volume lock"),
        )
        .expect("open_volume with a tiny quota");
        with_real_mount(quota_engine, |root| {
            let result = std::fs::write(root.join("too_big.bin"), vec![0u8; 10_000]);
            assert!(
                result.is_err(),
                "a write past the volume's quota must fail, not silently succeed"
            );
        });
    }

    fn open_test_engine(root: &std::path::Path, password: &str, limit: u64) -> VolumeEngine {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let master_key = rt.block_on(async {
            let security = moku_core::SecurityManager::new_with_root(root.to_path_buf());
            security
                .initialize_vault(zeroize::Zeroizing::new(password.to_string()))
                .await
                .expect("init vault")
        });
        let keys = moku_volume_fs::derive_volume_keys(&master_key);
        let lock = moku_volume_fs::VolumeLock::acquire(root).expect("volume lock");
        VolumeEngine::open_volume(root.join("data"), keys, root.join("usage.json"), limit, lock)
            .expect("open_volume")
    }

    /// Reads a path's NTFS-style file index number the same way
    /// `GetFinalPathNameByHandleW`-adjacent identity checks do —
    /// `GetFileInformationByHandle`'s `nFileIndexHigh`/`nFileIndexLow`,
    /// which is exactly what WinFsp's `FileInfo.index_number` populates on
    /// the client side. `std::os::windows::fs::MetadataExt::file_index`
    /// would read the same value but is gated behind an unstable feature,
    /// so this calls the Win32 API directly instead.
    fn file_index(path: &std::path::Path) -> u64 {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
            OPEN_EXISTING,
        };
        use windows::core::PCWSTR;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        unsafe {
            let handle = CreateFileW(
                PCWSTR(wide.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
            .expect("CreateFileW");
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            GetFileInformationByHandle(handle, &mut info).expect("GetFileInformationByHandle");
            let _ = CloseHandle(handle);
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64
        }
    }

    /// Proves the volume's `file_id` header actually reaches Windows as a
    /// stable `index_number`: stable across repeated stats and across a
    /// rename, and different after a delete + recreate at the same name
    /// (the cache-invalidation regression case).
    #[test]
    fn test_real_mount_stable_file_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let engine = open_test_engine(tmp.path(), "file-id-test-password", 1_000_000);

        with_real_mount(engine, |root| {
            std::fs::write(root.join("a.md"), b"hello").expect("write a.md");
            let idx1 = file_index(&root.join("a.md"));
            let idx2 = file_index(&root.join("a.md"));
            assert_eq!(idx1, idx2, "file_index must be stable across repeated stats");
            assert_ne!(idx1, 0, "file_index must not be the placeholder 0");

            std::fs::rename(root.join("a.md"), root.join("b.md")).expect("rename");
            let idx_after_rename = file_index(&root.join("b.md"));
            assert_eq!(idx1, idx_after_rename, "file_index must survive a rename");

            std::fs::remove_file(root.join("b.md")).expect("delete b.md");
            std::fs::write(root.join("b.md"), b"different file").expect("recreate b.md");
            let idx_new = file_index(&root.join("b.md"));
            assert_ne!(
                idx1, idx_new,
                "a new file created at a reused name must not inherit the deleted file's id"
            );

            std::fs::create_dir(root.join("notes")).expect("mkdir notes");
            let dir_idx1 = file_index(&root.join("notes"));
            let dir_idx2 = file_index(&root.join("notes"));
            assert_eq!(dir_idx1, dir_idx2, "directory file_index must also be stable");
            assert_ne!(dir_idx1, 0);

            std::fs::remove_file(root.join("b.md")).expect("cleanup b.md");
            std::fs::remove_dir(root.join("notes")).expect("cleanup notes");
        });
    }

    /// Unit-level coverage (no mount needed) for the pending-notify queue's
    /// push/drain logic that Feature B's `NotifyingFileSystemContext` impl
    /// relies on.
    #[test]
    fn test_notify_queue_pushes_and_drains() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let engine = open_test_engine(tmp.path(), "notify-queue-test-password", 1_000_000);
        let context = VolumeFsContext { engine: Arc::new(engine), pending_notify: Mutex::new(VecDeque::new()) };

        assert_eq!(context.pending_notify_len(), 0);
        assert!(context.should_notify().is_none(), "an empty queue must not fire a notify batch");

        context.queue_notify(&VirtualPath::parse("/a.md"), FILE_NOTIFY_CHANGE_FILE_NAME.0, FILE_ACTION_ADDED.0);
        context.queue_notify(&VirtualPath::parse("/b.md"), FILE_NOTIFY_CHANGE_FILE_NAME.0, FILE_ACTION_ADDED.0);
        assert_eq!(context.pending_notify_len(), 2);

        let drained = context.should_notify().expect("a non-empty queue must produce a batch");
        assert_eq!(drained.len(), 2);
        assert_eq!(context.pending_notify_len(), 0, "should_notify must drain the queue");
        assert!(context.should_notify().is_none(), "draining twice in a row must not double-fire");
    }

    #[test]
    fn test_notify_queue_caps_and_drops_oldest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let engine = open_test_engine(tmp.path(), "notify-cap-test-password", 1_000_000);
        let context = VolumeFsContext { engine: Arc::new(engine), pending_notify: Mutex::new(VecDeque::new()) };

        for i in 0..MAX_PENDING_NOTIFY + 5 {
            context.queue_notify(
                &VirtualPath::parse(&format!("/f{i}.md")),
                FILE_NOTIFY_CHANGE_FILE_NAME.0,
                FILE_ACTION_ADDED.0,
            );
        }
        assert_eq!(
            context.pending_notify_len(),
            MAX_PENDING_NOTIFY,
            "the queue must never grow past its cap"
        );
    }
}
