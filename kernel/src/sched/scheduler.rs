use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use common::{MemoryRegion, PAGE_SIZE};
use core::ptr::NonNull;
use core::mem::take;
use core::sync::atomic::{AtomicU8, AtomicIsize, AtomicUsize, Ordering};
use core::ffi::c_void;
use core::ptr::null_mut;
use super::{DispatchRoutine, KProcess, ProcessStatus, get_current_process, get_process_info, KTimerInnerType};
use crate::cpu::{self, MAX_CPUS, PerCpu, Stack, get_panic_base, get_total_cores, get_worker_stack, set_panic_base};
use crate::hal::{self, IPIRequestType, create_kernel_context, disable_scheduler_timer, enable_scheduler_timer, fetch_context, get_per_cpu_base, get_per_cpu_data, get_per_cpu_kernel_base, set_per_cpu_base, set_per_cpu_data, switch_context};
use crate::mem::{VCB, get_kernel_addr_space, set_address_space};
use crate::sync::{KEvent, KSem, KSemInnerType, Spinlock};
use crate::io::{self, IrpPtr, deallocate_irp};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::list::{List, ListNode, DynList};
use kernel_intf::driver::{DeviceObject, Irp, IrpMajor, IrpMinor, IrpResult, Status};
use kernel_intf::{acquire_spinlock, release_spinlock};
use kernel_intf::{KError, debug, info};

#[cfg(target_arch = "x86_64")]
use crate::hal::{get_per_cpu_kernel_base_for_core, set_tss_stack};

// This is in milliseconds
pub const QUANTUM: usize = 10;
const INIT_QUANTA: usize = 10;

pub type KThread = Arc<Spinlock<Task>, PoolAllocatorGlobal>;

static TASK_ID: AtomicUsize = AtomicUsize::new(0);
static TASK_CPU: AtomicU8 = AtomicU8::new(0);
static TASKS: Spinlock<BTreeMap<usize, KThread>> = Spinlock::new(BTreeMap::new());

const _: () = {
    assert!(u8::MAX as usize + 1 >= MAX_CPUS);
};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TaskStatus {
    RUNNING,
    ACTIVE,
    WAITING,
    TERMINATED
}

pub struct Task {
    id: usize,
    is_kernel_mode: bool,
    core: usize,
    stack: Option<Stack>,
    status: TaskStatus,
    context: usize,
    quanta: usize,
    panic_base: usize,
    user_fn: Option<DispatchRoutine>,
    wait_semaphores: DynList<KSemInnerType>,
    issued_irps: DynList<IrpPtr>,
    term_notify: KSem,
    process: Option<KProcess>,
    vcb: Option<VCB>,
    pub context_ptr: *mut c_void,
    exit_code: AtomicIsize,
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
            core,
            stack,
            status: TaskStatus::ACTIVE,
            context: 0,
            quanta: INIT_QUANTA,
            panic_base: 0,
            user_fn,
            wait_semaphores: List::new(),
            issued_irps: List::new(),
            term_notify: KSem::new(0, 1),
            process: None,
            vcb: None,
            context_ptr: null_mut(),
            exit_code: AtomicIsize::new(0),
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

    pub fn get_status(&self) -> TaskStatus {
        self.status
    }
    
    pub fn get_core(&self) -> usize {
        self.core
    }

    pub fn get_stack(&self) -> Option<usize> {
        Some(self.stack.as_ref()?.get_stack_base())
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

    pub fn register_irp(&mut self, irp: IrpPtr) {
        self.issued_irps.add_node(irp).expect("Failed to register IRP with task");
    }

    pub fn find_and_remove_irp(&mut self, irp: *mut Irp) {
        self.issued_irps.find_and_remove(|&p| p == irp);
    }

    pub fn take_irp_list(&mut self) -> DynList<IrpPtr> {
        take(&mut self.issued_irps)
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
    task_queue: Spinlock<TaskQueue>
} 

pub struct TaskQueue {
    active_tasks: DynList<KThread>,
    waiting_tasks: DynList<KThread>,
    terminated_tasks: DynList<KThread>,
    notifier_list: DynList<KSem>,
    timer_list: DynList<KTimerInnerType>,
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
            notifier_list: List::new(),
            timer_list: List::new(),
            running_task: None,
            idle_task_stack: NonNull::dangling(),
            leftover_stack: List::new(),
            flip_flop: false
        }
    }
}

