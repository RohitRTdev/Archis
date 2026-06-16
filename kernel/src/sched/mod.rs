mod scheduler;
mod proc;
mod user;
mod cleanup;

pub use proc::*;
pub use scheduler::*;
pub use user::*;
pub use cleanup::*;

pub type DispatchRoutine = extern "C" fn() -> !;

pub fn init() {
    proc::init();
    scheduler::init();
    cleanup::init();
}