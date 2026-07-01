use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use common::{MemoryRegion, PAGE_SIZE, align_down, get_highest_set_bit};
use core::ptr::{null_mut, NonNull};
use core::mem::take;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicIsize, AtomicUsize, Ordering};
use core::ffi::c_void;
use super::{DispatchRoutine, KProcess, ProcessStatus, get_current_process, get_process_info};
use crate::cpu::{self, MAX_CPUS, PerCpu, Stack, get_panic_base, get_total_cores, get_worker_stack, set_panic_base};
use crate::hal::{self, *};
use crate::mem::*;
use crate::sched::{ProcessCleanupWork, SignalHandler, enqueue_cleanup, exit_process, kill_process};
use crate::sync::{KEvent, KSem, KSemInnerType, Spinlock};
use crate::io::{self, DeviceState, IrpPtr, deallocate_irp, get_device};
use kernel_intf::mem::{PoolAllocator, PoolAllocatorGlobal};
use kernel_intf::list::{DynList, List, ListNode, ListNodeGuard};
use kernel_intf::driver::{DeviceObject, Irp, IrpMajor, IrpMinor, IrpResult, Status};
use kernel_intf::{acquire_spinlock, release_spinlock};
use kernel_intf::{KError, debug, info};

#[cfg(target_arch = "x86_64")]
use crate::hal::{get_per_cpu_kernel_base_for_core, set_tss_stack};

// This is in milliseconds
pub const QUANTUM: usize = 10;
const INIT_QUANTA: usize = 10;
pub const MAX_SIGNALS: usize = 6;

pub type KThread = Arc<Spinlock<Task>, PoolAllocatorGlobal>;

static TASK_ID: AtomicUsize = AtomicUsize::new(0);
static TASK_CPU: AtomicU8 = AtomicU8::new(0);
static TASKS: Spinlock<BTreeMap<usize, KThread>> = Spinlock::new(BTreeMap::new());

const _: () = {
    assert!(u8::MAX as usize + 1 >= MAX_CPUS);
};

#[derive(PartialEq, Clone, Copy)]
pub enum SignalCause {
    Normal,
    TimerExpiry,
    Interruption
}


#[derive(Clone, Copy)]
struct SignalFrame {
    signal: u8,
    is_kernel_mode: bool,
    is_in_syscall: bool,
#[cfg(target_arch = "x86_64")]
    syscall_gs: u64,
#[cfg(target_arch = "x86_64")]
    syscall_rsp: u64,
    handler: DispatchRoutine,
    user_ctx: *mut c_void,
    context: usize,
    mapped_base: usize,
    kernel_stack_base: usize,
    last_signal_cause: SignalCause,
#[cfg(target_arch="x86_64")]
    per_cpu_base: u64
}

