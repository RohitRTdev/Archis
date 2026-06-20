use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

use kernel_intf::Lock;
use crate::hal;
use common::ptr_to_ref_mut;

// This assumption is used by Lock variable in kernel_intf
const _: () = {
    assert!(core::mem::size_of::<hal::Spinlock>() == 8);
};

pub struct SpinlockGuard<'a, T> {
#[cfg(not(test))]
    lock: &'a hal::Spinlock,
#[cfg(test)]
    _lock: MutexGuard<'a, u32>,
#[cfg(not(test))]
    int_status: bool,
    data: *mut T
}

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe {&*self.data}
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {&mut *self.data}
    }
}

#[cfg(not(test))]
impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
        hal::enable_interrupts(self.int_status);
    }
}

pub struct Spinlock<T> {
#[cfg(not(test))] 
    pub lock: hal::Spinlock,
#[cfg(test)]
    pub lock: Mutex<u32>,
    data: UnsafeCell<T>
}

unsafe impl<T: Send> Sync for Spinlock<T>{}

impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Spinlock {
#[cfg(not(test))]
            lock: hal::Spinlock::new(),
#[cfg(test)]
            lock: Mutex::new(0),
            data: UnsafeCell::new(data)
        }
    }
    
#[cfg(not(test))]
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        let int_status = hal::disable_interrupts();

        #[cfg(all(debug_assertions, feature = "deadlock_detection"))]
        {
            let mut count = 0;        
            while !self.lock.try_lock() {
                count += 1;

                if count > 10000000 {
                    break;
                }
            }
            
            if count > 10000000 {
                panic!("Lock acquisition expired!!");
            }
        }

        #[cfg(not(all(debug_assertions, feature = "deadlock_detection")))]
        self.lock.lock();

        SpinlockGuard { lock: &self.lock, int_status, data: self.data.get()}
    }

#[cfg(test)]
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        let guard = self.lock.lock().unwrap();
        SpinlockGuard { _lock: guard, data: self.data.get()}
    }

    // This gives access to underlying data without locking
    // Only used in infra code during exception handling
    pub unsafe fn as_ref(&self) -> &T {
        unsafe { &*self.data.get() }
    }
}

#[unsafe(no_mangle)]
extern "C" fn create_spinlock_ffi(lock: &mut Lock) {
    let val = hal::Spinlock::new();

    unsafe {
        ptr_to_ref_mut::<_, hal::Spinlock>(&lock.lock).write(val);
    }
}

#[unsafe(no_mangle)]
extern "C" fn acquire_spinlock_ffi(lock: &mut Lock) {
    unsafe {
        #[cfg(not(test))]
        let stat = hal::disable_interrupts();
        
        #[cfg(not(all(debug_assertions, feature = "deadlock_detection")))]
        (*ptr_to_ref_mut::<_, hal::Spinlock>(&lock.lock)).lock();
        
        #[cfg(all(debug_assertions, feature = "deadlock_detection"))]
        {
            let mut count = 0;
            while !(*ptr_to_ref_mut::<_, hal::Spinlock>(&lock.lock)).try_lock() {
                count += 1;
                if count > 10000000 {
                    break;
                }
            } 

            if count > 10000000 {
                panic!("Lock acquisition failed on logger lock..."); 
            }

        }
        
        #[cfg(not(test))] 
        {
            lock.int_status = stat;
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn release_spinlock_ffi(lock: &mut Lock) {
    unsafe {
        // Snapshot int_status BEFORE releasing the lock. Once unlock() runs,
        // another core can acquire the lock and overwrite lock.int_status with
        // its own pre-acquire flag, which would cause us to restore the wrong
        // interrupt state on this core (potentially leaving interrupts
        // permanently disabled).
        #[cfg(not(test))]
        let int_status = lock.int_status;

        (*ptr_to_ref_mut::<_, hal::Spinlock>(&lock.lock)).unlock();

        #[cfg(not(test))]
        hal::enable_interrupts(int_status);
    }
}