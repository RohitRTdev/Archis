use crate::sync::{KSem, Spinlock};
use kernel_intf::mem::PoolAllocatorGlobal;
use alloc::sync::Arc;
use kernel_intf::KError;

pub type KTimerInnerType = Arc<Spinlock<KTimerInner>, PoolAllocatorGlobal>;

// We'll introduce periodic timers later
pub struct KTimerInner {
    init_count: usize,
    wait_sem: KSem    
}

impl KTimerInner {
    fn new(init_count: usize) -> Self {
        Self {
            init_count,
            wait_sem: KSem::new(0, 1)
        }
    }

    pub fn update_timer_count(&mut self, count: usize) -> bool {
        self.init_count = self.init_count.saturating_sub(count);
        
        self.init_count == 0
    }

    pub fn get_semaphore(&self) -> KSem {
        self.wait_sem.clone()
    }
}

pub struct KTimer {
    inner: KTimerInnerType   
}

impl KTimer {
    pub fn new(init_count: usize) -> Self {
        Self {
            inner: Arc::new_in(Spinlock::new(
                KTimerInner::new(init_count)
            ), PoolAllocatorGlobal)
        }
    }

    pub fn wait(&self) -> Result<(), KError> {
        let inner_clones  = {
            let inner = self.inner.lock();

            if inner.init_count > 0 {
                let timer_clone = Arc::clone(&self.inner);

                Some((inner.wait_sem.clone(), timer_clone))
            }
            else {
                None
            }
        };

        if let Some(clones) = inner_clones {
            return clones.0.wait_with_timer(clones.1);
        }

        Ok(())
    }
}

// Do not call this function from interrupt context
pub fn delay_ms(value: usize) {
    let timer = KTimer::new(value);

    // Let's not panic if wait fails (Since this could happen if process/thread is getting killed)
    let _ = timer.wait();
}

#[unsafe(no_mangle)]
pub extern "C" fn delay_ms_ffi(value: usize) {
    delay_ms(value);
}