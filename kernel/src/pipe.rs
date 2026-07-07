use alloc::sync::{Arc, Weak};
use alloc::string::String;
use alloc::collections::BTreeMap;
use alloc::borrow::ToOwned;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_intf::KError;
use kernel_intf::ds::RingBuffer;
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sched::{Handle, OPEN_WRITE_FLAG};
use crate::sync::Spinlock;
use crate::{fs::FileBuffer, sync::KEvent};

pub type PipeRef = Arc<Pipe, PoolAllocatorGlobal>;
type WeakPipeRef = Weak<Pipe, PoolAllocatorGlobal>;
const MAX_STREAM_CAPACITY: usize = 512;

static NAMED_PIPE_OBJECTS: Spinlock<BTreeMap<String, WeakPipeRef>> = Spinlock::new(BTreeMap::new());

pub struct Pipe {
    read_waiter: KEvent,
    write_waiter: KEvent,
    stream: Spinlock<RingBuffer<u8, MAX_STREAM_CAPACITY>>,
    writer_count: AtomicUsize,
    name: Option<String>
}

pub struct PipeType {
    is_reader: bool,
    inner: PipeRef
}

impl PipeType {
    pub fn new(pipe: PipeRef, is_reader: bool) -> Self {
        if !is_reader {
            pipe.writer_count.fetch_add(1, Ordering::AcqRel);
            crate::pipe_log!("New Writer count: {}", pipe.writer_count.load(Ordering::Acquire));
        }

        Self {
            is_reader,
            inner: pipe
        }
    }

    pub fn read(&self, buf: &FileBuffer) -> Result<usize, KError> {
        assert!(self.is_reader);
        self.inner.read(buf)
    }

    pub fn write(&self, buf: &FileBuffer) -> Result<usize, KError> {
        assert!(!self.is_reader);
        self.inner.write(buf)
    }
}

impl Clone for PipeType {
    fn clone(&self) -> Self {
        // A writer is being cloned
        // Increment writer count
        if !self.is_reader {
            self.inner.writer_count.fetch_add(1, Ordering::AcqRel);
            crate::pipe_log!("Clone Writer count: {}", self.inner.writer_count.load(Ordering::Acquire));
        }

        Self {
            is_reader: self.is_reader,
            inner: self.inner.clone()
        }
    }
}

impl Drop for PipeType {
    fn drop(&mut self) {
        if !self.is_reader {
            let old = self.inner.writer_count.fetch_sub(1, Ordering::AcqRel);
            assert!(old != 0, "pipe writer count update is wrong!");
            crate::pipe_log!("Drop Writer count: {}", self.inner.writer_count.load(Ordering::Acquire));
            // Wake any blocked readers so they can observe EOF
            if old == 1 {
                crate::pipe_log!("pipe: last writer closed, signalling readers");
                self.inner.read_waiter.signal();
            }
        }
    }
}

impl Pipe {
    pub fn new(name: Option<String>) -> PipeRef {
        Arc::new_in(
            Pipe {
                read_waiter: KEvent::new(true),
                write_waiter: KEvent::new(true),
                stream: Spinlock::new(RingBuffer::new(0)),
                writer_count: AtomicUsize::new(0),
                name
            },
            PoolAllocatorGlobal
        )
    }

    // Try to read as many bytes as possible from input stream
    // Block until data available. If block failed return error.
    // If read completed, return number of bytes read
    pub fn read(&self, buf: &FileBuffer) -> Result<usize, KError> {
        if buf.len() == 0 {
            return Ok(0);
        }

        loop {
            let mut stream = self.stream.lock();

            if stream.len() > 0 {
                let give = buf.len().min(stream.len());
                let mut stream_buf = alloc::vec![0; give];

                unsafe { stream.peek_into(stream_buf.as_mut_ptr(), give); }
                let res = buf.write(stream_buf.as_ptr().addr(), give, 0);

                if res.is_ok() {
                    stream.advance(give);
                }

                crate::pipe_log!("pipe read: gave {} bytes, {} remain in buffer", give, stream.len());

                self.write_waiter.signal();

                if stream.len() > 0 {
                    self.read_waiter.signal();
                }

                return Ok(give);
            }
            else {
                if self.writer_count.load(Ordering::Acquire) == 0 {
                    crate::pipe_log!("pipe read: no writers and buffer empty, returning EOF");
                    return Ok(0);
                }
                crate::pipe_log!("pipe read: buffer empty, blocking");
                drop(stream);
                self.read_waiter.wait(true)?;
            }
        }
    }

    pub fn write(&self, buf: &FileBuffer) -> Result<usize, KError> {
        if buf.len() == 0 {
            return Ok(0);
        }

        loop {
            let mut stream = self.stream.lock();
            if stream.len() == MAX_STREAM_CAPACITY {
                crate::pipe_log!("pipe write: buffer full, blocking");
                drop(stream);
                self.write_waiter.wait(true)?;
            }
            else {
                let get = buf.len().min(MAX_STREAM_CAPACITY - stream.len());
                let mut stream_buf = alloc::vec![0; get];
                buf.read(stream_buf.as_mut_ptr().addr(), get, 0)?;
                for ch in stream_buf {
                    stream.push(ch);
                }

                crate::pipe_log!("pipe write: wrote {} bytes, buffer now {}/{}", get, stream.len(), MAX_STREAM_CAPACITY);

                self.read_waiter.signal();

                if stream.len() < MAX_STREAM_CAPACITY {
                    self.write_waiter.signal();
                }
                return Ok(get);
            }
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            crate::pipe_log!("pipe: destroying named pipe '{}'", name);
            NAMED_PIPE_OBJECTS.lock().remove(&name);
        }
        else {
            crate::pipe_log!("pipe: destroying anonymous pipe");
        }
    }
}

pub fn create_named_pipe(name: Option<String>) -> Result<PipeRef, KError> {
    match &name {
        Some(n) => {
            let mut guard = NAMED_PIPE_OBJECTS.lock();
            if guard.contains_key(n) {
                crate::pipe_log!("pipe: create_named_pipe '{}' failed — already exists", n);
                return Err(KError::InvalidArgument);
            }
            let pipe = Pipe::new(Some(n.to_owned()));
            let pipe_weak = Arc::downgrade(&pipe);

            guard.insert(n.to_owned(), pipe_weak);
            crate::pipe_log!("pipe: created named pipe '{}'", n);
            Ok(pipe)
        },
        None => {
            crate::pipe_log!("pipe: created anonymous pipe");
            Ok(Pipe::new(None))
        }
    }
}

fn open_pipe(name: &str, flags: u64) -> Result<Handle, KError> {
    let guard = NAMED_PIPE_OBJECTS.lock();
    match guard.get(name).and_then(|w| w.upgrade()) {
        Some(inner) => {
            if flags & OPEN_WRITE_FLAG != 0 {
                crate::pipe_log!("pipe: opened '{}' for writing", name);
                Ok(Handle::PipeWriteHandle(PipeType::new(inner, false)))
            }
            else {
                crate::pipe_log!("pipe: opened '{}' for reading", name);
                Ok(Handle::PipeReadHandle(PipeType::new(inner, true)))
            }
        },
        None => {
            crate::pipe_log!("pipe: open '{}' failed — pipe not found", name);
            Err(KError::InvalidArgument)
        }
    }
}

pub fn init() {
    crate::object::register_object_type("pipe", open_pipe)
        .expect("pipe object type already registered");
}
