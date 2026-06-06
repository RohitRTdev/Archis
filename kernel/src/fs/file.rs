use alloc::sync::Arc;
use alloc::string::String;
use alloc::borrow::ToOwned;
use kernel_intf::{KError, info};
use crate::INIT_FS;
use crate::sync::Spinlock;
use kernel_intf::mem::PoolAllocatorGlobal;
use super::{FileBuffer, FilePath};

pub type FileInstance = Arc<Spinlock<FileInst>, PoolAllocatorGlobal>;

pub struct FileInst {
    file_name: String,
    offset: usize,
    total_size: usize
}

impl FileInst {
    pub fn read(&mut self, buffer: &FileBuffer) -> usize {
        let remaining = self.total_size.saturating_sub(self.offset);
        let len = remaining.min(buffer.len());
        if len == 0 {
            return 0;
        }

        let filename = resolve_symlink(self.file_name.as_str());
        let init_fs = INIT_FS.get().unwrap();
        let entry = init_fs.fs.get(filename)
        .expect("Critical error! File not found in init fs!");

        let start = unsafe {
            entry.as_ptr().add(self.offset)
        };

        buffer.write(start.addr(), len, 0);
        self.offset += len;

        len
    }

    pub fn write(&mut self, _: FileBuffer) {
        panic!("write() not supported right now!");
    }

    pub fn len(&self) -> usize {
        self.total_size
    }

    pub fn get_offset(&self) -> usize {
        self.offset
    }
    
    pub fn get_path(&self) -> FilePath<'_> {
        FilePath::from(self.file_name.as_str())
    }

    pub fn get_name(&self) -> &str {
        self.file_name.as_str()
    }
}

impl Drop for FileInst {
    fn drop(&mut self) {
        info!("Dropped file instance: {}", self.file_name);
    }
}

pub fn open(file_name: &str) -> Result<FileInstance, KError> {
    let init_fs = INIT_FS.get().unwrap();
    let filename = resolve_symlink(file_name);

    let entry = init_fs.fs.get(filename)
    .ok_or(KError::InvalidArgument).or_else(|e| {
        crate::io_log!("Failed to open file {}", file_name);
        Err(e)
    })?;
    
    let file_desc = FileInst {
        file_name: filename.to_owned(),
        offset: 0,
        total_size: entry.len()
    };
    
    let file_instance = Arc::new_in(
        Spinlock::new(
            file_desc
        ),
        PoolAllocatorGlobal
    );

    Ok(file_instance)
}

pub fn resolve_symlink(name: &str) -> &str {
    let init_fs = INIT_FS.get().unwrap();
    init_fs.symlinks.get(name).copied().unwrap_or(name)
}
