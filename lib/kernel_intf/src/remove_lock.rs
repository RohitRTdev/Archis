use core::sync::atomic::{AtomicUsize, Ordering};

pub struct RemoveLock {
    state: AtomicUsize
}

const REMOVING: usize = 1 << (usize::BITS - 1);

impl RemoveLock {
    pub const fn new() -> Self {
        Self { state: AtomicUsize::new(0) }
    }

    // Take a reference. false once removal has begun -- caller must not
    // touch the protected data and must not call release().
    pub fn acquire(&self) -> bool {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            if cur & REMOVING != 0 {
                return false;
            }
            if self.state.compare_exchange_weak(
                cur, cur + 1, Ordering::AcqRel, Ordering::Acquire
            ).is_ok() {
                return true;
            }
        }
    }

    // Give back a reference taken via acquire(). true = this was the last
    // outstanding reference and removal had begun -- caller is now solely
    // responsible for freeing the protected data, exactly once.
    pub fn release(&self) -> bool {
        let prev = self.state.fetch_sub(1, Ordering::AcqRel);
        assert!(prev & !REMOVING != 0);
        (prev & REMOVING != 0) && (prev & !REMOVING) == 1
    }

    // Call exactly once. true = caller must free now (no
    // outstanding references) -- otherwise the eventual release() that
    // drains the last one will do it instead.
    pub fn begin_remove(&self) -> bool {
        let prev = self.state.fetch_or(REMOVING, Ordering::AcqRel);
        (prev & !REMOVING) == 0
    }
}

impl Default for RemoveLock {
    fn default() -> Self {
        Self::new()
    }
}
