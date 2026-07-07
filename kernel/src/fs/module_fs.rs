use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;

use common::{MemoryRegion, StrRef};
use kernel_intf::{E_SUCCESS, KError};
use kernel_intf::driver::DeviceObject;
use kernel_intf::fs::{FS_ENTRY_DIR, FS_ENTRY_SYMLINK, FsDirEntry, FsFileStat};
use kernel_intf::mem::PoolAllocatorGlobal;

use crate::io::{DeviceHandleK, OpenDeviceHandle, open_device};
use crate::loader::{LoadedImage, load_image};
use crate::sync::Spinlock;

use super::utils::FileBuffer;
use super::vfs::{DirEntry, EntryType, FileAttrs, ProbeStep};

const FS_MODULE_PATHS: &[&str] = &["/sys/libfat32.so"];

type FnIdentifyFs = extern "C" fn(*const DeviceObject) -> bool;
type FnFsOpen = extern "C" fn(*const DeviceObject, StrRef, bool, *mut usize, *mut bool, MemoryRegion, *mut usize, MemoryRegion, *mut usize) -> i64;
type FnFsClose = extern "C" fn(*const DeviceObject, usize) -> i64;
type FnFsRead = extern "C" fn(*const DeviceObject, usize, MemoryRegion, usize, *mut usize) -> i64;
type FnFsWrite = extern "C" fn(*const DeviceObject, usize, MemoryRegion, usize, usize, usize, *mut usize) -> i64;
type FnFsStat = extern "C" fn(*const DeviceObject, StrRef, bool, *mut FsFileStat, MemoryRegion, *mut usize, MemoryRegion, *mut usize) -> i64;
type FnFsCreateFile = extern "C" fn(*const DeviceObject, StrRef, u16) -> i64;
type FnFsMkdir = extern "C" fn(*const DeviceObject, StrRef, u16) -> i64;
type FnFsCreateSymlink = extern "C" fn(*const DeviceObject, StrRef, StrRef) -> i64;
type FnFsDelete = extern "C" fn(*const DeviceObject, StrRef) -> i64;
type FnFsRename = extern "C" fn(*const DeviceObject, StrRef, StrRef) -> i64;
type FnFsReaddir = extern "C" fn(*const DeviceObject, usize, usize, *mut FsDirEntry) -> i64;
type FnFsUnmount = extern "C" fn(*const DeviceObject) -> i64;

pub struct ModuleBackedFs {
    _image: LoadedImage,
    dev: OpenDeviceHandle,
    fs_open: FnFsOpen,
    fs_close: FnFsClose,
    fs_read: FnFsRead,
    fs_write: FnFsWrite,
    fs_stat: FnFsStat,
    fs_create_file: FnFsCreateFile,
    fs_mkdir: FnFsMkdir,
    fs_create_symlink: FnFsCreateSymlink,
    fs_delete: FnFsDelete,
    fs_rename: FnFsRename,
    fs_readdir: FnFsReaddir,
    fs_unmount: FnFsUnmount,
    open_paths: Spinlock<BTreeMap<String, usize>>,
    handle_paths: Spinlock<BTreeMap<usize, String>>
}

impl ModuleBackedFs {
    // Walks the list of filesystems we support and checks which is present
    // within this disk/partition device
    pub fn identify_and_open(dev: DeviceHandleK) -> Result<Arc<ModuleBackedFs, PoolAllocatorGlobal>, KError> {
        let dev_ptr = dev.device_ptr();

        for path in FS_MODULE_PATHS {
            let image = match load_image(path, false) {
                Ok(i) => i,
                Err(e) => { crate::fs_log!("identify_and_open: load_image({}) failed: {:?}", path, e); continue; }
            };
            let identify_addr = match image.lock().load_symbol("identify_fs") {
                Some(a) => a,
                None => { crate::fs_log!("identify_and_open: {} has no identify_fs export", path); continue; }
            };
            let identify: FnIdentifyFs = unsafe { core::mem::transmute(identify_addr) };
            if !identify(dev_ptr) {
                crate::fs_log!("identify_and_open: {} did not identify the device", path);
                continue;
            }

            let resolve = |name: &str| -> Result<usize, KError> {
                image.lock().load_symbol(name).ok_or(KError::Unsupported)
            };
            let fs_open = resolve("fs_open")?;
            let fs_close = resolve("fs_close")?;
            let fs_read = resolve("fs_read")?;
            let fs_write = resolve("fs_write")?;
            let fs_stat = resolve("fs_stat")?;
            let fs_create_file = resolve("fs_create_file")?;
            let fs_mkdir = resolve("fs_mkdir")?;
            let fs_create_symlink = resolve("fs_create_symlink")?;
            let fs_delete = resolve("fs_delete")?;
            let fs_rename = resolve("fs_rename")?;
            let fs_readdir = resolve("fs_readdir")?;
            let fs_unmount = resolve("fs_unmount")?;

            let open_handle = open_device(dev)?;

            let fs = ModuleBackedFs {
                _image: image,
                dev: open_handle,
                fs_open: unsafe { core::mem::transmute(fs_open) },
                fs_close: unsafe { core::mem::transmute(fs_close) },
                fs_read: unsafe { core::mem::transmute(fs_read) },
                fs_write: unsafe { core::mem::transmute(fs_write) },
                fs_stat: unsafe { core::mem::transmute(fs_stat) },
                fs_create_file: unsafe { core::mem::transmute(fs_create_file) },
                fs_mkdir: unsafe { core::mem::transmute(fs_mkdir) },
                fs_create_symlink: unsafe { core::mem::transmute(fs_create_symlink) },
                fs_delete: unsafe { core::mem::transmute(fs_delete) },
                fs_rename: unsafe { core::mem::transmute(fs_rename) },
                fs_readdir: unsafe { core::mem::transmute(fs_readdir) },
                fs_unmount: unsafe { core::mem::transmute(fs_unmount) },
                open_paths: Spinlock::new(BTreeMap::new()),
                handle_paths: Spinlock::new(BTreeMap::new())
            };
            return Ok(Arc::new_in(fs, PoolAllocatorGlobal));
        }

        Err(KError::Unsupported)
    }

