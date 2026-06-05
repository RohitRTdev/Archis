use core::ptr::NonNull;

use alloc::string::String;

use kernel_intf::list::{DynList, List, ListNodeGuard};
use kernel_intf::mem::PoolAllocator;
use kernel_intf::info;

use crate::sync::{KEvent, Once, Spinlock};
use crate::sched;

use super::driver::{get_device, load_driver_by_name};
use super::stack;

#[derive(Debug)]
pub enum PnpRequest {
    InvalidateDevice { device_id: usize },
    RefreshDeviceTree,
    StopDevice       { device_id: usize },
    RemoveDevice     { device_id: usize },
    RegisterDriver   { name: String }
}

static PNP_QUEUE: Spinlock<DynList<PnpRequest>> = Spinlock::new(List::new());
static PNP_SIGNAL: Once<KEvent> = Once::new();

pub fn pnp_post(req: PnpRequest) {
    PNP_QUEUE.lock().add_node(req).expect("PnP enqueue failed");
    PNP_SIGNAL.get().expect("io::init not called before pnp_post").signal();
}

pub fn refresh_device_tree() {
    pnp_post(PnpRequest::RefreshDeviceTree);
}

pub fn stop_device(device_id: usize) {
    pnp_post(PnpRequest::StopDevice { device_id });
}

pub fn remove_device_async(device_id: usize) {
    pnp_post(PnpRequest::RemoveDevice { device_id });
}

pub fn register_driver(name: String) {
    pnp_post(PnpRequest::RegisterDriver { name });
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
    info!("Handling request: {:?}", req);
    match req {
        PnpRequest::InvalidateDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => stack::enumerate_and_detect(dev),
                None => info!("pnp: invalidate target device {} no longer exists", device_id)
            }
        }
        PnpRequest::RefreshDeviceTree => {
            stack::do_refresh_device_tree();
        }
        PnpRequest::StopDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => { let _ = dev.stop(); }
                None => info!("pnp: stop target device {} no longer exists", device_id)
            }
        }
        PnpRequest::RemoveDevice { device_id } => {
            match get_device(*device_id) {
                Some(dev) => super::driver::remove_device(&dev),
                None => info!("pnp: remove target device {} no longer exists", device_id)
            }
        }
        PnpRequest::RegisterDriver { name } => {
            // This will get the new configuration data and scan uninitialized pdo to check
            // if the new device stacks can be attached to it 
            todo!();
        }
    }
}

fn pnp_worker() -> ! {
    info!("pnp_worker: started");
    loop {
        info!("pnp_worker: Waiting for requests");
        PNP_SIGNAL.get().unwrap().wait().expect("PnP signal wait failed");
        // Drain until empty, then go back to waiting.
        loop {
            let guard = match pop_one() {
                Some(g) => g,
                None => break
            };
            handle(&*guard);
        }
        info!("pnp_worker: Going back to sleep");
    }
}

pub fn start_worker() {
    PNP_SIGNAL.call_once(|| KEvent::new(true));
    sched::create_thread(pnp_worker).expect("Failed to spawn PnP worker thread");
}
