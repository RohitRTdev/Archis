use core::ptr::NonNull;
use alloc::sync::Arc;
use super::Spinlock;
use crate::hal;
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::list::{List, DynList};
use crate::sched::{self, KThread, is_preemption_enabled};

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
    All,
    AutoEvent
}

// True indicates that the lock was signalled
// False means that semaphore got signalled due to timeout
fn do_wait(inner_arc: &KSemInnerType, timeout: Option<usize>) -> bool {
    let int_enabled = hal::are_interrupts_enabled();
    let preemption_enabled = is_preemption_enabled();
    let in_dw = sched::is_in_dw_mode();

    let mut yield_flag = false;
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

        cur_task.lock().reset_expired_timer_status();

        let should_block = match &mut inner.state {
            SemState::Semaphore { counter, .. } => {
                if zero_timeout {
                    // Consume on success
                    if *counter >= 1 {
                        *counter -= 1;
                        return true;
                    }
                    else {
                        return false;
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
                    return success;
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

            if sched::add_cur_task_to_wait_queue(inner_wrap, timeout) {
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
            }
        }
    }

    // Yield outside the lock so the spinlock is released first.
    if yield_flag {
        sched::yield_cpu();
    }

    let task = sched::get_current_task().expect("wait() called in idle task!");
    !task.lock().get_expired_timer_status()
}

// Shared signal route:
// Wake exactly one waiter if the counter is still non-positive after the bump
// Manual-reset event: raise signalled and wake every blocked waiter.
// Auto-reset event: if a waiter is blocked, wake exactly one and leave
// signalled false (the signal is "transferred" to the waker). Otherwise
// raise signalled so the next waiter consumes it on entry.
fn do_signal(inner_arc: &KSemInnerType) {
    let mut inner = inner_arc.lock();

    let wake = match &mut inner.state {
        SemState::Semaphore { max_count, counter } => {
            *counter = (*max_count).min(*counter + 1);
            if *counter <= 0 { Wake::One } else { Wake::None }
        },
        SemState::Event { signalled, is_auto_reset } => {
            if *is_auto_reset {
                Wake::AutoEvent
            } else {
                // Manual-reset stays raised forever (until explicit reset()).
                *signalled = true;
                Wake::All
            }
        }
    };

    match wake {
        Wake::None => {},
        Wake::One => {
            if inner.blocked_list.get_nodes() > 0 {
                wake_task(&mut inner, inner_arc, None);
            }
        },
        Wake::All => {
            while inner.blocked_list.get_nodes() > 0 {
                wake_task(&mut inner, inner_arc, None);
            }
        },
        Wake::AutoEvent => {
            if inner.blocked_list.get_nodes() > 0 {
                // Wake exactly one — signal is consumed by transfer to the waker
                wake_task(&mut inner, inner_arc, None);
            } else if let SemState::Event { signalled, .. } = &mut inner.state {
                // No waiter — raise the flag for the next wait to consume.
                *signalled = true;
            }
        }
    }
}

fn do_signal_on_timer_expire(inner_arc: &KSemInnerType, task_id: usize) {
    let mut inner = inner_arc.lock();

    if wake_task(&mut inner, inner_arc, Some(task_id)) {
        // The timer expiry caused this semaphore to be signalled.
        // Restore the unit the semaphore wait decremented at entry
        if let SemState::Semaphore { max_count, counter } = &mut inner.state {
            *counter = (*max_count).min(*counter + 1);
        }
    }
}

fn wake_task(inner: &mut KSemInner, inner_arc: &KSemInnerType, task_id: Option<usize>) -> bool {
    let wait_task_ptr = if let Some(id) = &task_id {
        inner.blocked_list.iter().find(|t| {
            t.lock().get_id() == *id
        }).map(|t| {
            NonNull::from(t)
        })
    }
    else {
        // If there is no specific task id, then just return the head of the queue
        Some(NonNull::from(inner.blocked_list.first().unwrap()))
    };
    
    if let Some(waiting_task) = wait_task_ptr {
        let node = unsafe {
            inner.blocked_list.remove_node(waiting_task)
        };
        
        let id = node.lock().get_id();
        sched::signal_waiting_task(id, Arc::clone(inner_arc), task_id.is_some());
        return true;
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

    pub fn wait(&self) -> bool {
        do_wait(&self.inner, None)
    }

    pub fn wait_with_timeout(&self, timeout: usize) -> bool {
        do_wait(&self.inner, Some(timeout))
    }

    pub fn signal(&self) {
        do_signal(&self.inner);
    }

    pub fn signal_task(&self, task_id: usize) {
        do_signal_on_timer_expire(&self.inner, task_id);
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
            kernel_intf::debug!("Dropping semaphore for task {}", task_id);
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

    pub fn wait(&self) -> bool {
        do_wait(&self.inner, None)
    }
    
    pub fn wait_with_timeout(&self, timeout: usize) -> bool {
        do_wait(&self.inner, Some(timeout))
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
    sem.wait();
    ConfigGuard { sem }
}