static SCHEDULER_CON_BLK: PerCpu<SchedulerCB> = PerCpu::new_with(
    [const {SchedulerCB{preemption_count: AtomicUsize::new(0), task_queue: Spinlock::new(TaskQueue::new())}}; MAX_CPUS]
);

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

    hal::yield_cpu();
}

pub fn is_preemption_enabled() -> bool {
    SCHEDULER_CON_BLK.local().preemption_count.load(Ordering::Acquire) == 0
}

pub fn init() {
    let init_task = Task::new(false, 0, None)
    .expect("Init task creation failed!!");
    
    let init_proc = get_process_info(0).expect("Unable to locate init process!");
    
    TASKS.lock().insert(0, Arc::clone(&init_task));

    init_task.lock().status = TaskStatus::RUNNING;
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

// Set task to waiting and add the timer atomically
pub fn add_cur_task_to_wait_queue_with_timer(wait_semaphore: KSemInnerType, timer: KTimerInnerType) -> bool {
    let mut sched_cb = SCHEDULER_CON_BLK.local().task_queue.lock();
    let cb = sched_cb.running_task;
    if cb.is_none() {
        panic!("add_cur_task_to_wait_queue_with_timer() called from idle task!!");
    }
    
    let cur_task = unsafe { &**cb.unwrap().as_ptr() };
    let mut task = cur_task.lock();
    
    // TERMINATED > WAITING, don't do anything
    if task.status == TaskStatus::TERMINATED {
        return false;
    }
    
    let res = sched_cb.timer_list.add_node(timer);
    if res.is_err() {
        return false;
    }

    assert!(task.wait_semaphores.get_nodes() == 0);
    let res = task.wait_semaphores.add_node(wait_semaphore);
    if res.is_err() {
        sched_cb.timer_list.pop_node();
        return false;
    }

    task.status = TaskStatus::WAITING;
    true
}

pub fn add_cur_task_to_wait_queue(wait_semaphore: KSemInnerType) -> bool {
    let sched_cb = SCHEDULER_CON_BLK.local().task_queue.lock();
    let cb = sched_cb.running_task;
    if cb.is_none() {
        panic!("add_cur_task_to_wait_queue() called from idle task!!");
    }
    
    let cur_task = unsafe { &**cb.unwrap().as_ptr() };

    let mut task = cur_task.lock();
    
    // TERMINATED > WAITING, don't do anything
    if task.status == TaskStatus::TERMINATED {
        return false;
    }
    
    assert!(task.wait_semaphores.get_nodes() == 0);

    let res = task.wait_semaphores.add_node(wait_semaphore);
    if res.is_err() {
        return false;
    }

    task.status = TaskStatus::WAITING;
    true
}

fn remove_wait_semaphore(task: &mut Task, wait_semaphore: KSemInnerType) {
    let mut sem = None;
    let sem_val = (&*wait_semaphore) as *const _;

    assert!(task.wait_semaphores.get_nodes() == 1);

    for semaphore in task.wait_semaphores.iter() {
        let val = (&***semaphore) as *const _;

        if val == sem_val {
            sem = Some(NonNull::from(semaphore));
            break;
        } 
    }

    if sem.is_some() {
        unsafe {
            task.wait_semaphores.remove_node(sem.unwrap());
        }
    }
}


pub fn signal_waiting_task(task_id: usize, wait_semaphore: KSemInnerType) {
    let this_task = get_task_info(task_id);

    // It could be that this task has been killed
    if this_task.is_none() {
        return;
    }

    let this_task = this_task.unwrap();
    let core = this_task.lock().core;
    let mut skip_notify = false;
    disable_preemption();
    {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock() 
        };

        let status = this_task.lock().status;
        match status {
            TaskStatus::WAITING => {
                let mut waiting_task = None;
                for task in sched_cb.waiting_tasks.iter() {
                    if task.lock().get_id() == task_id {
                        waiting_task = Some(NonNull::from(task));
                        break;
                    }
                }
                
                let mut task = this_task.lock();
                // This happens when signal task is called even before the waiting task gets a chance to be put into the wait queue
                if waiting_task.is_none() {
                    // Let task run again with high priority
                    task.status = TaskStatus::RUNNING;
                    task.quanta = INIT_QUANTA;
                }
                else {
                    let signal_task = unsafe {
                        ListNode::into_inner(sched_cb.waiting_tasks.remove_node(waiting_task.unwrap()))
                    };
                    
                    sched_cb.active_tasks.insert_node_at_head(signal_task);
                    task.status = TaskStatus::ACTIVE;
                }

                remove_wait_semaphore(&mut *task, wait_semaphore);
            },

            TaskStatus::TERMINATED => {
                skip_notify = true;
            },

            TaskStatus::ACTIVE | TaskStatus::RUNNING => {
                panic!("Signalled task {} which was in ACTIVE/RUNNING state??", task_id);
            }
        }
    }
    
    if !skip_notify {
        notify_other_cpu(core);
    }

    enable_preemption();
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

    if this_task.is_none() {
        return;
    }

    let this_task  = this_task.unwrap();
    let core = this_task.lock().core;
    
    disable_preemption();

    assert!(task_id != 0, "Attempted to kill init task!!!");
    let sweep_irps;
    {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock()
        };

        let status = {
            let mut task_locked = this_task.lock();
            let status = task_locked.status;
            task_locked.status = TaskStatus::TERMINATED;
            task_locked.exit_code.store(exit_code, Ordering::Relaxed);
            status
        };
        sweep_irps = status != TaskStatus::TERMINATED;

        // Remove task from active list and add to terminated list
        match status {
            TaskStatus::ACTIVE => {
                let mut task_l = None;
                for active_task in sched_cb.active_tasks.iter() {
                    if active_task.lock().id == task_id {
                        task_l = Some(NonNull::from(active_task));
                        break;
                    }
                }

                assert!(task_l.is_some());
                let task_node = unsafe {
                    ListNode::into_inner(sched_cb.active_tasks.remove_node(task_l.unwrap()))
                };

                sched_cb.terminated_tasks.insert_node_at_tail(task_node);
            },

            TaskStatus::WAITING => {
                let mut task_l = None;
                for waiting_task in sched_cb.waiting_tasks.iter() {
                    if waiting_task.lock().id == task_id {
                        task_l = Some(NonNull::from(waiting_task));
                        break;
                    }
                }
                
                if task_l.is_some() {
                    let task_node = unsafe {
                        ListNode::into_inner(sched_cb.waiting_tasks.remove_node(task_l.unwrap()))
                    };

                    sched_cb.terminated_tasks.insert_node_at_tail(task_node);

                }
                else {
                    // Task might not have been scheduled out
                    // In this case, let scheduler take care of it

                    // We might be here due to interrupt in same cpu / kill task issued by different cpu
                }

                drop_task = true;
            },

            TaskStatus::RUNNING => {
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

            TaskStatus::TERMINATED => {
                info!("Task {} already terminated..", task_id);
                skip_notify = true;
                yield_flag = hal::get_core() == core;
            }
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
            dev.pending_irps.lock().find_and_remove(|&p| p == irp);
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
            dev.pending_irps.lock().add_node(irp_raw)
                .expect("Failed to register IRP with device pending list");
        }
    }

    cur_thread.lock().register_irp(irp_raw);
    enable_preemption();

    irp_raw
}
 
