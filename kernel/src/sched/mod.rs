mod scheduler;
mod proc;
mod user;

pub use proc::*;
pub use scheduler::*;
pub use user::*;

pub type DispatchRoutine = extern "C" fn() -> !;

pub fn init() {
    proc::init();
    scheduler::init();
}