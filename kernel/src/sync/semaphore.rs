use core::ptr::NonNull;
use alloc::sync::Arc;
use super::Spinlock;
use crate::hal;
use crate::{sched::{self, KTimerInnerType}};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::list::{List, DynList};
use kernel_intf::KError;
use crate::sched::{KThread, is_preemption_enabled};

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

fn do_wait(inner_arc: &KSemInnerType) -> Result<(), KError> {
    let int_enabled = hal::are_interrupts_enabled();
    let preemption_enabled = is_preemption_enabled();

    let mut yield_flag = false;
    {
        let mut inner = inner_arc.lock();

        let cur_task = sched::get_current_task()
        .expect("wait() called from idle task!!");

        let should_block = match &mut inner.state {
            SemState::Semaphore { counter, .. } => {
                *counter -= 1;
                *counter < 0
            },
            SemState::Event { signalled, is_auto_reset } => {
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
            assert!(int_enabled, "wait() would block with interrupts disabled — deadlock risk");
            assert!(preemption_enabled, "wait() would block with preemption disabled — deadlock risk");

            let inner_wrap = Arc::clone(inner_arc);

            if let Err(err) = inner.blocked_list.add_node(cur_task) {
                undo_wait(&mut inner.state);
                return Err(err);
            }

            if !sched::add_cur_task_to_wait_queue(inner_wrap) {
                undo_wait(&mut inner.state);
                inner.blocked_list.pop_node();
                return Err(KError::WaitFailed);
            }

            yield_flag = true;
        }
    }

    // Yield outside the lock so the spinlock is released first.
    if yield_flag {
        sched::yield_cpu();
    }

    Ok(())
}

fn undo_wait(state: &mut SemState) {
    if let SemState::Semaphore { counter, .. } = state {
        *counter += 1;
    }
}

// Shared signal route. A semaphore releases at most one waiter; an event sets
// its flag and releases every waiter.
fn do_signal(inner_arc: &KSemInnerType) {
    let mut inner = inner_arc.lock();

    let wake = match &mut inner.state {
        SemState::Semaphore { max_count, counter } => {
            *counter = (*max_count).min(*counter + 1);
            if *counter <= 0 { Wake::One } else { Wake::None }
        },
        SemState::Event { signalled, .. } => {
            *signalled = true;
            Wake::All 
        }
    };

    match wake {
        Wake::None => {},
        Wake::One => {
            if inner.blocked_list.get_nodes() > 0 {
                wake_head(&mut inner, inner_arc);
            }
        },
        Wake::All => {
            while inner.blocked_list.get_nodes() > 0 {
                wake_head(&mut inner, inner_arc);
            }
        }
    }
}

fn wake_head(inner: &mut KSemInner, inner_arc: &KSemInnerType) {
    let wait_task_ptr = NonNull::from(inner.blocked_list.first().unwrap());
    let node = unsafe {
        inner.blocked_list.remove_node(wait_task_ptr)
    };

    let id = node.lock().get_id();
    sched::signal_waiting_task(id, Arc::clone(inner_arc));
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

    pub fn wait(&self) -> Result<(), KError> {
        do_wait(&self.inner)
    }

    pub fn wait_with_timer(&self, timer: KTimerInnerType) -> Result<(), KError> {
        let int_enabled = hal::are_interrupts_enabled();
        let preemption_enabled = is_preemption_enabled();
        let mut yield_flag = false;
        {
            let mut inner = self.inner.lock();

            let cur_task = sched::get_current_task()
            .expect("wait() called from idle task!!");

            let should_block = match &mut inner.state {
                SemState::Semaphore { counter, .. } => {
                    *counter -= 1;
                    *counter < 0
                },
                _ => {
                    panic!("Event type not allowed in timers!");
                }
            };

            if should_block {
                assert!(int_enabled, "wait_with_timer() would block with interrupts disabled — deadlock risk");
                assert!(preemption_enabled, "wait() would block with preemption disabled — deadlock risk");

                let inner_wrap = Arc::clone(&self.inner);

                if let Err(err) = inner.blocked_list.add_node(cur_task) {
                    undo_wait(&mut inner.state);
                    return Err(err);
                }

                if !sched::add_cur_task_to_wait_queue_with_timer(inner_wrap, timer) {
                    undo_wait(&mut inner.state);
                    inner.blocked_list.pop_node();
                    return Err(KError::WaitFailed);
                }

                yield_flag = true;
            }
        }

        if yield_flag {
            sched::yield_cpu();
        }

        Ok(())
    }

    pub fn signal(&self) {
        do_signal(&self.inner);
    }

    pub fn drop_task(inner_arc: KSemInnerType, task_id: usize) {
        let mut inner = inner_arc.lock();

        let mut blocked_task = None;
        for task in inner.blocked_list.iter() {
            if task.lock().get_id() == task_id {
                blocked_task = Some(NonNull::from(task));
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

    pub fn wait(&self) -> Result<(), KError> {
        do_wait(&self.inner)
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
    sem.wait().expect("config sem wait failed");
    ConfigGuard { sem }
}