pub fn cancel_irp(irp_ptr: IrpPtr) {
    let irp = unsafe { &mut *irp_ptr };
    unsafe { acquire_spinlock(&mut irp.cancel_lock); }

    // Either another path cancelled this explicitly, or the driver did
    // not (a) get a chance yet to register the cancel routine, or
    // (b) decided that cancellation was not required for this request.
    if irp.cancel_routine.is_none() || irp.is_cancelled {
        unsafe { release_spinlock(&mut irp.cancel_lock); }
        return;
    }

    (irp.cancel_routine.unwrap())(irp.device, irp_ptr);
    unsafe { release_spinlock(&mut irp.cancel_lock); }
}

fn kill_sweep_irps(task: &KThread) {
    let irp_list = task.lock().take_irp_list();
    for irp in irp_list.iter() {
        cancel_irp(**irp);
    }
}

pub fn exit_thread(exit_code: isize) -> ! {
    let thread_id = get_current_task_id().expect("Attempted to kill idle task!!");

    assert!(is_preemption_enabled(), "exit_thread() called with preemption disabled!");

    kill_thread(thread_id, exit_code);

    panic!("exit_thread() unreachable reached!!");
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

            if is_last_thread_in_proc {
                crate::sched_log!("Adding process {} notifier to notifier list as task {} is terminating", process_guard.get_id(), id);
                sched_cb.notifier_list.add_node(process_guard.get_notify_sem()).expect("Failed to add process notify semaphore to notifier list!");
                sched_cb.notifier_list.add_node(process_guard.get_init_sem()).expect("Failed to add process init semaphore to notifier list!");
            }
        }

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
        let timer = sched_cb.timer_list.first().unwrap();

        let is_done = timer.lock().update_timer_count(QUANTUM);

        if is_done {
            let sem = timer.lock().get_semaphore();
            sched_cb.notifier_list.add_node(sem).expect("Unable to add timer node semaphore into notifier list!");

            unsafe {
                sched_cb.timer_list.remove_node(NonNull::from(timer))
            };
        }
        else {
            let timer_ref = unsafe {
                ListNode::into_inner(sched_cb.timer_list.remove_node(NonNull::from(timer)))
            };
            
            sched_cb.timer_list.insert_node_at_tail(timer_ref);
        }

        idx += 1;
    }
}


