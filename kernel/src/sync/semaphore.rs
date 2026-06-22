use core::ptr::NonNull;
use alloc::sync::Arc;
use kernel_intf::KError;
use super::Spinlock;
use crate::hal;
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::list::{List, DynList};
use crate::sched::{self, KThread, SignalCause, is_preemption_enabled};

pub type KSemInnerType = Arc<Spinlock<KSemInner>, PoolAllocatorGlobal>;

enum SemState {
    Semaphore { max_count: isize, counter: isize },
    Event { signalled: bool, is_auto_reset: bool }
}

pub struct KSemInner {
    state: SemState,
    blocked_list: DynList<KThread>
}

enum Wake {
    None,
    One,
    All
}

// True indicates that the lock was signalled
// False means that semaphore got signalled due to timeout
fn do_wait(inner_arc: &KSemInnerType, timeout: Option<usize>, is_interruptible: bool) -> Result<(), KError> {
    let int_enabled = hal::are_interrupts_enabled();
    let preemption_enabled = is_preemption_enabled();
    let in_dw = sched::is_in_dw_mode();

    let mut yield_flag = false;
    let mut wait_failed = false;
    let mut wait_failed_interrupt = false;
    {
        let mut inner = inner_arc.lock();
        let mut zero_timeout = false; 
        // A timeout of 0 is a special case
        // In this, we return true if the semaphore won't block (like a try_wait() fn)
        if let Some(timer_count) = &timeout {
            if *timer_count == 0 {
                zero_timeout = true;
            }   
        }

        let cur_task = sched::get_current_task()
        .expect("wait() called from idle task!!");

        // Reset to normal state
        cur_task.lock().set_signal_cause(SignalCause::Normal);

        let should_block = match &mut inner.state {
            SemState::Semaphore { counter, .. } => {
                if zero_timeout {
                    // Consume on success
                    if *counter >= 1 {
                        *counter -= 1;
                        return Ok(());
                    }
                    else {
                        return Err(KError::WaitTimedOut);
                    }
                }

                *counter -= 1;
                *counter < 0
            },
            SemState::Event { signalled, is_auto_reset } => {
                if zero_timeout {
                    let success = *signalled;
                    // If manual reset, we must not set signalled back to false
                    if success && *is_auto_reset {
                        *signalled = false;
                    }
                    if success {
                        return Ok(());
                    }
                    else {
                        return Err(KError::WaitTimedOut);
                    }
                }

                if *signalled {
                    // Auto-reset events consume the signal here; manual-reset
                    // events leave it raised for all subsequent waiters.
                    if *is_auto_reset {
                        *signalled = false;
                    }
                    false
                } else {
                    true
                }
            }
        };

        if should_block {
            assert!(!in_dw, "wait() called while in DW mode — only driver workers may run");
            assert!(int_enabled, "wait() would block with interrupts disabled — deadlock risk");
            assert!(preemption_enabled, "wait() would block with preemption disabled — deadlock risk");

            let inner_wrap = Arc::clone(inner_arc);

            let res = sched::add_cur_task_to_wait_queue(inner_wrap, timeout, is_interruptible);
            if res.is_none() {
                inner.blocked_list.add_node(cur_task).expect("Failed to add semaphore to blocked list!");
                yield_flag = true;
            }
            else {
                // Roll back the changes
                match &mut inner.state {
                    SemState::Semaphore { counter, .. } => {
                        *counter += 1;
                    },
                    _ => {}
                };
                match res.unwrap() {
                    SignalCause::Interruption => { wait_failed_interrupt = true },
                    SignalCause::Normal => { wait_failed = true },
                    _ => {}
                }
            }
        }
    }

    // Yield outside the lock
    if yield_flag {
        sched::yield_cpu();
    }

    let task = sched::get_current_task().expect("wait() called in idle task!");
    if wait_failed {
        Err(KError::WaitFailed)
    }
    else if wait_failed_interrupt {
        Err(KError::WaitInterrupted)
    }
    else {
        match task.lock().get_signal_cause() {
            SignalCause::Normal => {
                Ok(())
            },
            SignalCause::TimerExpiry => {
                Err(KError::WaitTimedOut)
            },
            SignalCause::Interruption => {
                Err(KError::WaitInterrupted)
            }
        }
    }
}

fn do_signal(inner_arc: &KSemInnerType) {
    let mut inner = inner_arc.lock();

    let wake = match &mut inner.state {
        SemState::Semaphore { max_count, counter } => {
            *counter = (*max_count).min(*counter + 1);
            if *counter <= 0 { Wake::One } else { Wake::None }
        },
        SemState::Event { signalled, is_auto_reset } => {
            if *is_auto_reset {
                Wake::One
            } else {
                Wake::All
            }
        }
    };

    match wake {
        Wake::None => {},
        Wake::One => {
            if !wake_task(&mut inner, inner_arc, None, false) {
                if let SemState::Event { signalled, is_auto_reset } = &mut inner.state {
                    if *is_auto_reset {
                        *signalled = true;
                    }
                }
            }
        },
        Wake::All => {
            while wake_task(&mut inner, inner_arc, None, false) {}
            if let SemState::Event { signalled, .. } = &mut inner.state {
                *signalled = true;
            }
        }
    };
}

