use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use common::{MemoryRegion, PAGE_SIZE, StrRef};
use kernel_intf::{KError, info, debug};
use kernel_intf::list::{List, DynList};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::fs::FileInstance;
use crate::loader::{LoadedImage, LoadedImageWeak, load_image};
use crate::loader::module::UserModule;
use crate::{KERNEL_PATH, hal};
use crate::mem::{self, PageDescriptor, VCB, VirtMemConBlk, deallocate_memory, get_physical_address};
use crate::sched::{self, *};
use crate::sync::{KEvent, Spinlock};
use crate::io::DeviceHandleK;
use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use core::ptr::NonNull;
use core::alloc::Layout;

static PROCESS_ID: AtomicUsize = AtomicUsize::new(0);
static PROCESSES: Spinlock<BTreeMap<usize, KProcess>> = Spinlock::new(BTreeMap::new());

pub type KProcess = Arc<Spinlock<Process>, PoolAllocatorGlobal>;

pub enum Handle {
    FileHandle(FileInstance),
    ImgHandle(LoadedImage),
    DeviceHandle(DeviceHandleK)
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessStatus {
    Ready,
    Terminated
}

pub struct Process {
    id: usize,
    // In current design, we will have the process struct holding weak pointers to the tasks.
    // Intuitively it should be the other way around, however this way it makes it easier code wise.
    // When tasks are dropped, the process struct will be automatically dropped
    threads: DynList<usize>,
    addr_space: VCB,
    status: ProcessStatus,
    is_user: bool,
    term_notify: KEvent,
    init_notify: KEvent,
    init_status: bool,

    file_table: Vec<Option<Handle>>,

    // Per-process registry of user modules mapped into this process's address space
    user_modules: Vec<LoadedImageWeak>,

    // List of physical addresses that are deallocated once the process is killed
    // The virtual address space would be destroyed prior to this deallocation
    memory_list: DynList<MemoryRegion>,
    args: Vec<String>,
    exit_code: AtomicIsize
}

unsafe impl Send for Process {}

impl Process {
    fn new(clone_addr_space: bool, is_user: bool, args: Vec<String>) -> Result<KProcess, KError> {
        let id = PROCESS_ID.fetch_add(1, Ordering::Relaxed);
        let kernel_addr_space = mem::get_kernel_addr_space();
        let new_addr_space = if clone_addr_space {
            VirtMemConBlk::clone(kernel_addr_space, id)?
        }
        else {
            kernel_addr_space
        };

        let proc = Arc::new_in(Spinlock::new(Self {
            id,
            threads: List::new(),
            addr_space: new_addr_space,
            status: ProcessStatus::Ready,
            is_user,
            term_notify: KEvent::new(false),
            init_notify: KEvent::new(false),
            init_status: false,
            memory_list: List::new(),
            file_table: Vec::new(),
            user_modules: Vec::new(),
            args,
            exit_code: AtomicIsize::new(0)
        }), PoolAllocatorGlobal);

        crate::sched_log!("Creating new process with id {}", id);

        Ok(proc)
    }

    pub fn get_args(&self) -> &[String] {
        &self.args
    }

    pub fn get_exit_code(&self) -> isize {
        self.exit_code.load(Ordering::Relaxed)
    }

    pub fn get_vcb(&self) -> VCB {
        self.addr_space
    }

    pub fn get_user_flag(&self) -> bool {
        self.is_user
    }

    pub fn get_status(&self) -> ProcessStatus {
        self.status
    }

    pub fn get_id(&self) -> usize {
        self.id
    }

    pub fn attach_thread_to_current_process(&mut self, thread_id: usize) -> Result<(), KError> {
        self.threads.add_node(thread_id)
    }

    pub fn remove_thread(&mut self, thread_id: usize, exit_code: isize) -> bool {
        let mut killed_thread = None;
        for node in self.threads.iter() {
            if **node == thread_id {
                killed_thread = Some(NonNull::from(node));
                break;
            }
        }

        crate::sched_log!("Remove thread called with id {}", thread_id);

        unsafe {
            if let Some(killed_thread) = killed_thread {
                self.threads.remove_node(killed_thread);
            }
        }
        
        if self.status == ProcessStatus::Terminated {
            return self.threads.get_nodes() == 0;
        }

        // This happens when all the threads in the process via 
        // individual kill_thread calls instead of calling
        // kill_process directly
        if self.threads.get_nodes() == 0 {
            self.exit_code.store(exit_code, Ordering::Relaxed);
            self.destroy_process();
            return true;
        }

        false
    }

    pub fn get_notify_sem(&self) -> KEvent {
        self.term_notify.clone()
    }
    
    pub fn get_init_sem(&self) -> KEvent {
        self.init_notify.clone()
    }

    pub fn complete_init(&mut self, status: bool) {
        self.init_status = status;
        self.init_notify.signal();
    }

    fn destroy_process(&mut self) {
        self.status = ProcessStatus::Terminated;
        PROCESSES.lock().remove(&self.id);
        crate::sched_log!("Called destroy process {}", self.id);
    }

