mod scheduler;
mod proc;
mod user;
mod timer;

pub use proc::*;
pub use scheduler::*;
pub use user::*;
pub use timer::*;

pub type DispatchRoutine = extern "C" fn() -> !;

pub fn init() {
    proc::init();
    scheduler::init();
}