fn do_signal_on_task(inner_arc: &KSemInnerType, task_id: usize, is_interruptible: bool) {
    let mut inner = inner_arc.lock();
    if wake_task(&mut inner, inner_arc, Some(task_id), is_interruptible) {
        // The timer expiry caused this semaphore to be signalled.
        // Restore the unit the semaphore wait decremented at entry
        if let SemState::Semaphore { max_count, counter } = &mut inner.state {
            *counter = (*max_count).min(*counter + 1);
        }
    }
}

fn wake_task(inner: &mut KSemInner, inner_arc: &KSemInnerType, task_id: Option<usize>, is_interruptible: bool) -> bool {
    if is_interruptible {
        assert!(task_id.is_some());
    }
    let wait_task_ptr = if let Some(id) = &task_id {
        inner.blocked_list.iter().find(|t| {
            t.lock().get_id() == *id
        }).map(|t| {
            NonNull::from(t)
        })
    }
    else {
        // If there is no specific task id, then just return the head of the queue (if it exists)
        let res = inner.blocked_list.first();
        if res.is_none() {
            None
        }
        else {
            Some(NonNull::from(res.unwrap()))
        }
    };
    
    if let Some(waiting_task) = wait_task_ptr {
        let node = unsafe {
            &*waiting_task.as_ptr()
        };
        
        
        let id = node.lock().get_id();
        let cause = if is_interruptible {
            SignalCause::Interruption
        }
        else if task_id.is_some() {
            SignalCause::TimerExpiry
        }
        else {
            SignalCause::Normal
        };

        if sched::signal_waiting_task(id, Arc::clone(inner_arc), cause) {
            unsafe {
                inner.blocked_list.remove_node(waiting_task)
            };
            return true;
        }
    }

    false
}

pub struct KSem {
    inner: KSemInnerType
}

unsafe impl Sync for KSem {}
unsafe impl Send for KSem {}

impl KSem {
    pub fn new(init_count: isize, max_count: isize) -> Self {
        Self {
            inner: Arc::new_in(Spinlock::new(KSemInner {
                state: SemState::Semaphore { max_count, counter: init_count },
                blocked_list: List::new()
            }), PoolAllocatorGlobal)
        }
    }

    pub fn from(inner: KSemInnerType) -> Self {
        Self {
            inner
        }
    }

    pub fn wait(&self, is_interruptible: bool) -> Result<(), KError> {
        do_wait(&self.inner, None, is_interruptible)
    }

    pub fn wait_with_timeout(&self, timeout: usize, is_interruptible: bool) -> Result<(), KError> {
        do_wait(&self.inner, Some(timeout), is_interruptible)
    }

    pub fn signal(&self) {
        do_signal(&self.inner);
    }

    pub fn signal_task_on_timeout(&self, task_id: usize) {
        do_signal_on_task(&self.inner, task_id, false);
    }

    pub fn signal_task_interrupted(&self, task_id: usize) {
        do_signal_on_task(&self.inner, task_id, true);
    }

    pub fn drop_task(inner_arc: KSemInnerType, task_id: usize) {
        let mut inner = inner_arc.lock();

        let mut blocked_task = None;
        for task in inner.blocked_list.iter() {
            if task.lock().get_id() == task_id {
                blocked_task = Some(NonNull::from(task));

                // The number of waiters have reduced by 1. Update state
                match &mut inner.state {
                    SemState::Semaphore { max_count, counter } => {
                        *counter = (*max_count).min(*counter + 1);
                    },
                    _ => {
                        // This state should explicitly be controlled by signal/wait calls only
                    }
                }

                break;
            }
        }

        // The list might not contain the waiting task
        // This might happen if another task called signal on this semaphore before drop_task got a chance to run
        if blocked_task.is_some() {
            crate::sched_log!("Dropping semaphore for task {}", task_id);
            unsafe {
                inner.blocked_list.remove_node(blocked_task.unwrap());
            }
        }
    }
}

impl Clone for KSem {
    fn clone(&self) -> Self {
        KSem {
            inner: Arc::clone(&self.inner)
        }
    }
}

pub struct KEvent {
    inner: KSemInnerType
}

unsafe impl Sync for KEvent {}
unsafe impl Send for KEvent {}

impl KEvent {
    pub fn new(is_auto_reset: bool) -> Self {
        Self {
            inner: Arc::new_in(Spinlock::new(KSemInner {
                state: SemState::Event { signalled: false, is_auto_reset },
                blocked_list: List::new()
            }), PoolAllocatorGlobal)
        }
    }

    pub fn wait(&self, is_interruptible: bool) -> Result<(), KError> {
        do_wait(&self.inner, None, is_interruptible)
    }
    
    pub fn wait_with_timeout(&self, timeout: usize, is_interruptible: bool) -> Result<(), KError> {
        do_wait(&self.inner, Some(timeout), is_interruptible)
    }

    pub fn signal(&self) {
        do_signal(&self.inner);
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        if let SemState::Event { signalled , .. } = &mut inner.state {
            *signalled = false;
        }
    }
}

impl Default for KEvent {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Clone for KEvent {
    fn clone(&self) -> Self {
        KEvent {
            inner: Arc::clone(&self.inner)
        }
    }
}

pub struct ConfigGuard<'a> {
    sem: &'a KSem
}

impl Drop for ConfigGuard<'_> {
    fn drop(&mut self) {
        self.sem.signal();
    }
}

pub fn semaphore_guard(sem: &KSem) -> ConfigGuard<'_> {
    sem.wait(false);
    ConfigGuard { sem }
}
