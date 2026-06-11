use core::panic::PanicInfo;
use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::hint::unlikely;
use core::ffi::c_char;
use common::StrRef;
use common::elf::*;
use rustc_demangle::demangle;
use crate::{cpu, logger};
use kernel_intf::println;
use crate::sync::Spinlock;
use crate::hal::{self, IPIRequestType, notify_core};
use crate::loader::{KERNEL_MODULES, module::*};

static DISABLE_CALLSTACK: AtomicBool = AtomicBool::new(false);
static EARLY_PANIC_PHASE: AtomicBool = AtomicBool::new(true);
static PRE_LOADER_PHASE: AtomicBool = AtomicBool::new(true);
static MP_INIT: AtomicBool = AtomicBool::new(false);
static GLOBAL_PANIC_LOCK: hal::Spinlock = hal::Spinlock::new();
static IS_NESTED: AtomicBool = AtomicBool::new(false);
static PANIC_CORE: Spinlock<Option<usize>> = Spinlock::new(None); 
const STACK_UNWIND_DEPTH: usize = 16;
const DEFAULT_PANIC_STRING: &'static str = "[Unknown]";

struct PanicCommon<'a> {
    msg: &'static str,
    info: Option<&'a PanicInfo<'a>>
}

impl<'a> PanicCommon<'a> {
    fn print(&self) {
        if self.info.is_some() {
            println!("Message: {}", self.info.unwrap().message());
        }
        else {
            println!("Message: {}", self.msg);
        }
    }
}


#[allow(dead_code)]
fn common_panic_handler(mod_name: &str, info: PanicCommon) -> ! {
    hal::disable_interrupts();
    let core = hal::get_core();

    // In case of nested panic, only allow the initially panicked core 
    if !IS_NESTED.load(Ordering::Acquire) || 
    PANIC_CORE.lock().is_none() ||
    *PANIC_CORE.lock().as_ref().unwrap() != core {
        GLOBAL_PANIC_LOCK.lock();
        *PANIC_CORE.lock() = Some(core);
    }

    let early_panic_phase = EARLY_PANIC_PHASE.load(Ordering::Acquire);
    let mp_init = MP_INIT.load(Ordering::Acquire);
    if !IS_NESTED.load(Ordering::Acquire) {
        if !early_panic_phase && mp_init {
            // Shutdown all the other cores
            for cpu in 0..cpu::get_total_cores() {
                if cpu != core {
                    notify_core(IPIRequestType::Shutdown, cpu);
                }
            }
        }

        kernel_intf::disable_logger();
        logger::set_panic_mode(core as u8);
        kernel_intf::enable_logger();
    }

    if IS_NESTED.load(Ordering::Acquire) {
        println!("====Nested panic!!====");
    }

    // Try and recover any more panics that might occur beyond this point
    IS_NESTED.store(true, Ordering::Release);

    if early_panic_phase || DISABLE_CALLSTACK.load(Ordering::Acquire) {
        println!("Kernel panic on core {}!!", core);
        info.print();
        println!("Module: {}", mod_name);
        
        hal::halt();
    }
    
    println!("Kernel panic on core {}!!", core);
    info.print();
    println!("Module: {}", mod_name);

    let stack_base = cpu::get_panic_base(); 
    start_unwind(mod_name, stack_base);

    hal::halt();
}

pub fn start_unwind(mod_name: &str, stack_base: usize) {
    let mut unwind_list: [usize; STACK_UNWIND_DEPTH] = [0; STACK_UNWIND_DEPTH];

    #[cfg(debug_assertions)]
    {
        println!("Callstack:");

        let (actual_depth, cur_base) = hal::unwind_stack(STACK_UNWIND_DEPTH, stack_base, unwind_list.as_mut_slice());
        let start_depth = if mod_name == env!("CARGO_PKG_NAME") { 3 } else { 4 };

        if actual_depth <= start_depth + 1 {
            println!("(Empty) => Current stack base: {:#X}, Current stack top: {:#X}", cur_base, stack_base);
        }

        for addr in start_depth..actual_depth {
            if unwind_list[addr] != 0 {
                let sym_info = symbol_trace(unwind_list[addr]);
                if let Some(sym) = sym_info {
                    println!("{:#X}({}!{}+{:#X})", unwind_list[addr], sym.0, demangle(sym.1), sym.2);
                }
                else {
                    println!("{:#X}(??)", unwind_list[addr]);
                }
            }
        }
    }

}