    pub(crate) fn dev_ptr(&self) -> *const DeviceObject {
        self.dev.device_ptr()
    }

    pub fn is_busy(&self) -> bool {
        !self.open_paths.lock().is_empty()
    }

    pub fn unmount(&self) {
        let _ = (self.fs_unmount)(self.dev_ptr());
    }

    pub fn is_path_open(&self, path: &str) -> bool {
        self.open_paths.lock().contains_key(path)
    }

    fn strip_trailing_components(path: &str, remaining: &str) -> String {
        let comps: alloc::vec::Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let remaining_count = remaining.split('/').filter(|s| !s.is_empty()).count();
        let keep = comps.len().saturating_sub(remaining_count + 1);
        let mut out = String::new();
        for c in &comps[..keep] {
            out.push('/');
            out.push_str(c);
        }
        if out.is_empty() { "/".to_string() } else { out }
    }

    pub fn probe(&self, path: &str, follow_final: bool) -> Result<ProbeStep, KError> {
        let mut out = FsFileStat { size: 0, mode: 0 };
        let mut sym_buf = [0u8; 256];
        let mut sym_len = 0usize;
        let mut rem_buf = [0u8; 256];
        let mut rem_len = 0usize;
        let sym_region = MemoryRegion { base_address: sym_buf.as_mut_ptr() as usize, size: sym_buf.len() };
        let rem_region = MemoryRegion { base_address: rem_buf.as_mut_ptr() as usize, size: rem_buf.len() };

        let code = (self.fs_stat)(
            self.dev_ptr(), StrRef::from_str(path), follow_final,
            &mut out, sym_region, &mut sym_len, rem_region, &mut rem_len
        );

        if code == E_SUCCESS {
            let attrs = FileAttrs { mode: out.mode, size: out.size };
            let symlink_target = if sym_len > 0 {
                core::str::from_utf8(&sym_buf[..sym_len]).ok().map(|s| s.to_string())
            } else {
                None
            };
            return Ok(ProbeStep::Found { attrs, symlink_target });
        }
        let err = KError::from(code);
        if err == KError::IsSymlink {
            let target = core::str::from_utf8(&sym_buf[..sym_len]).map_err(|_| KError::InvalidArgument)?.to_string();
            let remaining = core::str::from_utf8(&rem_buf[..rem_len]).map_err(|_| KError::InvalidArgument)?.to_string();
            let dir = Self::strip_trailing_components(path, &remaining);
            crate::fs_log!("fat32 backend: probe path={} hit symlink dir={} target={} remaining={}", path, dir, target, remaining);
            return Ok(ProbeStep::Symlink { dir, target, remaining });
        }
        Err(err)
    }