impl SignalFrame {
    fn new(signal: u8, user_ctx: *mut c_void, handler: DispatchRoutine) -> Self {
        Self {
            signal,
            is_kernel_mode: false,
            is_in_syscall: false,
        #[cfg(target_arch = "x86_64")]
            syscall_gs: 0,
        #[cfg(target_arch = "x86_64")]
            syscall_rsp: 0,
            handler,
            user_ctx,
            context: 0,
            mapped_base: 0,
            kernel_stack_base: 0,
            last_signal_cause: SignalCause::Normal,
            #[cfg(target_arch = "x86_64")]
            per_cpu_base: 0
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TaskStatus {
    Running,
    Active,
    Waiting,
    WaitingInterruptible,
    Terminated,
    Suspended
}

pub struct Task {
    id: usize,
    is_kernel_mode: bool,
    is_in_syscall: bool,
#[cfg(target_arch = "x86_64")]
    syscall_gs: u64,
#[cfg(target_arch = "x86_64")]
    syscall_rsp: u64,
    core: usize,
    stack: Option<Stack>,
    user_stack: Option<Stack>,
    status: TaskStatus,
    context: usize,
    quanta: usize,
    panic_base: usize,
    user_fn: Option<DispatchRoutine>,
    last_signal_cause: SignalCause,
    wait_semaphores: DynList<KSemInnerType>,
    issued_irps: DynList<IrpPtr>,
    in_signal_init: u8,
    term_notify: KEvent,
    process: Option<KProcess>,
    vcb: Option<VCB>,
    arg_context: *mut c_void,
    exit_code: AtomicIsize,
    pending_signals: u8,
    completed_signals: u8,
    signal_frame_pending: [Option<SignalFrame>; MAX_SIGNALS], 
    signal_frame_init: [Option<SignalFrame>; MAX_SIGNALS],
#[cfg(target_arch="x86_64")]
    per_cpu_base: u64
}

impl Task {
    fn new(alloc_stack: bool, core: usize, user_fn: Option<DispatchRoutine>) -> Result<KThread, KError> {
        let stack  = if alloc_stack {
            Some(Stack::new()?)
        } else {
            None
        };
        let id = TASK_ID.fetch_add(1, Ordering::Relaxed);  

        if alloc_stack {
            crate::sched_log!("Creating task with ID:{} and stack_addr={:#X} on core {}", id, stack.as_ref().unwrap().get_stack_base(), core);
        } 
        else {
            crate::sched_log!("Creating task with ID:{}", id);
        }

        let task = Arc::new_in(Spinlock::new(Task {
            id,
            is_kernel_mode: true,
            is_in_syscall: false,
            #[cfg(target_arch = "x86_64")]
            syscall_gs: 0,
            #[cfg(target_arch = "x86_64")]
            syscall_rsp: 0,
            core,
            stack,
            user_stack: None,
            status: TaskStatus::Active,
            context: 0,
            quanta: INIT_QUANTA,
            panic_base: 0,
            user_fn,
            wait_semaphores: List::new(),
            issued_irps: List::new(),
            last_signal_cause: SignalCause::Normal,
            term_notify: KEvent::new(false),
            in_signal_init: 0,
            process: None,
            vcb: None,
            arg_context: null_mut(),
            exit_code: AtomicIsize::new(0),
            pending_signals: 0,
            completed_signals: 0,
            signal_frame_pending: [None; MAX_SIGNALS],
            signal_frame_init: [None; MAX_SIGNALS],
            #[cfg(target_arch = "x86_64")]
            per_cpu_base: get_per_cpu_kernel_base_for_core(core)
        }), PoolAllocatorGlobal);

        Ok(task)
    }

    pub fn get_id(&self) -> usize {
        self.id
    }

    pub fn is_user_thread(&self) -> bool {
        self.user_fn.is_some()
    }

    pub fn get_user_fn(&self) -> Option<DispatchRoutine> {
        self.user_fn
    }

    pub fn get_arg_context(&self) -> *mut c_void {
        self.arg_context
    }

    pub fn get_status(&self) -> TaskStatus {
        self.status
    }
    
    pub fn get_signal_cause(&self) -> SignalCause {
        let signal = get_highest_set_bit(self.pending_signals);
        if signal == -1 {
            self.last_signal_cause
        }
        else {
            let sig_frame = self.signal_frame_pending[signal as usize].as_ref().expect("Failed to get sig_frame despite pending signal set");
            sig_frame.last_signal_cause
        }
    }
    
    pub fn set_signal_cause(&mut self, cause: SignalCause) {
        let signal = get_highest_set_bit(self.pending_signals);
        if signal == -1 {
            self.last_signal_cause = cause;
        }
        else {
            let sig_frame = self.signal_frame_pending[signal as usize].as_mut().expect("Failed to get sig_frame despite pending signal set");
            sig_frame.last_signal_cause = cause;
        }
    }
    
    pub fn get_core(&self) -> usize {
        self.core
    }

    pub fn get_stack(&self) -> Option<usize> {
        let signal = get_highest_set_bit(self.pending_signals);
        if signal == -1 {
            Some(self.stack.as_ref()?.get_stack_base())
        }
        else {
            let sig_frame = self.signal_frame_pending[signal as usize].as_ref().expect("Failed to get sig_frame despite pending signal set");
            // For kernel mode handlers, this value is not set
            // However its still fine since we don't call this in kernel mode
            Some(sig_frame.kernel_stack_base)
        }
    }

    pub fn get_process(&self) -> Option<KProcess> {
        if let Some(proc) = &self.process {
            Some(Arc::clone(proc))
        }
        else {
            None
        }
    }

    pub fn get_exit_code(&self) -> isize {
        self.exit_code.load(Ordering::Relaxed)
    }

    fn register_irp(&mut self, irp: IrpPtr) {
        self.issued_irps.add_node(irp).expect("Failed to register IRP with task");
    }

    fn find_and_remove_irp(&mut self, irp: *mut Irp) {
        self.issued_irps.find_and_remove(|&p| p == irp);
    }

    fn take_irp_list(&mut self) -> DynList<IrpPtr> {
        take(&mut self.issued_irps)
    }

    fn is_in_syscall(&self) -> bool {
        let signal = get_highest_set_bit(self.pending_signals);
        if signal == -1 {
            self.is_in_syscall
        }
        else {
            let sig_frame = self.signal_frame_pending[signal as usize].as_ref().expect("Failed to get sig_frame despite pending signal set");
            sig_frame.is_in_syscall
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn syscall_gs(&self) -> u64 {
        let signal = get_highest_set_bit(self.pending_signals);
        if signal == -1 {
            self.syscall_gs
        }
        else {
            let sig_frame = self.signal_frame_pending[signal as usize].as_ref().expect("Failed to get sig_frame despite pending signal set");
            sig_frame.syscall_gs
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn syscall_rsp(&self) -> u64 {
        let signal = get_highest_set_bit(self.pending_signals);
        if signal == -1 {
            self.syscall_rsp
        }
        else {
            let sig_frame = self.signal_frame_pending[signal as usize].as_ref().expect("Failed to get sig_frame despite pending signal set");
            sig_frame.syscall_rsp
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        crate::sched_log!("Dropping task:{}", self.id);
        assert!(self.wait_semaphores.get_nodes() == 0);
        assert!(self.issued_irps.get_nodes() == 0, "task {} dropped with outstanding IRPs", self.id);
    }
}

unsafe impl Send for Task {}

enum CompletionCtxType {
    Async((extern "C" fn(*const IrpResult, *mut c_void), *mut c_void)),
    Sync((KEvent, *mut IrpResult))
}

pub struct AsyncCtx {
    irp: IrpPtr,
    comp_type: CompletionCtxType
}

struct SchedulerCB {
    preemption_count: AtomicUsize,
    is_dw_mode: AtomicBool,
    dw_queue: Spinlock<DynList<DriverWorker>>,
    task_queue: Spinlock<TaskQueue>
}

pub type DwRoutine = extern "C" fn(*mut c_void);

#[derive(Clone, Copy)]
pub struct DriverWorker {
    routine: DwRoutine,
    context: *mut c_void,
}

unsafe impl Send for DriverWorker {}

struct TimerInfo {
    timer_count: usize,
    task_id: usize,
    sem: KSemInnerType
}

pub struct TaskQueue {
    active_tasks: DynList<KThread>,
    waiting_tasks: DynList<KThread>,
    terminated_tasks: DynList<KThread>,
    suspended_tasks: DynList<KThread>,
    notifier_list: DynList<KEvent>,
    timer_notifier_list: DynList<TimerInfo>,
    cleanup_work_list: DynList<ProcessCleanupWork>,
    timer_list: DynList<TimerInfo>,
    running_task: Option<NonNull<ListNode<KThread>>>,
    idle_task_stack: NonNull<u8>,
    leftover_stack: DynList<Stack>,
    flip_flop: bool
}

unsafe impl Send for TaskQueue{}

impl TaskQueue {
    const fn new() -> Self {
        TaskQueue {
            active_tasks: List::new(),
            waiting_tasks: List::new(),
            terminated_tasks: List::new(),
            suspended_tasks: List::new(),
            notifier_list: List::new(),
            timer_list: List::new(),
            timer_notifier_list: List::new(),
            cleanup_work_list: List::new(),
            running_task: None,
            idle_task_stack: NonNull::dangling(),
            leftover_stack: List::new(),
            flip_flop: false
        }
    }
}

static SCHEDULER_CON_BLK: PerCpu<SchedulerCB> = PerCpu::new_with(
    [const {SchedulerCB{
        preemption_count: AtomicUsize::new(0),
        is_dw_mode: AtomicBool::new(false),
        dw_queue: Spinlock::new(List::new()),
        task_queue: Spinlock::new(TaskQueue::new())
    }}; MAX_CPUS]
);

pub fn add_user_stack_to_cur_task(user_stack: Stack) {
    let task = get_current_task().expect("add_user_stack_to_cur_task() called from idle task!");
    task.lock().user_stack = Some(user_stack);
}

pub fn get_task_info(task_id: usize) -> Option<KThread> {
    let task_map = TASKS.lock();

    task_map.get(&task_id).map(|item| {
        Arc::clone(item)
    })
}

// None indicates that idle task is currently running
// Be careful while calling these functions as they increment the strong count
// Once done with retreiving the info you want, it's important that you drop them
pub fn get_current_task() -> Option<KThread> {
    let cur_task_ptr = unsafe {
        get_per_cpu_data::<24>()
    };

    if cur_task_ptr == 0 {
        return None;
    }

    Some(Arc::clone(unsafe { &*(cur_task_ptr as *const KThread) }))
}

// Use this, if you just want the id
pub fn get_current_task_id() -> Option<usize> {
    Some(get_current_task()?.lock().get_id())
}

pub fn yield_cpu() {
    // Remove all remaining run time
    get_current_task()
    .expect("yield_cpu() called from idle task!").lock().quanta = 0;

    if is_system_in_interrupt_context() {
        schedule();
        force_context_switch();
    }
    else {
        hal::yield_cpu();
    }
}

pub fn is_preemption_enabled() -> bool {
    SCHEDULER_CON_BLK.local().preemption_count.load(Ordering::Acquire) == 0
}

pub fn is_in_dw_mode() -> bool {
    SCHEDULER_CON_BLK.local().is_dw_mode.load(Ordering::Acquire)
}

fn create_driver_worker(routine: DwRoutine, context: *mut c_void) -> Result<(), KError> {
    SCHEDULER_CON_BLK.local().dw_queue.lock().add_node(DriverWorker { routine, context })
}

fn pop_head_dw(q: &mut DynList<DriverWorker>) -> Option<ListNodeGuard<DriverWorker, PoolAllocator>> {
    let head = q.first().map(NonNull::from)?;
    let guard = unsafe { q.remove_node(head) };
    Some(guard)
}

pub fn dw_handler() {
    let cb = SCHEDULER_CON_BLK.local();

    if cb.is_dw_mode.load(Ordering::Acquire) {
        panic!("dw_handler() called in dw_mode!");
    }

    let mut dw = match pop_head_dw(&mut cb.dw_queue.lock()) {
        Some(d) => d,
        None => return,
    };

    cb.is_dw_mode.store(true, Ordering::Release);
    // Since we are now allowing nested interrupts, we need to ensure the variant
    // that user_gs == kernel_gs
    let user_base = get_per_cpu_base();
    set_per_cpu_base(get_per_cpu_kernel_base());

    // Stay in dw mode as long as long as dw queue is not empty
    loop {
        hal::enable_interrupts(true);
        (dw.routine)(dw.context);
        hal::disable_interrupts();

        match pop_head_dw(&mut cb.dw_queue.lock()) {
            Some(next) => { dw = next; }
            None => break,
        }
    }

    // No work items left, restore base
    set_per_cpu_base(user_base);
    cb.is_dw_mode.store(false, Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn io_create_driver_worker_ffi(routine: DwRoutine, context: *mut c_void) -> KError {
    match create_driver_worker(routine, context) {
        Ok(()) => KError::Success,
        Err(e) => e,
    }
}

pub fn init() {
    let init_task = Task::new(false, 0, None)
    .expect("Init task creation failed!!");
    
    let init_proc = get_process_info(0).expect("Unable to locate init process!");
    
    TASKS.lock().insert(0, Arc::clone(&init_task));

    init_task.lock().status = TaskStatus::Running;
    init_task.lock().panic_base = get_panic_base();
    init_task.lock().process = Some(init_proc);
    init_task.lock().vcb = Some(get_kernel_addr_space());

    {
        let mut sched_cb = SCHEDULER_CON_BLK.local().task_queue.lock();
        sched_cb.active_tasks.add_node(init_task).expect("Init task creation failed!");

        let task = NonNull::from(sched_cb.active_tasks.first().unwrap());

        unsafe {
            let guard = sched_cb.active_tasks.remove_node(task);
            sched_cb.running_task = Some(ListNode::into_inner(guard));
        }
    
        setup_current_task_ptr(&mut sched_cb);
    }

    // Now init idle task stack for all cpus
    for core in 0..get_total_cores() {
        let stack_base = get_worker_stack(core);
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock()
        };

        // We need to create separate stack for idle task on cpu 0, since the current stack is used by init task
        if core == 0 {
            let stack = Stack::into_inner(&mut Stack::new_with(cpu::WORKER_STACK_SIZE, PAGE_SIZE, false).expect("Could not create worker stack for cpu 0"));
            sched_cb.idle_task_stack = stack;

            debug!("Created idle stack for cpu-0 with address:{:#X}", stack.as_ptr().addr()); 
        }
        else {
            sched_cb.idle_task_stack = NonNull::new(stack_base as *mut u8).unwrap();
        }
    }


    info!("Created init task 0");
    enable_scheduler_timer();
}

pub fn add_cur_task_to_wait_queue(wait_semaphore: KSemInnerType, timeout: Option<usize>, is_interruptible: bool) -> Option<SignalCause> {
    if is_interruptible {
        let cur = get_current_process().expect("add_cur_task_to_wait_queue() called from idle process!");
        assert!(cur.lock().get_user_flag());
    }

    let mut sched_cb = SCHEDULER_CON_BLK.local().task_queue.lock();
    let cb = sched_cb.running_task;
    if cb.is_none() {
        panic!("add_cur_task_to_wait_queue() called from idle task!!");
    }
    
    let cur_task = unsafe { &**cb.unwrap().as_ptr() };

    let mut task = cur_task.lock();
    assert!(task.status != TaskStatus::Suspended);

    // TERMINATED > WAITING, don't do anything
    if task.status == TaskStatus::Terminated {
        return Some(SignalCause::Normal);
    }
    
    // If a task goes back to interruptible wait state
    // before we started executing the signal handler
    // then interrupt that too
    if is_interruptible && task.in_signal_init != 0 {
        return Some(SignalCause::Interruption);
    }
    
    assert!(task.wait_semaphores.get_nodes() == 0);

    task.wait_semaphores.add_node(wait_semaphore.clone())
    .expect("Failed to add semaphore to wait_semaphores list in task CB!");

    if let Some(timer_count) = timeout {
        assert!(timer_count > 0);
        sched_cb.timer_list.add_node(TimerInfo {timer_count, task_id: task.id, sem: wait_semaphore})
        .expect("Failed to add timer info into TaskQueue!");
    }

    if is_interruptible {
        task.status = TaskStatus::WaitingInterruptible;
    }
    else {
        task.status = TaskStatus::Waiting;
    }
    None
}

fn remove_wait_semaphore(task: &mut Task, sched_cb: &mut TaskQueue, wait_semaphore: KSemInnerType) {
    assert!(task.wait_semaphores.get_nodes() == 1);

    // Remove the timer and semaphore from task queue and task CB
    sched_cb.timer_list.find_and_remove(|t| {
        t.task_id == task.id && Arc::ptr_eq(&t.sem, &wait_semaphore) 
    });

    task.wait_semaphores.find_and_remove(|t| {
        Arc::ptr_eq(t, &wait_semaphore)
    });
}

pub fn signal_waiting_task(task_id: usize, wait_semaphore: KSemInnerType, cause: SignalCause) -> bool {
    let this_task = get_task_info(task_id);

    // It could be that this task has been killed
    if this_task.is_none() {
        // The task will be removed from the blocked list by kill_thread
        return false;
    }

    let this_task = this_task.unwrap();
    let core = this_task.lock().core;
    let mut skip_notify = false;
    let mut ret_status = true;
    disable_preemption();
    {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock() 
        };

        let status = this_task.lock().status;
        match status {
            TaskStatus::Waiting | TaskStatus::WaitingInterruptible => {
                if cause == SignalCause::Interruption && status != TaskStatus::WaitingInterruptible {
                    enable_preemption();
                    return false;
                }

                let res = sched_cb.waiting_tasks.find_and_remove(|t| t.lock().id == task_id);
                let mut task = this_task.lock();
                // This happens when signal task is called even before the waiting task gets a chance to be put into the wait queue
                if res.is_none() {
                    // Let task run again with high priority
                    task.status = TaskStatus::Running;
                    task.quanta = INIT_QUANTA;
                }
                else {
                    let signal_task = ListNode::into_inner(res.unwrap());
                    sched_cb.active_tasks.insert_node_at_head(signal_task);
                    task.status = TaskStatus::Active;
                }

                remove_wait_semaphore(&mut *task, &mut *sched_cb, wait_semaphore);
                task.set_signal_cause(cause);
            },

            TaskStatus::Terminated => {
                skip_notify = true;
                ret_status = false;
            },

            TaskStatus::Active | TaskStatus::Running | TaskStatus::Suspended => {
                // This case should really not be possible. If task were in one of these states
                // then it shouldn't have been part of the blocked list within a semaphore in the first place
                panic!("Signalled task {} which was in ACTIVE/RUNNING/SUSPENDED state??", task_id);
            }
        }
    }
    
    if !skip_notify {
        notify_other_cpu(core);
    }

    enable_preemption();
    ret_status
}
            
fn remove_timer_from_task_queue(sched_cb: &mut TaskQueue, id: usize) {
    sched_cb.timer_list.find_and_remove(|t| {
        t.task_id == id
    });
}

// Killing a thread is an unsafe process in general
// This procedure must be called in a coordinated manner, otherwise it simply
// destroys a task/process asynchronously. Most of the time it's fine. However this
// could lead to memory leaks. A task could have a heap reference Arc pointer on it's stack.
// If task is killed at this point, the destructor is never run and the memory is leaked
// Note that there is no stack unwinding destructor calls to avoid this problem within the kernel
// Doing stack unwinding for every process/task destruction is not practical and can cause lot
// of bookkeeping and performance issues
pub fn kill_thread(task_id: usize, exit_code: isize) {
    let mut yield_flag = false;
    let mut drop_task  = false;
    let mut skip_notify  = false;


    let this_task = get_task_info(task_id);
    
    #[cfg(not(feature = "kunit-test"))]
    {
        let target_proc = this_task
            .as_ref()
            .expect("Attempted to kill idle task!")
            .lock()
            .get_process();
        let target_proc_id = target_proc
            .expect("Target kill thread has no process??")
            .lock()
            .get_id();

        assert!(target_proc_id != 0, "Attempted to kill system thread!");
    } 

    if this_task.is_none() {
        return;
    }

    let this_task  = this_task.unwrap();
    let core = this_task.lock().core;
    
    disable_preemption();
    let sweep_irps;
    {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock()
        };

        let status = {
            let mut task_locked = this_task.lock();
            let status = task_locked.status;
            task_locked.status = TaskStatus::Terminated;
            task_locked.exit_code.store(exit_code, Ordering::Relaxed);
            status
        };
        sweep_irps = status != TaskStatus::Terminated;

        // Remove task from active list and add to terminated list
        match status {
            TaskStatus::Active => {
                let task_l = sched_cb.active_tasks.find_and_remove(|t| {t.lock().id == task_id});
                assert!(task_l.is_some());
                let task_node = ListNode::into_inner(task_l.unwrap());
                sched_cb.terminated_tasks.insert_node_at_tail(task_node);
            },

            TaskStatus::Waiting | TaskStatus::WaitingInterruptible => {
                let task_l = sched_cb.waiting_tasks.find_and_remove(|t| {t.lock().id == task_id});
                if task_l.is_some() {
                    let task_node = ListNode::into_inner(task_l.unwrap());

                    sched_cb.terminated_tasks.insert_node_at_tail(task_node);
                }
                else {
                    // Task might not have been scheduled out
                    // In this case, let scheduler take care of it

                    // We might be here due to interrupt in same cpu / kill task issued by different cpu
                }

                drop_task = true;
            },

            TaskStatus::Running => {
                // Since the task is currently running, we can't immediately drop it as it is using this stack
                // So, delay the stack destruction
                
                // Stack is guaranteed to be present. Only init task has None value here
                let stack = take(this_task.lock().stack.as_mut().unwrap());
                
                sched_cb.leftover_stack.add_node(stack).expect("Unable to add stack node to leftover_stack list!");
                sched_cb.flip_flop = true;
                
                // Only yield if the current task is killing itself (i.e It's not just that a task from another cpu is killing the 
                // current running task of this cpu)
                yield_flag = hal::get_core() == core;
            },

            TaskStatus::Suspended => {
                let task_l = sched_cb.suspended_tasks.find_and_remove(|t| {t.lock().id == task_id});
                
                // Scheduler did not get chance to put this task into suspended queue
                if task_l.is_some() {
                    let task_node = ListNode::into_inner(task_l.unwrap());
                    sched_cb.terminated_tasks.insert_node_at_tail(task_node);
                }
            },

            TaskStatus::Terminated => {
                info!("Task {} already terminated..", task_id);
                skip_notify = true;
                yield_flag = hal::get_core() == core;
            }
        }

        if drop_task {
            remove_timer_from_task_queue(&mut sched_cb, task_id);
        }
    }

    if !skip_notify {
        notify_other_cpu(core);
    }

    // Inform semaphore that this task is about to be killed, remove it from the blocked list
    if drop_task {
        let wait_semaphores = take(&mut this_task.lock().wait_semaphores);
        for sem in wait_semaphores.iter() {
            KSem::drop_task((**sem).clone(), task_id);
        }
    }

    if sweep_irps {
        kill_sweep_irps(&this_task);
    }

    // Drop it explicitly since we won't return from here and rust thinks that
    // this stack frame here is preserved, which means that this reference gets leaked
    drop(this_task);

    enable_preemption();

    // The current running task is killed, yield remaining context
    if yield_flag {
        crate::sched_log!("Yielding task {}", task_id);
        // This is case where task/thread is killing itself. It is important for caller
        // to ensure that preemption is not disabled (only in this case). Otherwise this thread would just keep running
        // If it is called from exit_thread, then this will result in panic
        yield_cpu();
    }
}

extern "C" fn io_complete(irp: *mut Irp, ctx: *mut c_void) {
    disable_preemption();
    let status = unsafe { (*irp).status };
    assert!(!unsafe{ (*irp).is_completed});
    crate::io_log!("io_complete: status_code {} on irp {:#X} by thread {}", status as isize, irp.addr(), unsafe{(*irp).thread_id});
    let ctx_ptr = ctx as *mut AsyncCtx;
    let ctx = unsafe { &mut *ctx_ptr };

    // If task is not killed and irp exists within the task irp list, remove it
    if let Some(dispatch_thread) = get_task_info(unsafe { (*irp).thread_id }) {
        dispatch_thread.lock().find_and_remove_irp(irp);
    }

    // Remove the IRP from the device's pending list too
    let dev_ptr = unsafe { (*irp).device };
    if !dev_ptr.is_null() {
        if let Some(dev) = io::resolve_device(dev_ptr) {
            crate::io_log!("Removing pending irp from device object");
            dev.get_pending_irps().lock().find_and_remove(|&p| p == irp);
        }
    }

    // Run the completion routines.
    match &mut ctx.comp_type {
        CompletionCtxType::Sync((event, result_ptr)) => {
            unsafe { result_ptr.write((*irp).to_result()); }
            event.signal();
        },
        CompletionCtxType::Async((user_routine, user_ctx)) => {
            let result = unsafe { (*irp).to_result() };
            user_routine(&result as *const IrpResult, *user_ctx);
        }
    }

    if status != Status::Cancelled {
        deallocate_irp(irp, ctx_ptr);
    }
    else {
        // For cancellation path, the deallocation is done once driver calls io_start_processing
        unsafe {
            (*irp).is_cancelled = true;
        }
    }

    unsafe { (*irp).is_completed = true }
    enable_preemption();
}

pub fn allocate_irp(
    major: IrpMajor,
    minor: IrpMinor,
    buffer: MemoryRegion,
    offset: usize,
    device: *const DeviceObject,
    complete_event: Option<KEvent>,
    completion_result_ptr: *mut IrpResult,
    user_completion_routine: Option<extern "C" fn(*const IrpResult, *mut c_void)>,
    user_completion_ctx: *mut c_void
) -> IrpPtr {
    disable_preemption();
    let cur_thread = get_current_task().expect("Called allocate_irp() from idle task!");

    // Create the context and irp
    let actx = Box::into_raw_with_allocator(Box::new_in(
        if complete_event.is_some() {
            AsyncCtx {
                irp: core::ptr::null_mut(),
                comp_type: CompletionCtxType::Sync((
                    complete_event.unwrap(),
                    completion_result_ptr
                ))
            }
        }
        else {
            AsyncCtx {
                irp: core::ptr::null_mut(),
                comp_type: CompletionCtxType::Async((
                    user_completion_routine.unwrap(),
                    user_completion_ctx
                )),
            }
        },
        PoolAllocatorGlobal
    )).0;

    let mut irp_box = Box::new_in(
        Irp::new(major, buffer, offset, io_complete, actx as *mut c_void, device, cur_thread.lock().get_id()),
        PoolAllocatorGlobal
    );
    irp_box.minor_code = minor;
    let irp_raw = Box::into_raw_with_allocator(irp_box).0;
    unsafe { (*actx).irp = irp_raw; }

    if !device.is_null() {
        if let Some(dev) = io::resolve_device(device) {
            dev.get_pending_irps().lock().add_node(irp_raw)
                .expect("Failed to register IRP with device pending list");
        }
    }

    cur_thread.lock().register_irp(irp_raw);
    enable_preemption();

    irp_raw
}
 
pub fn cancel_irp(irp_ptr: IrpPtr) {
    let irp = unsafe { &mut *irp_ptr };
    acquire_spinlock(&mut irp.cancel_lock);

    // Either another path cancelled this explicitly, or the driver did
    // not (a) get a chance yet to register the cancel routine, or
    // (b) decided that cancellation was not required for this request.
    if irp.cancel_routine.is_none() || irp.is_cancelled {
        release_spinlock(&mut irp.cancel_lock);
        return;
    }

    (irp.cancel_routine.unwrap())(irp.device, irp_ptr);
    release_spinlock(&mut irp.cancel_lock);
}

fn kill_sweep_irps(task: &KThread) {
    let irp_list = task.lock().take_irp_list();
    for irp in irp_list.iter() {
        let dev_obj = unsafe {(***irp).device};
        match get_device(unsafe {(*dev_obj).id}) {
            None => {
                // This device has been removed, don't send any cancel irp atp
                continue;
            },
            Some(d) => {
                match d.state() {
                    DeviceState::Removed | DeviceState::Removing => {
                        continue;
                    },
                    _ => {}
                }
            }
        }
        cancel_irp(**irp);
    }
}

pub fn exit_thread(exit_code: isize) -> ! {
    let thread_id = get_current_task_id().expect("Attempted to kill idle task!!");

    assert!(is_preemption_enabled(), "exit_thread() called with preemption disabled!");

    kill_thread(thread_id, exit_code);

    panic!("exit_thread() unreachable reached!!");
}

// Release any resource here that require this task's address space
fn release_task_resources_with_context(task: &KThread) {
    let mut guard = task.lock();
    while guard.pending_signals != 0 {
        let signal = get_highest_set_bit(guard.pending_signals);
        assert!(signal != -1);
        guard.pending_signals &= !(1 << signal as u8);
        uninit_signal_handler(&mut *guard, signal as usize, true);
    }

    // Release the user stack if any
    guard.user_stack = None;
}

fn release_task_resources(task: &KThread) {
    let old_vcb_opt = {
        let guard = task.lock();
        guard.vcb
    };

    if let Some(old_vcb) = old_vcb_opt {
        let new_vcb = unsafe { get_per_cpu_data::<32>() };
        // We're in idle task
        if new_vcb == 0 {
            switch_address_space_from_idle(old_vcb);
            release_task_resources_with_context(task);
            switch_address_space_for_idle(old_vcb);
        } 
        else {
            let new_vcb = NonNull::new(new_vcb as *mut Spinlock<VirtMemConBlk>).unwrap();
            switch_address_space(new_vcb, old_vcb);
            release_task_resources_with_context(task);
            switch_address_space(old_vcb, new_vcb);
        };
    }
    else {
        panic!("Task {} doesn't seem to have vcb assigned??", task.lock().id);   
    }
   
    // For now the only resource is the task stack, so drop it
    drop(task.lock().stack.take());
}

// We do all this moving out of stuff and into other stuff drama in order to avoid holding any lock during signal operation
fn reap_tasks(sched_cb: &mut TaskQueue) {
    while sched_cb.terminated_tasks.get_nodes() != 0 {
        let task = NonNull::from(sched_cb.terminated_tasks.first().unwrap());
        let task_inner = unsafe {
            &*task.as_ptr()
        };
        
        let id = task_inner.lock().get_id();
        let thread_exit_code = task_inner.lock().exit_code.load(Ordering::Relaxed);
        sched_cb.notifier_list.add_node(task_inner.lock().term_notify.clone()).expect("Failed to add semaphore to notifier list!");

        // Extract the pointer, release the lock and then call remove_thread
        // Otherwise, we run the risk of deadlock
        {
            let process_ref = task_inner.lock().process.as_ref().unwrap().clone();
            let mut process_guard = process_ref.lock();
            
            // If thread is last in the process and process is not already being killed with kill_process
            // then set the process's exit code as the exit code for this thread
            let is_last_thread_in_proc = process_guard.remove_thread(id, thread_exit_code);

            if is_last_thread_in_proc.is_some() {
                crate::sched_log!("Adding process {} notifier to notifier list as task {} is terminating", process_guard.get_id(), id);
                sched_cb.notifier_list.add_node(process_guard.get_notify_sem()).expect("Failed to add process notify semaphore to notifier list!");
                sched_cb.notifier_list.add_node(process_guard.get_init_sem()).expect("Failed to add process init semaphore to notifier list!");
                sched_cb.cleanup_work_list.add_node(is_last_thread_in_proc.unwrap()).expect("Failed to add cleanup work item in cleanup list");
            }
        }

        release_task_resources(task_inner);
        crate::sched_log!("Removing task {} on core {}", id, hal::get_core());
        unsafe {
            sched_cb.terminated_tasks.remove_node(task);
        }
        
        TASKS.lock().remove(&id);
    }   
}

fn update_timers(sched_cb: &mut TaskQueue) {
    let mut idx = 0;
    let list_size = sched_cb.timer_list.get_nodes();

    while idx < list_size {
        let timer = sched_cb.timer_list.first_mut().unwrap();
        timer.timer_count = timer.timer_count.saturating_sub(QUANTUM);
        let is_done = timer.timer_count == 0;

        if is_done {
            let task_id = timer.task_id;
            let sem = timer.sem.clone();
            let timer_new = TimerInfo { timer_count: 0, task_id, sem };
            sched_cb.timer_notifier_list.add_node(timer_new).expect("Unable to add timer node semaphore into notifier list!");
            sched_cb.timer_list.pop_head();
        }
        else {
            let timer_ptr = NonNull::from(timer);
            let timer_ref = unsafe {
                ListNode::into_inner(sched_cb.timer_list.remove_node(timer_ptr))
            };
            
            sched_cb.timer_list.insert_node_at_tail(timer_ref);
        }

        idx += 1;
    }
}


fn notify_watchers(notifier_list: DynList<KEvent>, timer_notifier_list: DynList<TimerInfo>) {
    for sem in notifier_list.iter() {
        sem.signal();
    }

    for expired_timer in timer_notifier_list.iter() {
        KSem::from(expired_timer.sem.clone()).signal_task_on_timeout(expired_timer.task_id);
    }
}

fn submit_cleanup_work(mut cleanup_list: DynList<ProcessCleanupWork>) {
    for work in cleanup_list.iter_mut() {
        enqueue_cleanup(take(&mut **work));
    }
}

pub fn disable_preemption() {
    SCHEDULER_CON_BLK.local().preemption_count.fetch_add(1, Ordering::AcqRel);
}

pub fn enable_preemption() {
    let old = SCHEDULER_CON_BLK.local().preemption_count.fetch_sub(1, Ordering::AcqRel);
    assert!(old != 0);
}

fn can_sleep(sched_cb: &mut TaskQueue) -> bool {
    sched_cb.flip_flop == false && sched_cb.timer_list.get_nodes() == 0
}

#[inline]
fn switch_address_space(old_vcb: VCB, new_vcb: VCB) {
    if old_vcb != new_vcb {
        unsafe {
            set_address_space(new_vcb);
        }
    }
}

#[inline]
fn switch_address_space_for_idle(old_vcb: VCB) {
    let new_vcb = get_kernel_addr_space();

    switch_address_space(old_vcb, new_vcb);
}

#[inline]
fn switch_address_space_from_idle(new_vcb: VCB) {
    let old_vcb = get_kernel_addr_space();

    switch_address_space(old_vcb, new_vcb);
}

#[cfg(target_arch = "x86_64")]
pub fn set_kernel_mode_and_syscall_params(is_kernel_mode: bool, is_in_syscall: bool, user_gs: u64, user_rsp: u64) {
    let task = get_current_task()
    .expect("toggle_cur_task_to_kernel_mode() called from idle task!");
    
    let mut guard = task.lock();
    let signal = get_highest_set_bit(guard.pending_signals);
    if signal == -1 {
        guard.is_kernel_mode = is_kernel_mode;
        guard.is_in_syscall = is_in_syscall;
        guard.syscall_gs = user_gs;
        guard.syscall_rsp = user_rsp;
    }
    else {
        let sig_frame = guard.signal_frame_pending[signal as usize].as_mut().expect("Failed to get sig_frame despite pending signal set");
        sig_frame.is_kernel_mode = is_kernel_mode;
        sig_frame.is_in_syscall = is_in_syscall;
        sig_frame.syscall_gs = user_gs;
        sig_frame.syscall_rsp = user_rsp;
    } 
}

pub fn toggle_cur_task_kernel_mode() {
    let task = get_current_task()
    .expect("toggle_cur_task_to_kernel_mode() called from idle task!");
    
    let mut guard = task.lock();
    let signal = get_highest_set_bit(guard.pending_signals);
    if signal == -1 {
        guard.is_kernel_mode = !guard.is_kernel_mode;
    }
    else {
        let sig_frame = guard.signal_frame_pending[signal as usize].as_mut().expect("Failed to get sig_frame despite pending signal set");
        sig_frame.is_kernel_mode = !sig_frame.is_kernel_mode;
    }
}

fn setup_current_task_ptr(sched_cb: &mut TaskQueue) {
    let cur_task_ptr = if sched_cb.running_task.is_some() {
        unsafe {
            let kthread = &(**sched_cb.running_task.as_ref().unwrap().as_ptr()) as *const KThread;
            
            {
                let guard = (*kthread).lock(); 
                let signal = get_highest_set_bit(guard.pending_signals);
                let stack_addr  = if signal == -1 {
                    if let Some(cur_stack_address) = &guard.stack {
                        cur_stack_address.get_stack_base()
                    }
                    else {
                        0
                    }
                }
                else {
                    let sig_frame = guard.signal_frame_pending[signal as usize]
                        .as_ref().expect("signal_init set but signal frame info not available!");
                    sig_frame.kernel_stack_base
                };
                let vcb = if let Some(vcb_ptr) = guard.vcb {
                    vcb_ptr.as_ptr().addr() as u64
                }
                else {
                    0
                };
                set_per_cpu_data::<16>(stack_addr as u64);
                set_per_cpu_data::<32>(vcb);
                
                // We're switching to a user thread. Setup the kernel stack
                if guard.is_user_thread() {
                    #[cfg(target_arch = "x86_64")]
                    set_tss_stack(stack_addr as u64);
                }
            }
            
            kthread as u64
        }
    } 
    else {
        // Idle task will set vcb as 0
        unsafe { set_per_cpu_data::<32>(0); }
        0
    };

    unsafe {
        set_per_cpu_data::<24>(cur_task_ptr); 
    }
}

fn get_task_context(task: &mut Task, new_schedule: bool) -> (usize, u64) {
    if !new_schedule {
        return (fetch_context(), get_per_cpu_base())
    }

    let signal = get_highest_set_bit(task.pending_signals);
    
    // Restore the task context itself
    if signal == -1 {
        (task.context, task.per_cpu_base)
    }
    else {
        // Restore the previous signal handler context
        let sig_frame = task.signal_frame_pending[signal as usize]
            .as_ref().expect("signal_init set but signal frame info not available!");
        (sig_frame.context, sig_frame.per_cpu_base)
    }
}

// Inits the signal handler if its possible
// If success, returns true
fn init_signal_handler(task: &mut Task, signal: usize, new_schedule: bool) -> bool {
    let (task_context, task_per_cpu_base) = get_task_context(task, new_schedule);

    // We need to first check if the thread has been to user context yet
    // That is, it must performing a syscall or must have come here from user mode
    // due to an interrupt
    let is_user = is_user_context(task_context);
    
    #[cfg(target_arch = "x86_64")]
    if is_user || task.is_in_syscall() {
        // We can setup the signal handler now
        let (user_rsp, per_cpu_base) = if is_user {
            (get_user_stack(task_context) as u64,
            task_per_cpu_base)
        }
        else {
            (
                task.syscall_rsp(),
                task.syscall_gs()
            )
        };

        // We want to put the initial context for the signal handler somewhere
        // So we put that context in the user stack of this thread right above 
        // where its currently being used by the user thread.
        // Once this is done, further context saves and kernel mode execution
        // uses the kernel stack that belongs to this thread.

        // To access the user stack we first map a max of 2 pages 
        // from stack to kernel and create the context
        let context_base = align_down(user_rsp as usize, PAGE_SIZE);
        let mapped_base = context_base - PAGE_SIZE;
        let phys_addr = get_physical_address(mapped_base, PageDescriptor::USER)
            .expect("No physical address found for given user_rsp!");
        let mapped_base_kernel = map_to_kernel(phys_addr, PAGE_SIZE * 2)
            .expect("Failed to map user stack address to build signal frame!");
        let mapped_context_base = mapped_base_kernel.addr() + PAGE_SIZE;

        debug!("Setting up signal handler -> signal={}, is_user={}, in_syscall={}, user_rsp={:#X}, per_cpu_base={:#X}, phys_addr={:#X}, context_base={:#X}",
        signal, is_user, task.is_in_syscall, user_rsp, per_cpu_base, phys_addr, mapped_context_base);

        let sig_frame = task.signal_frame_init[signal]
            .as_mut().expect("signal_init set but signal frame info not available!");
        let context = if is_user {
            create_context_from(sig_frame.handler, mapped_context_base as *mut u8, context_base, task_context, sig_frame.user_ctx.addr())
        }
        else {
            create_user_context(sig_frame.handler, mapped_context_base as *mut u8, context_base, sig_frame.user_ctx.addr())
        };

        sig_frame.context = context;
        sig_frame.per_cpu_base = per_cpu_base;
        sig_frame.mapped_base = mapped_base_kernel.addr();

        // We will use the existing thread kernel stack as the kernel stack for this signal
        // Use the space on that stack. If this is a nested signal, then it layers on 
        // top of the previous one
        sig_frame.kernel_stack_base = align_down(task_context, 16);

        assert!(task.signal_frame_pending[signal].is_none());

        // Move frame from init -> pending
        task.signal_frame_pending[signal] = task.signal_frame_init[signal].take();
    }
    else {
        return false;
    }
    
    if !new_schedule {
        // Since we're going to run this handler, save the previous handler/task state
        // If new schedule, it means that context is already saved in handler/task. Don't override it.
        save_task_context(task);
    } 
    
    // Update the signal states
    task.in_signal_init &= !(1 << signal);
    task.pending_signals |= 1 << signal;

    true
}

fn restore_task_context(task: &mut Task) {
    // Looks at pending signals and brings back the context 
    // which was previously running
    let signal = get_highest_set_bit(task.pending_signals);
    
    // Restore the task context itself
    if signal == -1 {
        switch_context(task.context);
        #[cfg(target_arch = "x86_64")]
        {
            set_per_cpu_base(task.per_cpu_base);
        }
    }
    else {
        // Restore the previous signal handler context
        let sig_frame = task.signal_frame_pending[signal as usize]
            .as_ref().expect("signal_init set but signal frame info not available!");
        switch_context(sig_frame.context);
        #[cfg(target_arch = "x86_64")]
        {
            set_per_cpu_base(sig_frame.per_cpu_base);
        }
    }
}

fn save_task_context(task: &mut Task) {
    // Look at current pending signal and save 
    // its context
    let context = fetch_context();
    let per_cpu_base = get_per_cpu_base();
    let signal = get_highest_set_bit(task.pending_signals);
    if signal == -1 {
        // No signal handler exists. Save the context within task itself
        task.context = context;
        task.per_cpu_base = per_cpu_base;
    }
    else {
        let sig_frame = task.signal_frame_pending[signal as usize]
            .as_mut().expect("signal_init set but signal frame info not available!");

        sig_frame.context = context;
        sig_frame.per_cpu_base = per_cpu_base; 
    }
}

fn uninit_signal_handler(task: &mut Task, signal: usize, no_context_switch: bool) {
    let sig_frame = task.signal_frame_pending[signal]
        .as_mut().expect("signal_init set but signal frame info not available!");

    // Unmap the mapped stack
    unmap_from_kernel(sig_frame.mapped_base, PAGE_SIZE * 2)
        .expect("Failed to unmap signal frame stack");

    task.completed_signals &= !(1 << signal);

    if !no_context_switch {
        // Restore previous context
        restore_task_context(task);
    }

    task.signal_frame_pending[signal] = None;
}

fn save_signal_context(task: &mut Task, signal: usize) {
    let sig_frame = task.signal_frame_pending[signal]
        .as_mut().expect("signal_init set but signal frame info not available!");

    sig_frame.context = fetch_context();

    #[cfg(target_arch = "x86_64")]
    {
        sig_frame.per_cpu_base = get_per_cpu_base();
    }
}

// This is called right before a thread is going to be scheduled out
// So save whatever state we currently have
fn check_and_save_signal_handler(task:&mut Task) -> bool {
    let comp_signal = get_highest_set_bit(task.completed_signals);
    let signal = if comp_signal == -1 {
        get_highest_set_bit(task.pending_signals)
    }
    else {
        // Completion bit has been set for this process
        // but its about to be scheduled out.
        // If we only consider pending bit, we'll wrongly
        // save to the context of another handler/task
        comp_signal
    };

    if signal == -1 {
        // Tell scheduler to do normal thread scheduling
        false
    }
    else {
        save_signal_context(task, signal as usize);
        true
    }
}

fn switch_to_signal_handler(task: &mut Task, signal: usize, new_schedule: bool) {
    // This means the signal handler was already running or 
    // another signal handler completed. In both cases, the context
    // is already setup, we don't need to do anything else
    if !new_schedule {
        return;
    }
    let sig_frame = task.signal_frame_pending[signal]
        .as_mut().expect("signal_init set but signal frame info not available!");

    switch_context(sig_frame.context);
#[cfg(target_arch = "x86_64")]
    set_per_cpu_base(sig_frame.per_cpu_base);
}

// If there is a signal handler pending, switch to its context
// If there is init pending for a signal handler and no higher prio signal is running
// then save the current task/signal context and then switch to new context
// If any signals have been completed, then release its resources and switch back 
// to any pending signal handlers/task
fn check_and_execute_signal_handler(task: &mut Task, new_schedule: bool) -> bool {
    let comp_signal = get_highest_set_bit(task.completed_signals);
    
    // First check for any completions and deallocate the resources
    // associated with them
    if comp_signal != -1 {
        assert!(task.completed_signals.is_power_of_two()); 
        
        debug!("check_and_execute: completing signal {}", comp_signal);
        uninit_signal_handler(task, comp_signal as usize, false);
    }
    
    let pend_signal = get_highest_set_bit(task.pending_signals);
    let init_signal = get_highest_set_bit(task.in_signal_init);

    // Now check and run any pending signals
    if pend_signal == -1 && init_signal == -1 {
        // There is no signal handler or anything pending
        // Tell scheduler to do normal thread scheduling
        false
    }
    else if pend_signal != -1 && init_signal == -1 {
        // Continue running the pending signal handler
        switch_to_signal_handler(task, pend_signal as usize, new_schedule);
        true
    }
    else if pend_signal == -1 && init_signal != -1 {
        // Init the new signal handler and start running it
        if !init_signal_handler(task, init_signal as usize, new_schedule) {
            debug!("Init signal {} found, but failed to start", init_signal);
            false
        }
        else {
            debug!("Init signal {} found. Starting new handler with context={:#X}", init_signal, task.context);
            // We have successfully init this handler. Now start running it immediately
            switch_to_signal_handler(task, init_signal as usize, true);
            true
        }
    }
    else {
        assert!(pend_signal != init_signal); 
        if pend_signal < init_signal {
            // There is higher priority signal
            // Save the current one and start allocating 
            // and running the new handler
            if init_signal_handler(task, init_signal as usize, new_schedule) {
                debug!("Higher prio {} signal found over {}. Switching to that with context={:#X}", init_signal, pend_signal, task.context);
                switch_to_signal_handler(task, init_signal as usize, true);
            }
            else {
                panic!("Signal handler already running.. This case must not be possible!");
            }
        }
        else {
            // Continue running the same signal
            debug!("Found lower prio {} signal over {}. Continuing with existing one with context={:#X}", init_signal, pend_signal, task.context);
            switch_to_signal_handler(task, pend_signal as usize, new_schedule);
        }
    
        true
    }
}


// Main scheduler loop
pub fn schedule() {
    let (
        notifier_list, 
        timer_notifier_list,
        cleanup_work_list
    ) = {
        let sched_cb_cpu = SCHEDULER_CON_BLK.local();
        let mut sched_cb = sched_cb_cpu.task_queue.lock();
        update_timers(&mut sched_cb);
        
        if sched_cb_cpu.preemption_count.load(Ordering::Acquire) > 0
            || sched_cb_cpu.is_dw_mode.load(Ordering::Acquire) {
            (
                take(&mut sched_cb.notifier_list), 
                take(&mut sched_cb.timer_notifier_list),
                take(&mut sched_cb.cleanup_work_list)
            )
        }
        else {
            if sched_cb.running_task.is_some() {
                let current_task = sched_cb.running_task.unwrap(); 

                let mut task_info = unsafe {
                    current_task.as_ref().lock()
                };

                task_info.quanta = task_info.quanta.saturating_sub(1);
                let old_vcb = task_info.vcb.expect("VCB is none");
                
                // Switch to new task
                if task_info.status == TaskStatus::Waiting || task_info.status == TaskStatus::WaitingInterruptible || 
                task_info.status == TaskStatus::Terminated || task_info.status == TaskStatus::Suspended || 
                task_info.quanta == 0 {
                    // First choose new task
                    // We create NonNull here so that the node can later be removed
                    let head_task = sched_cb.active_tasks.first().and_then(|item| {
                        Some(NonNull::from(item))
                    });

                    // We have a new task to switch to
                    if head_task.is_some() {
                        let mut head_task_info = unsafe {
                            head_task.unwrap().as_ref().lock()
                        };
                        
                        assert!(head_task_info.status == TaskStatus::Active); 
                        head_task_info.status = TaskStatus::Running;
                        head_task_info.quanta = INIT_QUANTA;
                        let new_context = head_task_info.context;
                        let new_vcb = head_task_info.vcb.expect("VCB is none");

                        if !check_and_save_signal_handler(&mut *task_info) {
                            task_info.context = fetch_context();

                            #[cfg(target_arch = "x86_64")] 
                            {
                                task_info.per_cpu_base = get_per_cpu_base();
                            }
                        }

                        if task_info.status == TaskStatus::Running {
                            task_info.status = TaskStatus::Active; 
                        }

                        // This ensures that list doesn't delete the node. It simply removes it from the list 
                        let head_task = unsafe {
                            ListNode::into_inner(sched_cb.active_tasks.remove_node(head_task.unwrap()))
                        };

                        if task_info.status == TaskStatus::Waiting || task_info.status == TaskStatus::WaitingInterruptible {
                            sched_cb.waiting_tasks.insert_node_at_tail(current_task);
                        }
                        else if task_info.status == TaskStatus::Terminated {
                            crate::sched_log!("Adding task {} to terminated list", task_info.id);
                            sched_cb.terminated_tasks.insert_node_at_tail(current_task);
                        }
                        else if task_info.status == TaskStatus::Suspended {
                            crate::sched_log!("Adding task {} to suspended list", task_info.id);
                            sched_cb.suspended_tasks.insert_node_at_tail(current_task);
                        }
                        else {
                            sched_cb.active_tasks.insert_node_at_tail(current_task);
                        }

                        sched_cb.running_task = Some(head_task);

                        switch_address_space(old_vcb, new_vcb);
                        set_panic_base(head_task_info.panic_base);

                        if !check_and_execute_signal_handler(&mut head_task_info, true) {
                            switch_context(new_context);

                            #[cfg(target_arch = "x86_64")]
                            set_per_cpu_base(head_task_info.per_cpu_base);
                        }
                    }
                    else {
                        // No more tasks left. Check if we can continue running same task
                        if task_info.status != TaskStatus::Running {
                            if !check_and_save_signal_handler(&mut *task_info) {
                                task_info.context = fetch_context();
                                
                                #[cfg(target_arch = "x86_64")] 
                                {
                                    task_info.per_cpu_base = get_per_cpu_base();
                                }
                            }

                            if task_info.status == TaskStatus::Waiting || task_info.status == TaskStatus::WaitingInterruptible {
                                sched_cb.waiting_tasks.insert_node_at_tail(current_task);
                            }
                            else if task_info.status == TaskStatus::Terminated {
                                crate::sched_log!("Adding task {} to terminated list", task_info.id);
                                sched_cb.terminated_tasks.insert_node_at_tail(current_task);
                            }
                            else if task_info.status == TaskStatus::Suspended {
                                crate::sched_log!("Adding task {} to suspended list", task_info.id);
                                sched_cb.suspended_tasks.insert_node_at_tail(current_task);
                            }

                            prep_idle_task(&mut sched_cb, old_vcb);
                        }
                        else {
                            // Current task is in running state. Continue with it
                            check_and_execute_signal_handler(&mut *task_info, false);
                            task_info.quanta = INIT_QUANTA;
                        }
                    }
                }
                else {
                    // Continue executing same task
                    check_and_execute_signal_handler(&mut *task_info, false);
                }
            }
            else {
                // This means we're in idle task. Check and run any active tasks
                let head_task = sched_cb.active_tasks.first().and_then(|item| {
                    Some(NonNull::from(item))
                });

                if head_task.is_some() {
                    let mut head_task_info = unsafe {
                        head_task.unwrap().as_ref().lock()
                    };

                    assert!(head_task_info.status == TaskStatus::Active); 
                    head_task_info.status = TaskStatus::Running;
                    head_task_info.quanta = INIT_QUANTA;
                    let new_context = head_task_info.context;
                    
                    let head_task = unsafe {
                        ListNode::into_inner(sched_cb.active_tasks.remove_node(head_task.unwrap()))
                    };
                    sched_cb.running_task = Some(head_task);
                    
                    // Idle task uses the default kernel virtual address space
                    let new_vcb = head_task_info.vcb.unwrap();

                    switch_address_space_from_idle(new_vcb);
                    set_panic_base(head_task_info.panic_base);

                    if !check_and_execute_signal_handler(&mut *head_task_info, true) {
                        switch_context(new_context);
                        
                        #[cfg(target_arch = "x86_64")]
                        set_per_cpu_base(head_task_info.per_cpu_base);
                    }
                }
                else {
                    // Nothing to run
                    // If stack deletions / timers are pending, don't go into idle task yet
                    if can_sleep(&mut sched_cb) {
                        disable_scheduler_timer(); 
                    }
                }
            }

            setup_current_task_ptr(&mut sched_cb);

            // We do the flip flop technique since we are still using the same terminated task stack at this point
            // But we won't be using it from the next schedule on
            if sched_cb.flip_flop {
                sched_cb.flip_flop = false;
            }
            else {
                sched_cb.leftover_stack.clear();
            }

            reap_tasks(&mut sched_cb);
            (
                take(&mut sched_cb.notifier_list), 
                take(&mut sched_cb.timer_notifier_list),
                take(&mut sched_cb.cleanup_work_list)
            )
        }
    };

    notify_watchers(notifier_list, timer_notifier_list);
    submit_cleanup_work(cleanup_work_list);
}

fn prep_idle_task(sched_cb: &mut TaskQueue, old_vcb: VCB) {
    sched_cb.running_task = None;
    let context = create_kernel_context(idle_task, sched_cb.idle_task_stack.as_ptr() as *mut u8);
    
    switch_address_space_for_idle(old_vcb);
    set_panic_base(sched_cb.idle_task_stack.as_ptr() as usize);
    switch_context(context);

    #[cfg(target_arch = "x86_64")]
    set_per_cpu_base(get_per_cpu_kernel_base());
    
    if can_sleep(sched_cb) {
        disable_scheduler_timer(); 
    }
}

// This is not a general suspend mechanism
// This allows process suspend at init. It decided whether it needs to suspend 
// the process based on ProcessStatus
pub fn suspend_process() {
    // Check if process state is suspended. If so, place thread in suspended state
    let cur_proc = get_current_process().expect("suspend_process() called from idle process!");
    let cur_task = get_current_task().expect("suspend_process() called from idle task!");
    
    // Lock order: Proc -> Task
    let proc_guard = cur_proc.lock();

    // If not true, it likely means that resume_process got called before we could reach here
    if proc_guard.get_status() == ProcessStatus::Suspended {
        let mut task_guard = cur_task.lock();
        match task_guard.status {
            TaskStatus::Terminated => {
                // TERMINATED > SUSPENDED
                crate::sched_log!("Suspend process {} failed since task is terminated", proc_guard.get_id());
                return;  
            },
            TaskStatus::Running => {
                crate::sched_log!("Suspend process {} suspended task {}", proc_guard.get_id(), task_guard.id);
                task_guard.status = TaskStatus::Suspended;
            },
            _ => {
                panic!("suspend_process called on task with invalid state = {:?}!", task_guard.status);
            }
        }
        drop(task_guard);
        drop(proc_guard);
        yield_cpu();
    }
}


pub fn resume_process(pid: usize) -> bool {
    let this_proc_res = get_process_info(pid);
    if this_proc_res.is_none() {
        return false;
    }
    
    let this_proc = this_proc_res.unwrap();
    let res = {
        let mut proc_guard = this_proc.lock();
        if proc_guard.get_status() == ProcessStatus::Suspended {
            assert!(proc_guard.get_num_threads() == 1);
            crate::sched_log!("Resuming process {}", proc_guard.get_id());
            proc_guard.set_status(ProcessStatus::Ready);
        }

        proc_guard.get_first_task()
    };
    if res.is_none() {
        return false;
    }

    let task_id = res.unwrap();
    let this_task_res = get_task_info(task_id);
    
    // Task could have been killed
    if this_task_res.is_none() {
        return false;
    }
    let this_task = this_task_res.unwrap();
    let core = this_task.lock().core;

    // Lock order: Sched -> Task
    let mut sched_cb = unsafe { SCHEDULER_CON_BLK.get(core).task_queue.lock() };

    // If task was suspended, remove from suspend list
    if this_task.lock().status == TaskStatus::Suspended {
        let id = this_task.lock().id;
        let task = sched_cb.suspended_tasks.find_and_remove(|t| {t.lock().id == id});
        if task.is_some() {
            let task_inner = ListNode::into_inner(task.unwrap());
            sched_cb.active_tasks.insert_node_at_head(task_inner);
            crate::sched_log!("Resumed task {} from suspend list", id);
            this_task.lock().status = TaskStatus::Active;
        }
        else {
            // Scheduler did not get chance to put the suspended task into suspended queue
            // So just revert state back to running
            crate::sched_log!("Resumed task {} directly", id);
            this_task.lock().status = TaskStatus::Running;
        }
        notify_other_cpu(core);
        true
    }
    else {
        false
    }
}

fn setup_signal_handler(
    signal: u8,
    user_ctx: *mut c_void,
    handler: DispatchRoutine,
    task: &mut Task
) {
    debug!("setup_signal_handler: signal={} task_id={}", signal, task.id);
    let signal_frame = SignalFrame::new(signal, user_ctx, handler);
    task.signal_frame_init[signal as usize] = Some(signal_frame);
}

// Signal handling must not be ongoing or in init phase
fn check_signal_validity(task: &mut Task, signal: u8) -> bool {
    (task.in_signal_init & (1 << signal) == 0) && (task.pending_signals & (1 << signal) == 0)
}

fn do_issue_signal_to_eligible_thread(
    handler: DispatchRoutine,
    signal: u8,
    user_ctx: *mut c_void,
    state: Option<TaskStatus>,
    thread_list: &Vec<usize>
) -> bool {
    let mut waiting_desc = None;
    for &tid in thread_list {
        let res = get_task_info(tid);
        if let Some(task) = res {
            let mut guard = task.lock();
            let valid = check_signal_validity(&mut *guard, signal);
            debug!("do_issue_signal: tid={} status={:?} valid={}", tid, guard.status, valid);

            if !valid {
                continue;
            }

            if state.is_none() || guard.status == *state.as_ref().unwrap() {
                match guard.status {
                    TaskStatus::Terminated => {
                        continue;
                    },
                    TaskStatus::Suspended => {
                        panic!("Task in suspended state in signal issue path!");
                    },
                    _ => {
                        debug!("do_issue_signal: setting up signal {} on tid={} (status={:?})", signal, tid, guard.status);
                        guard.in_signal_init |= 1 << signal;
                        setup_signal_handler(
                            signal,
                            user_ctx,
                            handler,
                            &mut *guard
                        );
                        if guard.status == TaskStatus::WaitingInterruptible {
                            debug!("do_issue_signal: waking interruptible tid={}", tid);
                            let wait_sem = (*guard.wait_semaphores.first().unwrap()).clone();
                            waiting_desc = Some((task.clone(), wait_sem));
                            break;
                        }
                        else {
                            return true;
                        }
                    }
                }
            }
        }
    }

    if let Some((task, wait_sem)) = waiting_desc {
        let task_id = task.lock().id;
        KSem::from(wait_sem).signal_task_interrupted(task_id);
        return true;
    }

    false
}

fn check_process_signal_validity(pid: usize) -> Option<KProcess> {
    let res = get_process_info(pid);
    if res.is_none() {
        return None;
    }

    let this_proc = res.unwrap();
    assert!(this_proc.lock().get_user_flag());

    // If process about to be suspended or terminated, then don't issue signal
    if this_proc.lock().get_status() == ProcessStatus::Ready {
        Some(this_proc)
    }
    else {
        None
    }
}

fn unset_signal_from_process(this_proc: KProcess, signal: u8) {
    let mut guard = this_proc.lock();
    let mut signal_mask = guard.get_pending_signals();
    signal_mask &= !(1 << signal);
    guard.set_pending_signals(signal_mask);
}

fn proc_do_issue_signal(this_proc: KProcess, action: SignalHandler, signal: u8) -> bool {
    let pid = this_proc.lock().get_id();
    let SignalHandler{handler, user_ctx} = action;

    const PRIORITY_LIST: [TaskStatus; 4] = [
            TaskStatus::Running,
            TaskStatus::Active,
            TaskStatus::WaitingInterruptible,
            TaskStatus::Waiting
        ];

    let threads = this_proc.lock().get_threads_snapshot();
    for state in PRIORITY_LIST {
        let res = do_issue_signal_to_eligible_thread(
            handler,
            signal,
            user_ctx,
            Some(state),
            &threads
        );

        if res {
            debug!("issue_signal: signal {} delivered to pid={} (state={:?})", signal, pid, state);
            return true;
        }
    }
    
    debug!("issue_signal: no eligible thread found for pid={} signal={}", pid, signal);
    false 
}

pub fn issue_signal_to_thread(tid: usize, signal: u8) {
    debug!("issue_signal_to_thread: tid={} signal={}", tid, signal);
    let task_opt = get_task_info(tid);
    if task_opt.is_none() {
        return;
    }

    let task = task_opt.unwrap();
    let proc = task.lock().get_process().unwrap();
    let pid = proc.lock().get_id();
    if check_process_signal_validity(pid).is_none() {
        return;
    }
    disable_preemption();

    let handler_opt = proc.lock().get_signal_handler(signal);
    
    // Default action is to kill the process (even if thread directed)
    if handler_opt.is_none() {
        debug!("issue_signal_to_thread: Taking default action on signal {}", signal);
        drop(task);
        drop(proc);
        enable_preemption();
        kill_process(pid, -1);
    }
    else {
        let SignalHandler { user_ctx, handler } = handler_opt.unwrap();
        let thread_list = vec![tid];
        do_issue_signal_to_eligible_thread(handler, signal, user_ctx, None, &thread_list);
        enable_preemption();
    }
}

pub fn issue_signal(pid: usize, signal: u8) {
    debug!("issue_signal: pid={} signal={}", pid, signal);
    let proc_res = check_process_signal_validity(pid);

    if proc_res.is_none() {
        debug!("issue_signal: process {} not valid for signalling", pid);
        return;
    }
    let this_proc = proc_res.unwrap();
    // For process, the pending signal mask indicates whether the signal has 
    // been delivered to a thread or not. Whether it has started executing and
    // such is managed by the thread itself.

    // This is a spinlock instead of mutex. Required since issue_signal could 
    // be called from idle task
    let sig_guard = this_proc.lock().get_signal_guard();
    let _guard = sig_guard.lock();
    let action = {
        let mut guard = this_proc.lock();
        let handler_info_opt = guard.get_signal_handler(signal);
        let mut pending_signal_mask = guard.get_pending_signals();  
        let pending_signal = get_highest_set_bit(pending_signal_mask);
        pending_signal_mask |= 1 << signal;
        guard.set_pending_signals(pending_signal_mask);

        // No signal exists, we're free to fire the new signal to an eligible thread
        if pending_signal == -1 {
            handler_info_opt
        }
        else {
            let pending_signal = pending_signal as u8;
            // This signal is already in queue, don't do anything further
            if pending_signal == signal {
                debug!("signal already pending in process");
                return;
            }
            else if pending_signal > signal {
                debug!("current signal is lower prio than existing one");
                // If this signal is lower priority, queue it
                return;
            }
            else {
                // New signal priority is higher, send this to one of the threads
                handler_info_opt
            }
        }
    };

    // No handler installed, take default action
    // For now, its just to kill the process
    disable_preemption();
    if action.is_none() {
        debug!("issue_signal: Taking default action for signal {}", signal);
        unset_signal_from_process(this_proc, signal);
        
        // It could be that we're killing this process, so drop the signal_guard explicitly
        drop(_guard);
        drop(sig_guard);
        enable_preemption();
        kill_process(pid, -1);
    }
    else {
        // We have found an eligible thread to issue the signal to
        if proc_do_issue_signal(this_proc.clone(), action.unwrap(), signal) {
            unset_signal_from_process(this_proc, signal);
        }
        else {
            debug!("issue_signal: Failed to issue signal {} to process", signal);
        }
        enable_preemption();
    }
}

// Always called from signal handler context
// If user calls sigreturn from another context, we kill the process
pub fn complete_signal() -> ! {
    disable_preemption();
    let this_proc = {
        let task = get_current_task().expect("complete_signal called from idle task");
        let mut guard = task.lock();
        let pend_signal = get_highest_set_bit(guard.pending_signals);
        if pend_signal == -1 {
            debug!("complete_signal: no pending signal to complete for task {}", guard.id);
            drop(guard);
            enable_preemption();
            exit_process(-1);
        }
        debug!("complete_signal: marking signal {} as completed for task {}", pend_signal, guard.id);
        guard.completed_signals |= 1 << pend_signal;
        guard.pending_signals &= !(1 << pend_signal);
        guard.get_process().unwrap()
    };

    // Now check if there are any pending signals in this process and fire them
    let sig_guard = this_proc.lock().get_signal_guard();
    let _guard = sig_guard.lock();
    let (pid, mut pending_signal_mask) = {
        let guard = this_proc.lock();
        let pid = guard.get_id();
        let pending_signal_mask = guard.get_pending_signals();
        (pid, pending_signal_mask)
    };

    let mut pending_signal = get_highest_set_bit(pending_signal_mask);
    let mut kill_signal = false;
    while pending_signal != -1 {
        let signal = pending_signal as u8;
        let handler_opt = this_proc.lock().get_signal_handler(signal);
        // Take default action
        if handler_opt.is_none() {
            debug!("complete_signal: Taking default action for signal {}", signal);
            pending_signal_mask &= !(1 << signal);
            kill_signal = true;
            break;
        }
        else {
            debug!("complete_signal: Trying to deliver signal {} to eligible thread", signal);
            if !proc_do_issue_signal(this_proc.clone(), handler_opt.unwrap(), signal) {
                // We tried to issue high prio signal but failed, stop issuing further signals
                debug!("complete_signal: Failed to issue signal {}", signal);
                break;
            }
        }

        pending_signal_mask &= !(1 << signal);
        pending_signal = get_highest_set_bit(pending_signal_mask);
    }

    this_proc.lock().set_pending_signals(pending_signal_mask);
    drop(_guard);
    drop(sig_guard);
    drop(this_proc);
    enable_preemption();
    if kill_signal {
        kill_process(pid, -1);
    }

    yield_cpu();
    panic!("Reached end of complete_signal!");
}


extern "C" fn idle_task() -> ! {
    hal::sleep();
}

fn notify_other_cpu(target_core: usize) {
    if hal::get_core() == target_core {
        enable_scheduler_timer();
        return;
    }

    crate::sched_log!("Notifying core {}", target_core);
    hal::notify_core(IPIRequestType::SchedChange, target_core);
}

// handler = Function to execute in kernel mode once thread starts
// user_function = Function to execute in user mode (Usually provided as argument by create_user_thread)
fn create_thread_common(handler: DispatchRoutine, user_function: Option<DispatchRoutine>) -> Result<(KThread, usize), KError> {
    // We will use simple round robin to determine the cpu which gets this task
    let core = TASK_CPU.fetch_add(1, Ordering::Relaxed) as usize % get_total_cores();   
    let task = Task::new(true, core, user_function)?;
    
    {
        let mut task = task.lock();
        let stack_base = task.stack.as_ref().unwrap().get_stack_base();

        // Setup the initial context
        let context = create_kernel_context(handler, stack_base as *mut u8);
        task.context = context;  
        task.panic_base = stack_base;
    }

    Ok((task, core))
}

// Internal API: Do not call this
pub fn create_init_thread(
    handler: DispatchRoutine, 
    process: KProcess, 
    context_ptr: *mut c_void
) -> Result<KThread, KError> {
    let is_user_thread = process.lock().get_user_flag();
    let proc_id = process.lock().get_id();

    let (thread, core) = if is_user_thread {
        create_thread_common(super::user::user_init_handler, Some(handler))?
    }
    else {
        create_thread_common(handler, None)?
    };

    let thread_id = thread.lock().get_id();
    let proc_addr_space = process.lock().get_vcb();
    crate::sched_log!("Created init thread {} on process {} on core {}", thread_id, proc_id, core);

    thread.lock().arg_context = context_ptr;
    thread.lock().process = Some(process);
    thread.lock().vcb = Some(proc_addr_space);
    Ok(thread)
}

pub fn start_task(thread: &KThread, core: usize, process: &KProcess, registry: &Spinlock<BTreeMap<usize, KProcess>>) -> Result<(), KError> {
    {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock()
        };
        
        let thread_id = thread.lock().get_id();
        
        // Add to ready queue
        sched_cb.active_tasks.add_node(Arc::clone(&thread))?;

        let mut process_inner = process.lock();
        let proc_id = process_inner.get_id();
        process_inner.attach_thread_to_current_process(thread_id)?;
        registry.lock().insert(proc_id, Arc::clone(&process));

        TASKS.lock().insert(thread_id, Arc::clone(&thread));
    }
    notify_other_cpu(core);

    Ok(())
}

pub fn create_thread_do_work(proc_id: Option<usize>, handler: DispatchRoutine, user_fn: Option<DispatchRoutine>, context_ptr: *mut c_void) -> Result<KThread, KError> {
    disable_preemption();

    let (thread, core) = match create_thread_common(handler, user_fn) {
        Ok(v) => v,
        Err(e) => {
            enable_preemption();
            return Err(e);
        }
    };

    let thread_id = thread.lock().get_id();
    let process = if let Some(id) = proc_id {
        get_process_info(id)
    }
    else {
        get_current_process()
    };

    // Lock order => Scheduler -> Process -> Task
    // We compute the setup result inside this block so that all the locks
    // (scheduler, process, task) drop before we call enable_preemption(),
    // which itself acquires the local scheduler lock.
    let setup_result: Result<(), KError> = {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock()
        };

        if let Some(process) = process {
            let process_ref = Arc::clone(&process);
            let mut guard = process.lock();

            assert!(guard.get_user_flag() == user_fn.is_some(), "Thread type mismatch!");

            let proc_addr_space = guard.get_vcb();
            if guard.get_status() == ProcessStatus::Terminated {
                Err(KError::ProcessTerminated)
            }
            else if let Err(e) = guard.attach_thread_to_current_process(thread_id) {
                Err(e)
            }
            else {
                crate::sched_log!("Creating new task {} under process {}", thread_id, guard.get_id());
                thread.lock().arg_context = context_ptr;
                thread.lock().process = Some(process_ref);
                thread.lock().vcb = Some(proc_addr_space);
                drop(guard);

                match sched_cb.active_tasks.add_node(Arc::clone(&thread)) {
                    Ok(_) => {
                        TASKS.lock().insert(thread_id, Arc::clone(&thread));
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        }
        else {
            Err(KError::ProcessTerminated)
        }
    };

    if let Err(e) = setup_result {
        enable_preemption();
        return Err(e);
    }

    notify_other_cpu(core);
    enable_preemption();

    Ok(thread)
}

// Must be called from valid process context
pub fn create_thread(handler: DispatchRoutine, context_ptr: *mut c_void) -> Result<KThread, KError> {
    let res = create_thread_do_work(None, handler, None, context_ptr);
    if res.is_err() {
        info!("Failed to create kernel thread");
    }

    res
}

// Create a new worker thread under kernel process (which is always going to be process 0 and is guaranteed to exist)
pub fn create_system_thread(handler: DispatchRoutine, context_ptr: *mut c_void) -> Result<KThread, KError> {
    let res = create_thread_do_work(Some(0), handler, None, context_ptr);
    if res.is_err() {
        info!("Failed to create system thread");
    }

    res
}

pub fn get_current_thread_args() -> *mut c_void {
    match get_current_task() {
        Some(t) => t.lock().arg_context,
        None => null_mut(),
    }
}

impl Spinlock<Task> {
    // Blocks caller until thread terminates
    pub fn wait(&self, is_interruptible: bool) -> bool {
        let sem = {
            let task = self.lock();
            task.term_notify.clone() 
        };

        sem.wait(is_interruptible).is_ok()
    }
    
    pub(super) fn get_inner_sem(&self) -> KSemInnerType {
        self.lock().term_notify.inner()
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_get_cur_thread_arg_ffi() -> *mut c_void {
    let task = get_current_task().expect("sched_get_cur_thread_arg() called from idle task!");
    task.lock().arg_context
} 

#[unsafe(no_mangle)]
extern "C" fn sched_get_cur_thread_id_ffi() -> usize {
    get_current_task_id().expect("sched_get_cur_thread_id() called from idle task!")
}   

#[unsafe(no_mangle)]
extern "C" fn sched_create_thread_ffi(
    handler: DispatchRoutine,
    context_ptr: *mut c_void,
) -> usize {
    match create_thread(handler, context_ptr) {
        Ok(thread) => thread.lock().get_id(),
        Err(_) => usize::MAX,
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_exit_thread_ffi(exit_code: isize) -> ! {
    exit_thread(exit_code)
}

#[unsafe(no_mangle)]
extern "C" fn sched_kill_thread_ffi(thread_id: usize, exit_code: isize) {
    kill_thread(thread_id, exit_code)
}

// Do not call this function from interrupt context
pub fn delay_ms(value: usize, is_interruptible: bool) -> bool {
    let timer = KSem::new(0, 1);

    let res = timer.wait_with_timeout(value, is_interruptible);
    match res {
        Ok(()) => {
            panic!("delay_ms() semaphore signalled in normal path??");
        },
        Err(e) => {
            e == KError::WaitTimedOut
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn sched_delay_ms_ffi(value: usize) {
    delay_ms(value, false);
}