fn symbol_trace_do_work(addr: usize, module: &KernelModule) -> Option<(&'static str, &'static str, usize)> {
    // Check if this symbol is part of this module
    if (addr < module.info.base) || (addr >= module.info.base + module.info.size) {
        return None;
    }

    // Now iterate through symbols to find the correct one
    if let Some(sym) = &module.info.sym_tab {
        let strtab = module.info.sym_str.as_ref().unwrap();

        let entries = unsafe {
            core::slice::from_raw_parts(sym.start as *const Elf64Sym, sym.size / sym.entry_size)
        };

        let stringizer = |str_idx: usize| {
            let str_base = unsafe {
                (strtab.base_address as *const u8).add(str_idx)
            };

            unsafe {
                CStr::from_ptr(str_base as *const c_char).to_str().unwrap()
            }
        };
        
        let shift = addr - module.info.base;
        for entry in entries {
            let e_type = entry.st_info & 0x0f;
            if e_type != STT_OBJECT && e_type != STT_FUNC {
                continue;
            }
            
            let lower_bound = entry.st_value as usize;
            let upper_bound = lower_bound + entry.st_size as usize;
            
            // We found the data object or function this symbol belong to
            if shift >= lower_bound && shift < upper_bound {
                let offset = shift - lower_bound;
                return Some((module.name, stringizer(entry.st_name as usize), offset))
            }
        }
    }

    None
}   

fn symbol_trace(addr: usize) -> Option<(&'static str, &'static str, usize)> {
    // Avoid locking — this runs from the panic handler and the locks we'd
    // otherwise take may already be held by the panicking core. See
    // Spinlock::as_ref safety doc.
    if unlikely(PRE_LOADER_PHASE.load(Ordering::Acquire)) {
        let aris = unsafe { ARIS.get().unwrap().as_ref() };
        symbol_trace_do_work(addr, aris.kernel())
    }
    else {
        let loaded_images = unsafe { KERNEL_MODULES.as_ref() };
        for image in loaded_images.iter() {
            let entry = image.upgrade();
            if entry.is_none() {
                continue;
            }

            let arc = entry.as_ref().unwrap();
            let module = unsafe { Spinlock::as_ref(&**arc) };

            // Defensive: only kernel modules carry kernel-space symbol info
            let kmod = match module.kernel_opt() {
                Some(k) => k,
                None => continue
            };

            let res = symbol_trace_do_work(addr, kmod);

            if res.is_some() {
                return res;
            }
        }

        None
    }
}

pub fn disable_early_panic_phase() {
    EARLY_PANIC_PHASE.store(false, Ordering::Release);
}

pub fn disable_callstack() {
    DISABLE_CALLSTACK.store(true, Ordering::Release);
}

pub fn disable_preloader_phase() {
    PRE_LOADER_PHASE.store(false, Ordering::Release);
}

#[allow(dead_code)]
pub fn enable_mp_init() {
    MP_INIT.store(true, Ordering::Release);
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let panic_info = PanicCommon {
        msg: DEFAULT_PANIC_STRING,
        info: Some(info)
    };
    common_panic_handler(env!("CARGO_PKG_NAME"), panic_info);
}


#[cfg(not(test))]
#[unsafe(no_mangle)]
extern "C" fn panic_router(mod_name: StrRef, info: StrRef) -> ! {
    common_panic_handler(unsafe {mod_name.as_str()}, unsafe { PanicCommon {msg: info.as_str(), info: None}})
}