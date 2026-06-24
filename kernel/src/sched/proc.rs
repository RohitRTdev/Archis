use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use alloc::borrow::ToOwned;
use common::{MemoryRegion, StrRef};
use kernel_intf::{KError, info, debug};
use kernel_intf::list::{List, DynList};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::fs::FileInstance;
use crate::loader::{LoadedImage, LoadedImageWeak, load_image};
use crate::{KERNEL_PATH, hal};
use crate::mem::{self, PageDescriptor, VCB, VirtMemConBlk, get_physical_address};
use crate::sched::{self, *};
use crate::sync::{KEvent, KSemInnerType, Spinlock};
use crate::io::OpenDeviceHandle;
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

static PROCESS_ID: AtomicUsize = AtomicUsize::new(0);
static PROCESSES: Spinlock<BTreeMap<usize, KProcess>> = Spinlock::new(BTreeMap::new());

pub type KProcess = Arc<Spinlock<Process>, PoolAllocatorGlobal>;

#[derive(Clone)]
pub enum Handle {
    FileHandle(FileInstance),
    ImgHandle(LoadedImage),
    DeviceHandle(OpenDeviceHandle),
    ThreadHandle(KThread),
    ProcessHandle(KProcess),
    SyncHandle(KSemInnerType)
}

pub struct HandleType {
    handle_info: Handle,
    is_inheritable: bool
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessStatus {
    Ready,
    Suspended,
    Terminated
}

#[derive(Clone, Copy)]
pub struct SignalHandler {
    pub user_ctx: *mut c_void,
    pub handler: DispatchRoutine
}

pub struct Process {
    id: usize,
    pid: usize,
    session: KSession,
    pgroup: KProcessGroup,
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

    handle_table: Vec<Option<HandleType>>,

    // Per-process registry of user modules mapped into this process's address space
    user_modules: Vec<LoadedImageWeak>,