fn notify_watchers(notifier_list: &DynList<KSem>) {
    for sem in notifier_list.iter() {
        sem.signal();
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

pub fn toggle_cur_task_kernel_mode() {
    let task = get_current_task()
    .expect("toggle_cur_task_to_kernel_mode() called from idle task!");
    
    let mut guard = task.lock();
    guard.is_kernel_mode = !guard.is_kernel_mode;
}

fn setup_current_task_ptr(sched_cb: &mut TaskQueue) {
    let cur_task_ptr = if sched_cb.running_task.is_some() {
        unsafe {
            let kthread = &(**sched_cb.running_task.as_ref().unwrap().as_ptr()) as *const KThread;
            
            {
                let guard = (*kthread).lock();  
                let stack_addr = if let Some(cur_stack_address) = &guard.stack {
                    cur_stack_address.get_stack_base()
                }
                else {
                    0
                };

                set_per_cpu_data::<16>(stack_addr as u64);
                
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
        0
    };

    unsafe {
        set_per_cpu_data::<24>(cur_task_ptr); 
    }
}

// Main scheduler loop
pub fn schedule() {
    let notifier_list = {
        let sched_cb_cpu = SCHEDULER_CON_BLK.local();
        let mut sched_cb = sched_cb_cpu.task_queue.lock();
        update_timers(&mut sched_cb);
        
        if sched_cb_cpu.preemption_count.load(Ordering::Acquire) > 0 {
            take(&mut sched_cb.notifier_list)
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
                if task_info.status == TaskStatus::WAITING || task_info.status == TaskStatus::TERMINATED ||
                task_info.quanta == 0 {
                    // First choose new task
                    // We create NonNull here so that the node can later be removed
                    let head_task = sched_cb.active_tasks.first().and_then(|item| {
                        Some(NonNull::from(item))
                    });

                    if head_task.is_some() {
                        let mut head_task_info = unsafe {
                            head_task.unwrap().as_ref().lock()
                        };
                        
                        assert!(head_task_info.status == TaskStatus::ACTIVE); 
                        head_task_info.status = TaskStatus::RUNNING;
                        head_task_info.quanta = INIT_QUANTA;
                        let new_context = head_task_info.context;
                        let new_vcb = head_task_info.vcb.expect("VCB is none");

                        let prev_context = fetch_context();
                        task_info.context = prev_context;

                        #[cfg(target_arch = "x86_64")] 
                        {
                            task_info.per_cpu_base = get_per_cpu_base();
                        }

                        if task_info.status == TaskStatus::RUNNING {
                            task_info.status = TaskStatus::ACTIVE; 
                        }

                        // This ensures that list doesn't delete the node. It simply removes it from the list 
                        let head_task = unsafe {
                            ListNode::into_inner(sched_cb.active_tasks.remove_node(head_task.unwrap()))
                        };

                        if task_info.status == TaskStatus::WAITING {
                            sched_cb.waiting_tasks.insert_node_at_tail(current_task);
                        }
                        else if task_info.status == TaskStatus::TERMINATED {
                            sched_cb.terminated_tasks.insert_node_at_tail(current_task);
                        }
                        else {
                            sched_cb.active_tasks.insert_node_at_tail(current_task);
                        }

                        sched_cb.running_task = Some(head_task);

                        switch_address_space(old_vcb, new_vcb);
                        set_panic_base(head_task_info.panic_base);
                        switch_context(new_context);

                        #[cfg(target_arch = "x86_64")]
                        set_per_cpu_base(head_task_info.per_cpu_base);
                    }
                    else {
                        if task_info.status != TaskStatus::RUNNING {
                            let prev_context = fetch_context();
                            task_info.context = prev_context;
                            
                            #[cfg(target_arch = "x86_64")] 
                            {
                                task_info.per_cpu_base = get_per_cpu_base();
                            }

                            if task_info.status == TaskStatus::WAITING {
                                sched_cb.waiting_tasks.insert_node_at_tail(current_task);
                            }
                            else if task_info.status == TaskStatus::TERMINATED {
                                crate::sched_log!("Adding task {} to terminated list", task_info.id);
                                sched_cb.terminated_tasks.insert_node_at_tail(current_task);
                            }

                            prep_idle_task(&mut sched_cb, old_vcb);
                        }
                        else {
                            // No other task to run. Continue with this task
                            task_info.quanta = INIT_QUANTA;
                        }
                    }
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

                    assert!(head_task_info.status == TaskStatus::ACTIVE); 
                    head_task_info.status = TaskStatus::RUNNING;
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
                    switch_context(new_context);
                    
                    #[cfg(target_arch = "x86_64")]
                    set_per_cpu_base(head_task_info.per_cpu_base);
                }
                else {
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
            take(&mut sched_cb.notifier_list)
        }
    };

    notify_watchers(&notifier_list);
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
pub fn create_init_thread(handler: DispatchRoutine, process: KProcess, context_ptr: *mut c_void) -> Result<KThread, KError> {
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

    thread.lock().context_ptr = context_ptr;
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

pub fn create_thread_do_work(handler: DispatchRoutine, user_fn: Option<DispatchRoutine>, context_ptr: *mut c_void) -> Result<KThread, KError> {
    disable_preemption();

    let (thread, core) = match create_thread_common(handler, user_fn) {
        Ok(v) => v,
        Err(e) => {
            enable_preemption();
            return Err(e);
        }
    };

    let thread_id = thread.lock().get_id();
    let cur_process = get_current_process();

    // Lock order => Scheduler -> Process -> Task
    // We compute the setup result inside this block so that all the locks
    // (scheduler, process, task) drop before we call enable_preemption(),
    // which itself acquires the local scheduler lock.
    let setup_result: Result<(), KError> = {
        let mut sched_cb = unsafe {
            SCHEDULER_CON_BLK.get(core).task_queue.lock()
        };

        if let Some(process) = cur_process {
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
                thread.lock().context_ptr = context_ptr;
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
            panic!("create_thread() called from idle task!!");
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
    let res = create_thread_do_work(handler, None, context_ptr);
    if res.is_err() {
        info!("Failed to create kernel thread");
    }

    res
}

pub fn get_current_thread_args() -> *mut c_void {
    match get_current_task() {
        Some(t) => t.lock().context_ptr,
        None => null_mut(),
    }
}

impl Spinlock<Task> {
    // Blocks caller until thread terminates
    pub fn wait(&self) -> Result<(), KError> {
        let sem = {
            let task = self.lock();
            task.term_notify.clone() 
        };

        sem.wait()
    }
}

#[unsafe(no_mangle)]
extern "C" fn sched_get_cur_thread_arg_ffi() -> *mut c_void {
    let task = get_current_task().expect("sched_get_cur_thread_arg() called from idle task!");
    task.lock().context_ptr
} 

#[unsafe(no_mangle)]
extern "C" fn sched_get_cur_thread_id_ffi() -> usize {
    get_current_task_id().expect("sched_get_cur_thread_id() called from idle task!")
}   