    pub fn get_num_handles(&self) -> usize {
        self.file_table.len()
    }

    // Get all non-stale references of loadedImage from 
    // per-process user module registry
    pub fn get_user_modules(&self) -> Vec<LoadedImage> {
        self.user_modules.iter()
        .map(|p| {p.upgrade()})
        .flatten()
        .collect()
    }

    pub fn register_user_module(&mut self, module: &LoadedImage) {
        self.user_modules.push(Arc::downgrade(module));
    }

#[cfg(debug_assertions)]
    pub fn print_handles(&self) {
        let mut file_handles = 0;
        let mut img_handles = 0;
        let mut device_handles = 0;
        self.file_table.iter().for_each(|handle| {
            match handle.as_ref() {
                Some(h) => {
                    match h {
                        Handle::FileHandle(_) => {
                            file_handles += 1;
                        },
                        Handle::ImgHandle(_) => {
                            img_handles += 1;
                        },
                        Handle::DeviceHandle(_) => {
                            device_handles += 1;
                        }
                    }
                },
                _ => {}
            }
        });

        debug!("proc_id = {}, File handles = {}, image handles = {}, device handles = {}", self.id, file_handles, img_handles, device_handles);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        crate::sched_log!("Dropping process {}", self.id);
        unsafe {
            VirtMemConBlk::destroy_address_space(self.addr_space);
        }

        crate::sched_log!("Deallocating regions from process {} memory list", self.id);
        for range in self.memory_list.iter() {
            debug!("Deallocating memory region base={:#X} of size={}", range.base_address, range.size);
            deallocate_memory(range.base_address as *mut u8, Layout::from_size_align(range.size, PAGE_SIZE).unwrap(), 0)
            .expect("Failed to deallocate physical memory from process");
        }
    }
}

pub fn init() {
    // Create init process and attach init task (task id = 0) to it
    let init_proc = Process::new(false, false, vec!(KERNEL_PATH.into()))
    .expect("Failed to create init process");

    PROCESSES.lock().insert(0, Arc::clone(&init_proc));

    let mut proc = init_proc.lock();
    proc.status = ProcessStatus::Ready;
    proc.threads.add_node(0).expect("Init process allocation failed!");

    info!("Created init process 0");
}

pub fn get_current_process() -> Option<KProcess> {
    let task = get_current_task()?;
    let guard = task.lock();
    guard.get_process()
}

pub fn get_current_process_id() -> Option<usize> {
    Some(get_current_process()?.lock().get_id())
}

pub fn get_process_info(proc_id: usize) -> Option<KProcess> {
    let proc_map = PROCESSES.lock();

    proc_map.get(&proc_id).map(|item| {
        Arc::clone(item)
    })
}

extern "C" fn kernel_init_handler() -> ! {
    let path = {
        let proc = get_current_process().expect("kernel_init_handler: no current process");
        let guard = proc.lock();
        guard.args[0].clone()
    };

    let img = match load_image(&path, false) {
        Ok(img) => img,
        Err(e) => {
            info!("kernel_init_handler: failed to load image '{}': {:?}", path, e);
            exit_process(-1);
        }
    };

    let entry = img.lock().kernel().info.entry;
    add_new_handle(Handle::ImgHandle(img));

    let entry_fn: DispatchRoutine = unsafe { core::mem::transmute(entry) };
    
    // Call the user entry function
    entry_fn()
}

pub fn get_current_process_args() -> *const Vec<String> {
    let proc = match get_current_process() {
        Some(p) => p,
        None => return core::ptr::null(),
    };
    let guard = proc.lock();
    
    // The Arc keeps the Process alive as long as any thread runs within it,
    // so this pointer remains valid for the caller's lifetime on the current thread.
    &guard.args as *const Vec<String>
}

pub fn create_process(args: Vec<String>, context_ptr: *mut c_void, is_user: bool) -> Result<KProcess, KError> {
    // args[0] must name the module to load (kernel and user alike)
    if args.is_empty() {
        return Err(KError::InvalidArgument);
    }

    disable_preemption();

    let process = match Process::new(true, is_user, args) {
        Ok(p) => p,
        Err(e) => {
            enable_preemption();
            return Err(e);
        }
    };

    let init_notify_sem = process.lock().init_notify.clone();

    let init_handler: DispatchRoutine = if is_user {
        super::user::user_init_handler
    } else {
        kernel_init_handler
    };

    let thread = match sched::create_init_thread(init_handler, Arc::clone(&process), context_ptr) {
        Ok(t) => t,
        Err(e) => {
            enable_preemption();
            return Err(e);
        }
    };

    let core = thread.lock().get_core();

    if let Err(e) = start_task(&thread, core, &process, &PROCESSES) {
        enable_preemption();
        return Err(e);
    }

    enable_preemption();

    if is_user {
        init_notify_sem.wait();
        if !process.lock().init_status {
            return Err(KError::ProcessInitFailed);
        }
    }

    Ok(process)
}

pub fn kill_process(proc_id: usize, exit_code: isize) {
    let proc = get_process_info(proc_id);
    if proc.is_none() {
        return;
    }

    assert!(proc_id != 0, "Attempted to kill system process!");

    let proc = proc.unwrap();
    let cur_task_id = get_current_task_id();

    disable_preemption();

    // We clone the list here in order to release the process lock
    let threads = {
        let mut guard = proc.lock();
        if guard.status != ProcessStatus::Ready {
            drop(guard);
            enable_preemption();
            return;
        }

        guard.status = ProcessStatus::Terminated;
        guard.exit_code.store(exit_code, Ordering::Relaxed);
        guard.threads.clone()
    };

    crate::sched_log!("Killing process {}", proc_id);

    let is_idle_task = cur_task_id.is_none();
    let cur_task_id = if cur_task_id.is_some() {cur_task_id.unwrap()} else {0};
    let mut is_exit = false; 

    // Kill all the tasks within the process
    // We won't remove the task nodes here, the task will call remove_thread when it's about to be removed from scheduler queue
    for thread_id in threads.iter() {
        // We don't want the current task to kill itself right away
        // This happens if the current process is killing itself (exit)
        if is_idle_task || **thread_id != cur_task_id {
            crate::sched_log!("Issuing kill to thread {}", **thread_id);
            sched::kill_thread(**thread_id, exit_code);
        }
        else {
            is_exit = true;
        }
    }

    proc.lock().destroy_process();

    enable_preemption();

    // Kill the current thread last
    if is_exit {
        // Drop it explicitly since we are not going to return from this call
        drop(proc);
        sched::kill_thread(cur_task_id, exit_code);
    }
}

/* Important to ensure that no locks are held or that preemption is not disabled during this call */
pub fn exit_process(exit_code: isize) -> ! {
    assert!(super::is_preemption_enabled());
    let proc_id = get_current_process_id().expect("Attempted to kill idle process!!");

    kill_process(proc_id, exit_code);

    // We could land here. Suppose two thread of a process call exit_process.
    // Only one of them succeeds in acquiring lock and setting status to terminate.
    // At this point, the other thread, would simply return from kill_process.
    // So we wait here. This is the right thing to do, as this thread would just get killed

    hal::sleep();
}

pub fn add_memory_range_to_cur_process(virtual_base: usize, size: usize, is_user: bool) {
    let flags = if is_user {PageDescriptor::USER} else {0};
    let base_address = get_physical_address(virtual_base, flags)
    .expect("Unable to find physical address for given virtual address from add_memory_range_to_cur_process!");
    
    let range = MemoryRegion {base_address, size};

    let process = get_current_process()
    .expect("Called add_memory_range_to_cur_process() from idle task!");

    crate::sched_log!("Adding memory range with virtual_base:{:#X}, phy_base:{:#X} and size {}", virtual_base,
    base_address, size);

    process.lock().memory_list.add_node(range)
    .expect("Failed to add node to process memory list!");
}

pub fn add_new_handle(handle: Handle) -> usize {
    let proc = get_current_process()
    .expect("add_new_handle() called in idle task!");

    let mut guard = proc.lock();

    // If we have free entry in table, then use that
    for fd in 0..guard.file_table.len() {
        if guard.file_table[fd].is_none() {
            guard.file_table[fd] = Some(handle);
            return fd;
        }
    }

    // Otherwise, allocate new entry
    guard.file_table.push(Some(handle));

    guard.file_table.len() - 1
}

#[unsafe(no_mangle)]
extern "C" fn sched_exit_process_ffi(exit_code: isize) -> ! {
    exit_process(exit_code)
}

#[unsafe(no_mangle)]
extern "C" fn sched_get_num_process_args_ffi() -> usize {
    get_current_process().expect("get_num_process_args() called in idle process!").lock().args.len()
}

#[unsafe(no_mangle)]
extern "C" fn sched_get_cur_process_arg_ffi(num: usize) -> StrRef {
    let proc = get_current_process().expect("get_cur_process_arg() called in idle process!");
    let guard = proc.lock();

    assert!(num < guard.args.len());

    // This is fine since the argument location is valid for the lifetime of this process
    StrRef::from_str(guard.args[num].as_str())
}

#[unsafe(no_mangle)]
extern "C" fn sched_create_process_ffi(
    args: *const StrRef,
    args_len: usize,
    context_ptr: *mut c_void,
) -> usize {
    let args_vec: Vec<String> = unsafe {
        core::slice::from_raw_parts(args, args_len)
            .iter()
            .map(|s| String::from(s.as_str()))
            .collect()
    };
    match create_process(args_vec, context_ptr, false) {
        Ok(proc) => proc.lock().get_id(),
        Err(_) => usize::MAX,
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_wait_process_ffi(proc_id: usize) {
    if let Some(proc) = get_process_info(proc_id) {
        let _ = proc.wait();
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_kill_process_ffi(proc_id: usize, exit_code: isize) {
    kill_process(proc_id, exit_code);
}

impl Spinlock<Process> {
    // Blocks caller until process terminates
    pub fn wait(&self) -> bool {
        let sem = {
            let task = self.lock();
            task.term_notify.clone() 
        };

        sem.wait()
    }
}