    // List of physical addresses that are deallocated once the process is killed
    // The virtual address space would be destroyed prior to this deallocation
    memory_list: DynList<MemoryRegion>,
    signal_handlers: [Option<SignalHandler>; MAX_SIGNALS],
    pending_signals: u8,
    signal_guard: Arc<Spinlock<bool>>,
    args: Vec<String>,
    exit_code: AtomicIsize
}

unsafe impl Send for Process {}

#[repr(C)]
pub struct ProcessInfo {
    pub id: usize,
    pub pid: usize,
    pub sid: usize,
    pub exit_code: isize
}

#[repr(C)]
pub struct ThreadInfo {
    pub id: u64,
    pub exit_code: i64
}

impl Process {
    fn new(
        clone_addr_space: bool,
        is_user: bool,
        is_suspended: bool,
        pid: usize,
        session: KSession,
        pgroup: KProcessGroup,
        args: Vec<String>,
        handle_table: Vec<Option<HandleType>>
    ) -> Result<KProcess, KError> {
        let id = PROCESS_ID.fetch_add(1, Ordering::Relaxed);
        let kernel_addr_space = mem::get_kernel_addr_space();
        let new_addr_space = if clone_addr_space {
            VirtMemConBlk::clone(kernel_addr_space, id)?
        }
        else {
            kernel_addr_space
        };

        let status = if is_suspended {
            ProcessStatus::Suspended
        }
        else {
            ProcessStatus::Ready
        };

        let proc = Arc::new_in(Spinlock::new(Self {
            id,
            pid,
            session,
            pgroup,
            threads: List::new(),
            addr_space: new_addr_space,
            status,
            is_user,
            term_notify: KEvent::new(false),
            init_notify: KEvent::new(false),
            init_status: false,
            memory_list: List::new(),
            handle_table,
            user_modules: Vec::new(),
            signal_handlers: [None; MAX_SIGNALS],
            pending_signals: 0,
            signal_guard: Arc::new(Spinlock::new(true)),
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

    pub fn get_num_threads(&self) -> usize {
        self.threads.get_nodes()
    }

    pub fn get_signal_handler(&self, signal: u8) -> Option<SignalHandler> {
        assert!((signal as usize) < self.signal_handlers.len());
        self.signal_handlers[signal as usize]
    }

    pub fn set_signal_handler(&mut self, signal: u8, handler: SignalHandler) {
        assert!((signal as usize) < self.signal_handlers.len());
        self.signal_handlers[signal as usize] = Some(handler);
    }

    pub fn get_threads_snapshot(&self) -> Vec<usize> {
        self.threads.iter().map(|t| {**t}).collect()
    }

    pub fn attach_thread_to_current_process(&mut self, thread_id: usize) -> Result<(), KError> {
        self.threads.add_node(thread_id)
    }

    // Returns cleanup_work if this was last thread that removed itself from this process
    pub fn remove_thread(&mut self, thread_id: usize, exit_code: isize) -> Option<ProcessCleanupWork> {
        self.threads.find_and_remove(|t| {*t == thread_id});
        crate::sched_log!("Remove thread called with id {}", thread_id);

        // Run the cleanup for this process once the last thread dies 
        if self.threads.get_nodes() == 0 {
            Some(self.destroy_process(exit_code))
        }
        else {
            None
        }
    }

    pub fn get_first_task(&self) -> Option<usize> {
        // This could be none, since there is a small window
        // where a process is killed, but the process struct
        // is still in registry and no threads exist
        Some(**self.threads.first()?)
    }

    pub fn get_notify_sem(&self) -> KEvent {
        self.term_notify.clone()
    }
    
    pub fn get_init_sem(&self) -> KEvent {
        self.init_notify.clone()
    }

    pub fn set_status(&mut self, status: ProcessStatus) {
        self.status = status;
    }

    pub fn complete_init(&mut self, status: bool) {
        self.init_status = status;
        self.init_notify.signal();
    }

    pub fn get_process_header(&self) -> ProcessInfo {
        ProcessInfo {
            id: self.id,
            pid: self.pid,
            sid: self.session.lock().sid,
            exit_code: self.get_exit_code()
        }
    }

    fn destroy_process(&mut self, exit_code: isize) -> ProcessCleanupWork {
        // If kill/exit_process wasn't called, the
        // exit code will be same as exit code passed
        // to the last killed thread
        if self.status != ProcessStatus::Terminated {
            self.exit_code.store(exit_code, Ordering::Relaxed);
            self.status = ProcessStatus::Terminated;
        }
        PROCESSES.lock().remove(&self.id);

        {
            let mut sess = self.session.lock();
            sess.processes.find_and_remove(|&p| p == self.id);
            if sess.leader == Some(self.id) {
                sess.leader = None;
            }
        }
        {
            let mut pg = self.pgroup.lock();
            pg.processes.find_and_remove(|&p| p == self.id);
        }
        
        // This function is called in critical section
        // We want the cleanup code to execute in lock free code
        // So we submit to dedicated reaper system thread
        let work = ProcessCleanupWork::new(
            self.id,
            self.addr_space,
            core::mem::take(&mut self.memory_list),
            core::mem::take(&mut self.handle_table)
        );

        // Its fine to clear this here since these are just weak pointers
        // It will only decrement the weak ref count and not run any destructor code
        self.user_modules.clear();
        crate::sched_log!("Called destroy process {}", self.id);

        // We will tell scheduler to enqueue this work item (no deadlock risk)
        work
    }

    pub fn get_num_handles(&self) -> usize {
        self.handle_table.len()
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

    // This operation is only allowed by the parent process
    // and the process must be in the same session as parent,
    // or must be done by the same process.
    fn set_session_leader(&mut self, id: usize) -> bool {
        assert!(self.id != 0, "Attempted to change session id for system process!");

        if self.session.lock().leader == Some(self.id) {
            return false;
        }

        let allowed = if id == self.id {
            true
        } else {
            let res = get_process_info(self.pid);
            if id != self.pid || res.is_none() {
                return false;
            }
            let parent = res.unwrap();
            let parent_guard = parent.lock();
            Arc::ptr_eq(&parent_guard.session, &self.session)
        };

        if allowed {
            self.session.lock().processes.find_and_remove(|&p| p == self.id);
            let new_sess = Session::new(self.id);
            new_sess.lock().processes.add_node(self.id).expect("set_session_leader: add to new session failed");
            new_sess.lock().leader = Some(self.id);
            self.session = new_sess;
        }

        allowed
    }

    fn set_pgroup_leader(&mut self, id: usize) -> bool {
        assert!(self.id != 0, "Attempted to change pgroup for system process!");

        // This process is already part of a different pgrp
        if self.pgroup.lock().pgid == self.id {
            return false;
        }

        // Only allowed if same process or parent process belonging to same process group
        let allowed = if id == self.id {
            true
        } else {
            let res = get_process_info(self.pid);
            if id != self.pid || res.is_none() {
                return false;
            }
            let parent = res.unwrap();
            let parent_guard = parent.lock();
            Arc::ptr_eq(&parent_guard.pgroup, &self.pgroup)
        };

        if allowed {
            self.pgroup.lock().processes.find_and_remove(|&p| p == self.id);
            let new_pg = ProcessGroup::new(self.id);
            new_pg.lock().processes.add_node(self.id).expect("set_pgroup_leader: add to new pgroup failed");
            self.pgroup = new_pg;
        }

        allowed
    }

#[cfg(debug_assertions)]
    pub fn print_handles(&self) {
        let mut file_handles = 0;
        let mut img_handles = 0;
        let mut device_handles = 0;
        let mut misc_handles = 0;
        self.handle_table.iter().for_each(|handle| {
            match handle.as_ref() {
                Some(h) => {
                    match h.handle_info {
                        Handle::FileHandle(_) => {
                            file_handles += 1;
                        },
                        Handle::ImgHandle(_) => {
                            img_handles += 1;
                        },
                        Handle::DeviceHandle(_) => {
                            device_handles += 1;
                        },
                        _ => {
                            misc_handles += 1;
                        }
                    }
                },
                _ => {}
            }
        });

        debug!("proc_id = {}, 
        File handles = {},
        image handles = {}, 
        device handles = {},
        misc handles = {}", self.id, file_handles, img_handles, device_handles, misc_handles);
    }

    pub(super) fn get_pending_signals(&self) -> u8 {
        self.pending_signals
    }
    
    pub(super) fn set_pending_signals(&mut self, pending_signal_mask: u8) {
        self.pending_signals = pending_signal_mask
    }

    pub(super) fn get_signal_guard(&self) -> Arc<Spinlock<bool>> {
        self.signal_guard.clone()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        crate::sched_log!("Dropping process {}", self.id);
    }
}

pub fn init() {
    let sess = Session::new(0);
    let pgrp = ProcessGroup::new(0);
    sess.lock().processes.add_node(0).expect("init: session add failed");
    pgrp.lock().processes.add_node(0).expect("init: pgroup add failed");

    // Create init process and attach init task (task id = 0) to it
    let init_proc = Process::new(
        false,
        false,
        false,
        0,
        sess,
        pgrp,
        vec!(KERNEL_PATH.into()),
        Vec::new())
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

pub fn set_session_leader(pid: usize) -> bool {
    let cur_proc_id = get_current_process_id().expect("set_sid() called from idle process!");
    let res = get_process_info(pid);

    if res.is_none() {
        return false;
    }

    let this_proc = res.unwrap();
    this_proc.lock().set_session_leader(cur_proc_id)
}

pub fn set_pgroup_leader(pid: usize) -> bool {
    let cur_proc_id = get_current_process_id().expect("set_pgroup_leader() called from idle process!");
    let res = get_process_info(pid);

    if res.is_none() {
        return false;
    }

    let this_proc = res.unwrap();
    this_proc.lock().set_pgroup_leader(cur_proc_id)
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
    add_new_handle(Handle::ImgHandle(img), false);

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

pub fn create_process<T: AsRef<str>>(args: &[T], context_ptr: *mut c_void, is_user: bool, is_suspended: bool) -> Result<KProcess, KError> {
    // args[0] must name the module to load (kernel and user alike)
    if args.is_empty() {
        return Err(KError::InvalidArgument);
    }

    let (pid, session, pgroup, inherited_table) = {
        let proc = get_current_process().expect("create_process() called from idle process!");
        let guard = proc.lock();
        let table: Vec<Option<HandleType>> = guard.handle_table.iter().map(|entry| {
            entry.as_ref().and_then(|h| {
                if h.is_inheritable {
                    Some(HandleType { handle_info: h.handle_info.clone(), is_inheritable: true })
                } else {
                    None
                }
            })
        }).collect();
        (guard.id, Arc::clone(&guard.session), Arc::clone(&guard.pgroup), table)
    };

    disable_preemption();

    let args = args.iter()
        .map(|s| s.as_ref().to_owned())
        .collect();

    let process = match Process::new(
        true,
        is_user,
        is_suspended,
        pid,
        Arc::clone(&session),
        Arc::clone(&pgroup),
        args,
        inherited_table) {
        Ok(p) => p,
        Err(e) => {
            enable_preemption();
            return Err(e);
        }
    };

    let new_id = process.lock().get_id();
    session.lock().processes.add_node(new_id).expect("create_process: session add failed");
    pgroup.lock().processes.add_node(new_id).expect("create_process: pgroup add failed");

    let init_notify_sem = process.lock().init_notify.clone();

    let init_handler: DispatchRoutine = if is_user {
        super::user::user_init_handler
    } else {
        kernel_init_handler
    };

    let thread = match sched::create_init_thread(
        init_handler, 
        Arc::clone(&process), context_ptr) {
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
        let _ = init_notify_sem.wait(false);
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
        if guard.status == ProcessStatus::Terminated {
            drop(guard);
            enable_preemption();
            return;
        }

        guard.status = ProcessStatus::Terminated;
        guard.exit_code.store(exit_code, Ordering::Relaxed);
        guard.threads.clone()
    };

    drop(proc);
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

    enable_preemption();

    // Kill the current thread last
    if is_exit {
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

pub fn remove_memory_range_from_cur_process(virtual_base: usize, size: usize, is_user: bool) {
    let flags = if is_user {PageDescriptor::USER} else {0};
    let base_address = get_physical_address(virtual_base, flags)
    .expect("Unable to find physical address for given virtual address from remove_memory_range_from_cur_process!");
    
    let process = get_current_process()
    .expect("Called remove_memory_range_from_cur_process() from idle task!");

    crate::sched_log!("Removing memory range with virtual_base:{:#X}, phy_base:{:#X} and size {}", virtual_base,
    base_address, size);
    
    process.lock().memory_list.find_and_remove(|t| {
        t.base_address == base_address && t.size == size
    })
    .expect("Failed to remove node from process memory list!");
}

pub fn get_handle(handle: usize) -> Option<Handle> {
    let proc = get_current_process()
    .expect("get_handle() called in idle task!");

    let guard = proc.lock();
    if guard.handle_table.len() > handle {
        guard.handle_table[handle].as_ref().map(|t| {
            (*t).handle_info.clone()
        })
    }
    else {
        None
    }
}

pub fn remove_handle(handle: usize) -> bool {
    let proc = get_current_process()
    .expect("remove_handle() called in idle task!");

    let mut guard = proc.lock();

    if guard.handle_table.len() > handle && guard.handle_table[handle].is_some() {
        guard.handle_table[handle] = None;
        return true;
    }

    false
}

pub fn add_new_handle(handle: Handle, is_inheritable: bool) -> usize {
    let handle_info = HandleType { handle_info: handle, is_inheritable };
    let proc = get_current_process()
    .expect("add_new_handle() called in idle task!");

    let mut guard = proc.lock();

    for idx in 0..guard.handle_table.len() {
        if guard.handle_table[idx].is_none() {
            guard.handle_table[idx] = Some(handle_info);
            return idx;
        }
    }

    guard.handle_table.push(Some(handle_info));

    guard.handle_table.len() - 1
}

pub fn add_handle_to_proc(proc: &KProcess, handle: Handle, is_inheritable: bool) -> usize {
    let handle_info = HandleType { handle_info: handle, is_inheritable };
    let mut guard = proc.lock();

    for idx in 0..guard.handle_table.len() {
        if guard.handle_table[idx].is_none() {
            guard.handle_table[idx] = Some(handle_info);
            return idx;
        }
    }

    guard.handle_table.push(Some(handle_info));
    guard.handle_table.len() - 1
}

pub fn place_handle_in_proc(proc: &KProcess, index: usize, handle: Handle, is_inheritable: bool) {
    let mut guard = proc.lock();
    
    // The index might be beyond the current allocated vector length
    // So pad the locations inbetween with None
    while guard.handle_table.len() <= index {
        guard.handle_table.push(None);
    }
    guard.handle_table[index] = Some(HandleType { handle_info: handle, is_inheritable });
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
    context_ptr: *mut c_void
) -> usize {
    let args_vec: Vec<String> = unsafe {
        core::slice::from_raw_parts(args, args_len)
            .iter()
            .map(|s| String::from(s.as_str()))
            .collect()
    };
    match create_process(args_vec.as_slice(), context_ptr, false, false) {
        Ok(proc) => proc.lock().get_id(),
        Err(_) => usize::MAX,
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_wait_process_ffi(proc_id: usize) {
    if let Some(proc) = get_process_info(proc_id) {
        let _ = proc.wait(false);
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_kill_process_ffi(proc_id: usize, exit_code: isize) {
    kill_process(proc_id, exit_code);
}

pub fn issue_pgrp(pgrp: &KProcessGroup, signal: u8) {
    let pids: Vec<usize> = pgrp.lock().processes.iter().map(|p| **p).collect();
    for pid in pids {
        issue_signal(pid, signal);
    }
}

#[unsafe(no_mangle)]
extern "C" fn proc_get_session_ffi(pid: usize) -> usize {
    match get_process_info(pid) {
        None => 0,
        Some(p) => {
            let session = Arc::clone(&p.lock().session);
            let ptr = Arc::as_ptr(&session);
            core::mem::forget(session);
            ptr as usize
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn proc_drop_session_ffi(val: usize) {
    if val == 0 { return; }
    unsafe { drop(Arc::from_raw_in(val as *const Spinlock<Session>, PoolAllocatorGlobal)) };
}

#[unsafe(no_mangle)]
extern "C" fn proc_is_session_active_ffi(val: usize) -> bool {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw_in(val as *const Spinlock<Session>, PoolAllocatorGlobal) });
    arc.lock().processes.get_nodes() > 0
}

#[unsafe(no_mangle)]
extern "C" fn proc_is_session_leader_ffi(pid: usize, val: usize) -> bool {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw_in(val as *const Spinlock<Session>, PoolAllocatorGlobal) });
    arc.lock().leader == Some(pid)
}

#[unsafe(no_mangle)]
extern "C" fn proc_get_pgrp_ffi(pid: usize) -> usize {
    match get_process_info(pid) {
        None => 0,
        Some(p) => {
            let pgroup = Arc::clone(&p.lock().pgroup);
            let ptr = Arc::as_ptr(&pgroup);
            core::mem::forget(pgroup);
            ptr as usize
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn proc_drop_pgrp_ffi(val: usize) {
    if val == 0 { return; }
    unsafe { drop(Arc::from_raw_in(val as *const Spinlock<ProcessGroup>, PoolAllocatorGlobal)) };
}

#[unsafe(no_mangle)]
extern "C" fn proc_is_pgrp_active_ffi(val: usize) -> bool {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw_in(val as *const Spinlock<ProcessGroup>, PoolAllocatorGlobal) });
    arc.lock().processes.get_nodes() > 0
}

#[unsafe(no_mangle)]
extern "C" fn proc_is_foreground_pgrp_ffi(pid: usize, val: usize) -> bool {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw_in(val as *const Spinlock<ProcessGroup>, PoolAllocatorGlobal) });
    arc.lock().processes.iter().any(|p| **p == pid)
}

#[unsafe(no_mangle)]
extern "C" fn proc_issue_signal_ffi(pid: usize, signal: u8) {
    issue_signal(pid, signal);
}

#[unsafe(no_mangle)]
extern "C" fn proc_issue_pgrp_ffi(val: usize, signal: u8) {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw_in(val as *const Spinlock<ProcessGroup>, PoolAllocatorGlobal) });
    issue_pgrp(&arc, signal);
}

impl Spinlock<Process> {
    // Blocks caller until process terminates
    pub fn wait(&self, is_interruptible: bool) -> bool {
        let sem = {
            let task = self.lock();
            task.term_notify.clone() 
        };

        sem.wait(is_interruptible).is_ok()
    }

    pub(super) fn get_inner_sem(&self) -> KSemInnerType {
        self.lock().get_notify_sem().inner()
    }
}