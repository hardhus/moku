//! WinFsp `FileSystemContext` implementation over `moku-vault-fs`'s
//! `VolumeEngine`. Only the operations a real read/write filesystem needs
//! are implemented; everything else keeps the trait's default
//! `STATUS_INVALID_DEVICE_REQUEST` (ACL get/set, reparse points, named
//! streams, extended attributes — none of which the vault format
//! supports, matching moku's existing no-ACL, no-EA storage model).

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL, STATUS_END_OF_FILE, STATUS_INVALID_HANDLE,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNSUCCESSFUL,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use winfsp::U16CStr;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};

use moku_vault_fs::{Attr, DirEntry, FileKind, VaultFsError, VirtualPath, VolumeEngine};

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

fn map_err(e: VaultFsError) -> winfsp::FspError {
    match e {
        VaultFsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND.into(),
        VaultFsError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION.into(),
        VaultFsError::NotADirectory => STATUS_NOT_A_DIRECTORY.into(),
        VaultFsError::IsADirectory => STATUS_NOT_A_DIRECTORY.into(),
        VaultFsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY.into(),
        VaultFsError::NameTooLong => STATUS_OBJECT_NAME_INVALID.into(),
        VaultFsError::QuotaExceeded => STATUS_DISK_FULL.into(),
        VaultFsError::BadFileHandle => STATUS_INVALID_HANDLE.into(),
        VaultFsError::Other(_) => STATUS_UNSUCCESSFUL.into(),
    }
}

fn write_file_info(dst: &mut FileInfo, attr: &Attr) {
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
    dst.index_number = 0;
    dst.hard_links = 0;
    dst.ea_size = 0;
}

/// One open file or directory handle. Directories carry no engine file
/// handle (`fh: None`) — `VolumeEngine`'s directory operations are
/// stateless and always take a path, so there's nothing to keep open.
pub struct VaultFileHandle {
    path: VirtualPath,
    fh: Option<u64>,
}

pub struct VaultFsContext {
    engine: Arc<VolumeEngine>,
}

