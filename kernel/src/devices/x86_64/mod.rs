mod rtc;
pub use rtc::read_realtime;

pub mod uart;

mod hpet;
pub use hpet::*;

pub mod ioapic;

pub fn init() {
#[cfg(feature = "acpi")]
    {
        hpet::init();
        ioapic::early_init();
    }
}