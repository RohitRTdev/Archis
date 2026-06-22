use core::ptr::NonNull;

use alloc::string::String;

use kernel_intf::list::{DynList, List, ListNodeGuard};
use kernel_intf::mem::PoolAllocator;
use kernel_intf::info;

use crate::io::unload_driver;
use crate::sync::{KEvent, Once, Spinlock};
use crate::sched;

use super::driver::get_device;
use super::stack;

pub enum PnpRequest {
    InvalidateDevice { device_id: usize },
    StartDevice      { device_id: usize },
    StopDevice       { device_id: usize },
    RemoveDevice     { device_id: usize },
    RemoveDriver     { name: String },
    AddConfig        { name: String },
    Fence            { event: KEvent }
}

impl core::fmt::Debug for PnpRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PnpRequest::InvalidateDevice { device_id } =>
                write!(f, "InvalidateDevice {{ device_id: {} }}", device_id),
            PnpRequest::StartDevice { device_id } =>
                write!(f, "StartDevice {{ device_id: {} }}", device_id),
            PnpRequest::StopDevice { device_id } =>
                write!(f, "StopDevice {{ device_id: {} }}", device_id),
            PnpRequest::RemoveDevice { device_id } =>
                write!(f, "RemoveDevice {{ device_id: {} }}", device_id),
            PnpRequest::RemoveDriver { name } =>
                write!(f, "RemoveDriver {{ name: {:?} }}", name),
            PnpRequest::AddConfig { name } =>
                write!(f, "AddConfig {{ name: {:?} }}", name),
            PnpRequest::Fence { .. } => write!(f, "Fence"),
        }
    }
}

static PNP_QUEUE: Spinlock<DynList<PnpRequest>> = Spinlock::new(List::new());
static PNP_SIGNAL: Once<KEvent> = Once::new();

pub fn pnp_post(req: PnpRequest) {
    PNP_QUEUE.lock().add_node(req).expect("PnP enqueue failed");
    PNP_SIGNAL.get().expect("io::init not called before pnp_post").signal();
}

pub fn start_device(device_id: usize) {
    pnp_post(PnpRequest::StartDevice { device_id });
}

pub fn stop_device(device_id: usize) {
    pnp_post(PnpRequest::StopDevice { device_id });
}

pub fn remove_device_async(device_id: usize) {
    pnp_post(PnpRequest::RemoveDevice { device_id });
}

pub fn add_config(name: String) {
    pnp_post(PnpRequest::AddConfig { name });
}

pub fn pnp_fence() {
    let event = KEvent::new(false);
    pnp_post(PnpRequest::Fence { event: event.clone() });
    event.wait(false);
}

fn pop_one() -> Option<ListNodeGuard<PnpRequest, PoolAllocator>> {
    let mut q = PNP_QUEUE.lock();
    if q.get_nodes() == 0 {
        return None;
    }
    let head = NonNull::from(q.first().unwrap());
    Some(unsafe { q.remove_node(head) })
}

fn handle(req: &PnpRequest) {
    crate::io_log!("Handling request: {:?}", req);
    match req {
        PnpRequest::InvalidateDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => stack::enumerate_and_detect(dev),
                None => info!("pnp: invalidate target device {} no longer exists", device_id)
            }
        },
        PnpRequest::StartDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => { let _ = dev.start(); }
                None => info!("pnp: start target device {} no longer exists", device_id)
            }
        },
        PnpRequest::StopDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => { let _ = dev.stop(); }
                None => info!("pnp: stop target device {} no longer exists", device_id)
            }
        },
        PnpRequest::RemoveDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => super::driver::remove_device(&dev),
                None => info!("pnp: remove target device {} no longer exists", device_id)
            }
        },
        PnpRequest::RemoveDriver { name } => {
            unload_driver(name);
        },
        PnpRequest::AddConfig { name } => {
            // This will get the new configuration data and scan uninitialized pdo to check
            // if the new device stacks can be attached to it
            //todo!();
        },
        PnpRequest::Fence { event } => {
            event.signal();
        }
    }
}

extern "C" fn pnp_worker() -> ! {
    info!("Started pnp worker thread");
    loop {
        crate::io_log!("pnp_worker: Waiting for requests");
        PNP_SIGNAL.get().unwrap().wait(false);
        // Drain until empty, then go back to waiting.
        loop {
            let guard = match pop_one() {
                Some(g) => g,
                None => break
            };
            handle(&*guard);
        }
        crate::io_log!("pnp_worker: Going back to sleep");
    }
}

pub fn start_worker() {
    PNP_SIGNAL.call_once(|| KEvent::new(true));
    sched::create_system_thread(
        pnp_worker, 
        core::ptr::null_mut()
    ).expect("Failed to spawn PnP worker thread");
}
