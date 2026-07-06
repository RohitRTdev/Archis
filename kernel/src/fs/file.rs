use core::cell::UnsafeCell;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;
use kernel_intf::KError;
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sync::{KSem, semaphore_guard};
use super::module_fs::ModuleBackedFs;
use super::vfs::{DirEntry, FileAttrs, FileData, MODE_DIR, MODE_FILE, NodeKind, Vfs, VfsNodeRef};
use super::utils::FileBuffer;

pub type FileInstance = Arc<FileInst, PoolAllocatorGlobal>;

enum HandleKind { File, Dir }

enum Backing {
    Memory { node: VfsNodeRef, ancestors: Vec<VfsNodeRef> },
    Module { backend: Arc<ModuleBackedFs, PoolAllocatorGlobal>, handle: usize }
}

pub struct FileInst {
    sem: KSem,
    path: String,
    backing: Backing,
    kind: HandleKind,
    offset: UnsafeCell<usize>,
    // Module-backed file size cache, refreshed on open and after writes that
    // extend the file. Unused (always 0) for Memory backing, which always
    // reads size live from the node instead.
    mod_size: UnsafeCell<u64>
}

unsafe impl Send for FileInst {}
unsafe impl Sync for FileInst {}

impl FileInst {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, HandleKind::Dir)
    }

    pub fn get_path(&self) -> &str {
        &self.path
    }

    pub fn get_offset(&self) -> usize {
        unsafe { *self.offset.get() }
    }

    pub fn len(&self) -> usize {
        match &self.backing {
            Backing::Memory { node, .. } => {
                let g = node.lock();
                match &g.kind {
                    NodeKind::File { data, .. } => data.len(),
                    _ => 0
                }
            }
            Backing::Module { .. } => unsafe { *self.mod_size.get() as usize }
        }
    }

    pub fn fstat(&self) -> FileAttrs {
        match &self.backing {
            Backing::Memory { node, .. } => {
                let g = node.lock();
                let mut attrs = g.attrs;
                attrs.size = match &g.kind {
                    NodeKind::File {data, .. } => {
                        data.len() as u64
                    },
                    NodeKind::Dir {children, .. } => {
                        children.len() as u64
                    },
                    NodeKind::Symlink { target } => {
                        target.len() as u64
                    }
                };

                attrs
            }
            Backing::Module { .. } => {
                let mode = if self.is_dir() { MODE_DIR } else { MODE_FILE };
                FileAttrs { mode, size: unsafe { *self.mod_size.get() } }
            }
        }
    }

    pub fn seek(&self, new_offset: usize) -> Result<(), KError> {
        match self.kind {
            HandleKind::Dir => Err(KError::IsADirectory),
            HandleKind::File => {
                let _guard = semaphore_guard(&self.sem);
                unsafe { *self.offset.get() = new_offset; }
                Ok(())
            }
        }
    }

    pub fn read(&self, buffer: &FileBuffer) -> Result<usize, KError> {
        match self.kind {
            HandleKind::Dir => Err(KError::IsADirectory),
            HandleKind::File => {
                let _guard = semaphore_guard(&self.sem);
                let offset = unsafe { &mut *self.offset.get() };
                let len = match &self.backing {
                    Backing::Memory { node, .. } => {
                        let g = node.lock();
                        let data = match &g.kind {
                            NodeKind::File { data, .. } => data,
                            _ => return Err(KError::InvalidArgument)
                        };
                        let remaining = data.len().saturating_sub(*offset);
                        let len = remaining.min(buffer.len());
                        if len == 0 { return Ok(0); }
                        let src = data.as_slice()[*offset..].as_ptr() as usize;
                        buffer.write(src, len, 0)?;
                        len
                    }
                    Backing::Module { backend, handle } => backend.read(*handle, buffer, *offset)?
                };
                *offset += len;
                Ok(len)
            }
        }
    }

    pub fn write(&self, buffer: &FileBuffer, len: usize, buf_offset: usize) -> Result<usize, KError> {
        match self.kind {
            HandleKind::Dir => Err(KError::IsADirectory),
            HandleKind::File => {
                let _guard = semaphore_guard(&self.sem);
                let offset = unsafe { &mut *self.offset.get() };
                let cur = *offset;
                match &self.backing {
                    Backing::Memory { node, .. } => {
                        let end = cur + len;
                        let mut g = node.lock();
                        let new_size = match &mut g.kind {
                            NodeKind::File { data, .. } => {
                                data.make_owned();
                                if let FileData::Owned(v) = data {
                                    if v.len() < end { v.resize(end, 0); }
                                    buffer.read(v[cur..end].as_mut_ptr() as usize, len, buf_offset)?;
                                }
                                data.len() as u64
                            }
                            _ => return Err(KError::InvalidArgument)
                        };
                        g.attrs.size = new_size;
                    }
                    Backing::Module { backend, handle } => {
                        let written = backend.write(*handle, buffer, len, buf_offset, cur)?;
                        let end = (cur + written) as u64;
                        unsafe {
                            let cur_size = &mut *self.mod_size.get();
                            if end > *cur_size { *cur_size = end; }
                        }
                    }
                }
                *offset += len;
                Ok(len)
            }
        }
    }

    pub fn readdir_at(&self, offset: usize) -> Result<DirEntry, KError> {
        match self.kind {
            HandleKind::File => Err(KError::NotADirectory),
            HandleKind::Dir => match &self.backing {
                Backing::Memory { node, ancestors } => Vfs::readdir(node, ancestors, offset),
                Backing::Module { backend, handle } => backend.readdir(*handle, offset)
            }
        }
    }
}

impl Drop for FileInst {
    fn drop(&mut self) {
        match &self.backing {
            Backing::Memory { node, ancestors } => Vfs::close(node, ancestors),
            Backing::Module { backend, handle } => backend.close(*handle)
        }
        crate::loader_log!("Closed handle: {}", self.path);
    }
}

pub(super) fn make_handle(
    path: String,
    node: VfsNodeRef,
    ancestors: Vec<VfsNodeRef>,
    is_dir: bool
) -> FileInstance {
    Arc::new_in(
        FileInst {
            sem: KSem::new(1, 1),
            path,
            backing: Backing::Memory { node, ancestors },
            kind: if is_dir { HandleKind::Dir } else { HandleKind::File },
            offset: UnsafeCell::new(0),
            mod_size: UnsafeCell::new(0)
        },
        PoolAllocatorGlobal
    )
}

pub(super) fn make_module_handle(
    path: String,
    backend: Arc<ModuleBackedFs, PoolAllocatorGlobal>,
    handle: usize,
    is_dir: bool,
    size: u64
) -> FileInstance {
    Arc::new_in(
        FileInst {
            sem: KSem::new(1, 1),
            path,
            backing: Backing::Module { backend, handle },
            kind: if is_dir { HandleKind::Dir } else { HandleKind::File },
            offset: UnsafeCell::new(0),
            mod_size: UnsafeCell::new(size)
        },
        PoolAllocatorGlobal
    )
}
