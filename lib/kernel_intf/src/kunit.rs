#![allow(static_mut_refs)]

#[repr(C)]
pub struct KUnitEntry {
    pub name: *const u8,
    pub name_len: usize,
    pub func: unsafe extern "C" fn(),
    pub enabled: bool
}

unsafe impl Sync for KUnitEntry {}

pub fn _run_range(start: *const KUnitEntry, end: *const KUnitEntry) {
    let count = unsafe { end.offset_from(start) } as usize;
    if count == 0 {
        crate::info!("No tests to run...");
        return;
    }
    crate::info!("=== Starting kunit tests ===");
    for i in 0..count {
        let entry = unsafe { &*start.add(i) };
        let name = unsafe {
            core::str::from_utf8_unchecked(
                core::slice::from_raw_parts(entry.name, entry.name_len)
            )
        };
        if !entry.enabled {
            crate::info!("kunit: skipping {}", name);
            continue;
        }
        crate::info!("kunit: running {}", name);
        unsafe { (entry.func)() };
    }
    crate::info!("=== Completed kunit tests ===");
}
