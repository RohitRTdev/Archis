use core::ptr::NonNull;
use alloc::sync::Arc;
use super::Spinlock;
use crate::{sched::{self, KTimerInnerType}};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::list::{List, DynList};
use kernel_intf::KError;
use crate::sched::KThread;

pub type KSemInnerType = Arc<Spinlock<KSemInner>, PoolAllocatorGlobal>;

pub struct KSemInner {
    max_count: isize,
    counter: isize,
    blocked_list: DynList<KThread>
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
                max_count,
                counter: init_count,
                blocked_list: List::new()
            }), PoolAllocatorGlobal) 
        }
    }

    pub fn wait(&self) -> Result<(), KError> {
        let mut yield_flag = false;
        {
            let mut inner = self.inner.lock();
            inner.counter -= 1;
            
            let cur_task = sched::get_current_task()
            .expect("wait() called from idle task!!");
            
            if inner.counter < 0 {
                let inner_wrap = Arc::clone(&self.inner);

                // Block task
                inner.blocked_list.add_node(cur_task).map_err(|err| {
                    inner.counter += 1;

                    err
                })?;

                if !sched::add_cur_task_to_wait_queue(inner_wrap) {
                    inner.counter += 1;
                    inner.blocked_list.pop_node();
                    
                    return Err(KError::WaitFailed);
                }
                
                yield_flag = true;
            }
        }

        // We call it here, in order to unlock the spinlock
        if yield_flag {
            sched::yield_cpu();
        }

        Ok(())
    }

    pub fn wait_with_timer(&self, timer: KTimerInnerType) -> Result<(), KError> {
        let mut yield_flag = false;
        {
            let mut inner = self.inner.lock();
            inner.counter -= 1;
            
            let cur_task = sched::get_current_task()
            .expect("wait() called from idle task!!");
            
            if inner.counter < 0 {
                let inner_wrap = Arc::clone(&self.inner);
                
                // Add kernel timer and add task to wait queue atomically
                inner.blocked_list.add_node(cur_task).map_err(|err| {
                    inner.counter += 1;

                    err
                })?;
                
                
                if !sched::add_cur_task_to_wait_queue_with_timer(inner_wrap, timer) {
                    inner.counter += 1;
                    inner.blocked_list.pop_node();

                    return Err(KError::WaitFailed);
                }
                yield_flag = true;
            }
        }

        // We call it here, in order to unlock the spinlock
        if yield_flag {
            sched::yield_cpu();
        }

        Ok(())
    }

    pub fn signal(&self) {
        {
            let mut inner = self.inner.lock();
            inner.counter = inner.max_count.min(inner.counter + 1);

            if inner.counter <= 0 {
                let wait_count = inner.blocked_list.get_nodes();
                
                // Remove head task from blocked list
                if wait_count > 0 {
                    let wait_task_ptr = NonNull::from(inner.blocked_list.first().unwrap());
                    let node = unsafe {
                        inner.blocked_list.remove_node(wait_task_ptr)
                    };

                    let inner_wrap = Arc::clone(&self.inner);
                    
                    let id = node.lock().get_id();
                    sched::signal_waiting_task(id, inner_wrap);
                } 
            }
        }
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