    // Open an already fully-resolved path.
    pub fn open_at(&self, path: &str) -> Result<(usize, bool), KError> {
        let mut handle = 0usize;
        let mut is_dir = false;
        let mut sym_buf = [0u8; 256];
        let mut sym_len = 0usize;
        let mut rem_buf = [0u8; 256];
        let mut rem_len = 0usize;
        let sym_region = MemoryRegion { base_address: sym_buf.as_mut_ptr() as usize, size: sym_buf.len() };
        let rem_region = MemoryRegion { base_address: rem_buf.as_mut_ptr() as usize, size: rem_buf.len() };

        let code = (self.fs_open)(
            self.dev_ptr(), StrRef::from_str(path), true,
            &mut handle, &mut is_dir, sym_region, &mut sym_len, rem_region, &mut rem_len
        );
        if code != E_SUCCESS {
            return Err(KError::from(code));
        }

        self.handle_paths.lock().insert(handle, path.to_string());
        *self.open_paths.lock().entry(path.to_string()).or_insert(0) += 1;
        Ok((handle, is_dir))
    }

    pub fn close(&self, handle: usize) {
        let _ = (self.fs_close)(self.dev_ptr(), handle);
        let Some(path) = self.handle_paths.lock().remove(&handle) else { return };
        let mut open_paths = self.open_paths.lock();
        if let Some(count) = open_paths.get_mut(&path) {
            *count -= 1;
            if *count == 0 { open_paths.remove(&path); }
        }
    }

    pub fn read(&self, handle: usize, buffer: &FileBuffer, offset: usize) -> Result<usize, KError> {
        let mut kbuf = vec![0u8; buffer.len()];
        let region = MemoryRegion { base_address: kbuf.as_mut_ptr() as usize, size: kbuf.len() };
        let mut out_len = 0usize;
        let code = (self.fs_read)(self.dev_ptr(), handle, region, offset, &mut out_len);
        if code != E_SUCCESS {
            return Err(KError::from(code));
        }
        buffer.write(kbuf.as_ptr() as usize, out_len, 0)?;
        Ok(out_len)
    }

    pub fn write(&self, handle: usize, buffer: &FileBuffer, len: usize, buf_offset: usize, file_offset: usize) -> Result<usize, KError> {
        let mut kbuf = vec![0u8; len];
        buffer.read(kbuf.as_mut_ptr() as usize, len, buf_offset)?;
        let region = MemoryRegion { base_address: kbuf.as_ptr() as usize, size: kbuf.len() };
        let mut out_len = 0usize;
        let code = (self.fs_write)(self.dev_ptr(), handle, region, len, 0, file_offset, &mut out_len);
        if code != E_SUCCESS {
            return Err(KError::from(code));
        }
        Ok(out_len)
    }

    pub fn create_file(&self, path: &str, mode: u16) -> Result<(), KError> {
        let code = (self.fs_create_file)(self.dev_ptr(), StrRef::from_str(path), mode);
        if code == E_SUCCESS { Ok(()) } else { Err(KError::from(code)) }
    }

    pub fn create_dir(&self, path: &str, mode: u16) -> Result<(), KError> {
        let code = (self.fs_mkdir)(self.dev_ptr(), StrRef::from_str(path), mode);
        if code == E_SUCCESS { Ok(()) } else { Err(KError::from(code)) }
    }

    pub fn create_symlink(&self, path: &str, target: &str) -> Result<(), KError> {
        let code = (self.fs_create_symlink)(self.dev_ptr(), StrRef::from_str(path), StrRef::from_str(target));
        if code == E_SUCCESS { Ok(()) } else { Err(KError::from(code)) }
    }

    pub fn delete(&self, path: &str) -> Result<(), KError> {
        let code = (self.fs_delete)(self.dev_ptr(), StrRef::from_str(path));
        if code == E_SUCCESS { Ok(()) } else { Err(KError::from(code)) }
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), KError> {
        let code = (self.fs_rename)(self.dev_ptr(), StrRef::from_str(from), StrRef::from_str(to));
        if code == E_SUCCESS { Ok(()) } else { Err(KError::from(code)) }
    }

    pub fn readdir(&self, handle: usize, offset: usize) -> Result<DirEntry, KError> {
        let mut fs_entry = FsDirEntry::empty();
        let code = (self.fs_readdir)(self.dev_ptr(), handle, offset, &mut fs_entry);
        if code != E_SUCCESS {
            return Err(KError::from(code));
        }
        let name = String::from_utf8_lossy(&fs_entry.name[..fs_entry.name_len]).into_owned();
        let _kind = if fs_entry.kind == FS_ENTRY_DIR {
            EntryType::Dir
        } else if fs_entry.kind == FS_ENTRY_SYMLINK {
            EntryType::Symlink
        } else {
            EntryType::File
        };
        let _symlink_target = if fs_entry.kind == FS_ENTRY_SYMLINK {
            Some(String::from_utf8_lossy(&fs_entry.target[..fs_entry.target_len]).into_owned())
        } else {
            None
        };
        Ok(DirEntry {
            name,
            _kind,
            _attrs: FileAttrs { mode: fs_entry.mode, size: fs_entry.size },
            _symlink_target
        })
    }
}