impl FileSystemContext for VaultFsContext {
    type FileContext = VaultFileHandle;

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
        // No ACL support (matches the vault format's no-metadata-beyond-
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
        write_file_info(file_info.as_mut(), &attr);
        Ok(VaultFileHandle { path, fh })
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
            write_file_info(file_info.as_mut(), &attr);
            Ok(VaultFileHandle { path, fh: None })
        } else {
            let (fh, attr) = self.engine.create(&parent, &name).map_err(map_err)?;
            write_file_info(file_info.as_mut(), &attr);
            Ok(VaultFileHandle { path, fh: Some(fh) })
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
            let _ = self.engine.unlink(&parent, name);
        } else {
            let _ = self.engine.rmdir(&parent, name);
        }
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self.engine.getattr(&context.path).map_err(map_err)?;
        write_file_info(file_info, &attr);
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
        // The vault format tracks only size and on-disk mtime — custom
        // attribute/timestamp writes are accepted but not persisted
        // (v1 scope cut, matches moku-vault-fs's Attr shape).
        let attr = self.engine.getattr(&context.path).map_err(map_err)?;
        write_file_info(file_info, &attr);
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
        write_file_info(file_info, &attr);
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
        write_file_info(file_info, &attr);
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
                write_file_info(file_info, &attr_before);
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
        write_file_info(file_info, &attr_after);
        Ok(n as u32)
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
            write_file_info(dir_info.file_info_mut(), &self_attr);
            dir_info.set_name_raw([b'.' as u16].as_slice())?;
            if !dir_info.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }

            let parent_attr = context
                .path
                .parent()
                .and_then(|p| self.engine.getattr(&p).ok())
                .unwrap_or_else(|| self_attr.clone());
            dir_info.reset();
            write_file_info(dir_info.file_info_mut(), &parent_attr);
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
            write_file_info(dir_info.file_info_mut(), &attr);
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

        if replace_if_exists
            && self
                .engine
                .getattr(&new_path)
                .map(|a| a.kind == FileKind::File)
                .unwrap_or(false)
        {
            let _ = self.engine.unlink(&new_parent, &new_name);
        }

        self.engine
            .rename(&old_parent, &old_name, &new_parent, &new_name)
            .map_err(map_err)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let total = self.engine.size_limit_bytes();
        let used = self.engine.usage_bytes();
        out_volume_info.total_size = total;
        out_volume_info.free_size = total.saturating_sub(used);
        out_volume_info.set_volume_label("Moku Vault");
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
    engine: VolumeEngine,
    mountpoint: &str,
    stop_rx: Receiver<()>,
    on_mounted: impl FnOnce(),
) -> Result<()> {
    winfsp::winfsp_init()
        .map_err(|e| anyhow!("WinFsp initialization failed: {e:?} (is WinFsp installed?)"))?;

    let mut volume_params = VolumeParams::new();
    volume_params
        .filesystem_name("MokuVault")
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .read_only_volume(false)
        .flush_and_purge_on_cleanup(true);

    let engine = Arc::new(engine);
    let context = VaultFsContext {
        engine: Arc::clone(&engine),
    };
    let params = FileSystemParams::default_params(volume_params);
    let mut host: FileSystemHost<VaultFsContext> =
        FileSystemHost::new_with_options(params, context)
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
    // `context` (and its Arc<VolumeEngine> clone) drops with `host` above;
    // this outer clone is still valid, so the final usage count — which
    // only updates in memory as writes happen — actually reaches disk.
    let _ = engine.flush_usage();
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

    /// End-to-end proof that a real encrypted volume actually mounts as a
    /// Windows drive and behaves like a normal filesystem: create, read,
    /// list, nested dirs, rename, delete, and a clean unmount that leaves
    /// no stuck drive behind. Requires WinFsp installed and a free drive
    /// letter, so it's `#[ignore]`d by default — run explicitly with
    /// `cargo test -p moku-vault-mount -- --ignored` to verify.
    #[test]
    #[ignore = "requires WinFsp installed and a free drive letter"]
    fn test_real_mount_full_crud_roundtrip() {
        let mountpoint =
            find_free_drive_letter().expect("no free drive letter available for the test");
        let volume_tmp = tempfile::tempdir().expect("tempdir");

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let engine = rt.block_on(async {
            let security =
                moku_core::SecurityManager::new_with_root(volume_tmp.path().to_path_buf());
            let master_key = security
                .initialize_vault(zeroize::Zeroizing::new("smoke-test-password".to_string()))
                .await
                .expect("init vault");
            let keys = moku_vault_fs::derive_volume_keys(&master_key);
            VolumeEngine::open_volume(
                volume_tmp.path().join("data"),
                keys,
                volume_tmp.path().join("usage.json"),
                50_000_000,
            )
            .expect("open_volume")
        });

        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let (mounted_tx, mounted_rx) = std::sync::mpsc::channel();
        let mount_mountpoint = mountpoint.clone();
        let mount_thread = std::thread::spawn(move || {
            mount_and_wait(engine, &mount_mountpoint, stop_rx, move || {
                let _ = mounted_tx.send(());
            })
        });

        mounted_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("on_mounted should fire once the WinFsp mount actually succeeds");

        let root = std::path::Path::new(&mountpoint).to_path_buf();
        let result = std::panic::catch_unwind(|| {
            assert!(
                std::fs::read_dir(&root).unwrap().next().is_none(),
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

            let entries: Vec<_> = std::fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .collect();
            assert!(entries.contains(&"hello.txt".to_string()));
            assert!(entries.contains(&"notes".to_string()));

            std::fs::rename(root.join("hello.txt"), root.join("renamed.txt")).expect("rename");
            assert!(root.join("renamed.txt").exists());
            assert!(!root.join("hello.txt").exists());

            std::fs::remove_file(root.join("renamed.txt")).expect("delete file");
            std::fs::remove_file(root.join("notes").join("a.md")).expect("delete nested file");
            std::fs::remove_dir(root.join("notes")).expect("rmdir");
        });

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
}
