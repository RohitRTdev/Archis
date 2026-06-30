use core::cell::UnsafeCell;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;
use kernel_intf::KError;
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sync::{KSem, semaphore_guard};
use super::vfs::{DirEntry, FileAttrs, FileData, NodeKind, Vfs, VfsNodeRef};
use super::utils::FileBuffer;

pub type FileInstance = Arc<FileInst, PoolAllocatorGlobal>;

enum HandleKind { File, Dir }

pub struct FileInst {
    sem: KSem,
    path: String,
    node: VfsNodeRef,
    ancestors: Vec<VfsNodeRef>,
    kind: HandleKind,
    offset: UnsafeCell<usize>
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
        let g = self.node.lock();
        match &g.kind {
            NodeKind::File { data, .. } => data.len(),
            _ => 0
        }
    }

    pub fn fstat(&self) -> FileAttrs {
        let g = self.node.lock();
        let mut attrs = g.attrs;
        if let NodeKind::File { data, .. } = &g.kind {
            attrs.size = data.len() as u64;
        }
        attrs
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
                let g = self.node.lock();
                let data = match &g.kind {
                    NodeKind::File { data, .. } => data,
                    _ => return Err(KError::InvalidArgument)
                };
                let remaining = data.len().saturating_sub(*offset);
                let len = remaining.min(buffer.len());
                if len == 0 { return Ok(0); }
                let src = data.as_slice()[*offset..].as_ptr() as usize;
                buffer.write(src, len, 0);
                drop(g);
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
                let end = cur + len;
                let mut g = self.node.lock();
                let new_size = match &mut g.kind {
                    NodeKind::File { data, .. } => {
                        data.make_owned();
                        if let FileData::Owned(v) = data {
                            if v.len() < end { v.resize(end, 0); }
                            buffer.read(v[cur..end].as_mut_ptr() as usize, len, buf_offset);
                        }
                        data.len() as u64
                    }
                    _ => return Err(KError::InvalidArgument)
                };
                g.attrs.size = new_size;
                drop(g);
                *offset += len;
                Ok(len)
            }
        }
    }

    pub fn readdir(&self) -> Result<Vec<DirEntry>, KError> {
        match self.kind {
            HandleKind::File => Err(KError::NotADirectory),
            HandleKind::Dir => Vfs::readdir(&self.node)
        }
    }
}

impl Drop for FileInst {
    fn drop(&mut self) {
        Vfs::close(&self.node, &self.ancestors);
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
            node,
            ancestors,
            kind: if is_dir { HandleKind::Dir } else { HandleKind::File },
            offset: UnsafeCell::new(0)
        },
        PoolAllocatorGlobal
    )
}
