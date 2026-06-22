use crate::hal::{write_port_u8, read_port_u8};
use crate::sync::Spinlock;
use kernel_intf::RtcTime;

const CMOS_ADDRESS: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

// CMOS uses an index/data port pair (0x70/0x71). All accesses must be
// serialised so that a register-select on one core can't interleave with
// a data read from another core.
static RTC_LOCK: Spinlock<()> = Spinlock::new(());

const RTC_SECONDS: u8 = 0x00;
const RTC_MINUTES: u8 = 0x02;
const RTC_HOURS: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_STATUS_B: u8 = 0x0B;

fn read_cmos(reg: u8) -> u8 {
    unsafe {
        write_port_u8(CMOS_ADDRESS, reg | 0x80); 
        read_port_u8(CMOS_DATA)
    }
}

fn is_updating() -> bool {
    unsafe {
        write_port_u8(CMOS_ADDRESS, 0x0A | 0x80);
        read_port_u8(CMOS_DATA) & 0x80 != 0
    }
}

fn bcd_to_bin(val: u8) -> u8 {
    ((val & 0xF0) >> 4) * 10 + (val & 0x0F)
}

// We don't want compiler to think that this is a pure function
#[inline(never)]
pub fn read_realtime() -> RtcTime {
    // Serialise CMOS port access across cores. The guard also disables
    // interrupts on the current core for the duration of the read.
    let _guard = RTC_LOCK.lock();

    // Wait until not updating
    while is_updating() {
        core::hint::spin_loop();
    }

    let mut second = read_cmos(RTC_SECONDS);
    let mut minute = read_cmos(RTC_MINUTES);
    let mut hour = read_cmos(RTC_HOURS);
    let mut day = read_cmos(RTC_DAY);
    let mut month = read_cmos(RTC_MONTH);
    let mut year = read_cmos(RTC_YEAR);

    let status_b = read_cmos(RTC_STATUS_B);
    let bcd = (status_b & 0x04) == 0;

    if bcd {
        second = bcd_to_bin(second);
        minute = bcd_to_bin(minute);
        hour = bcd_to_bin(hour & 0x7F) | (hour & 0x80);
        day = bcd_to_bin(day);
        month = bcd_to_bin(month);
        year = bcd_to_bin(year);
    }

    // If 12-hour mode, convert to 24-hour
    if (status_b & 0x02) == 0 && (hour & 0x80) != 0 {
        hour = ((hour & 0x7F) + 12) % 24;
    } else {
        hour &= 0x7F;
    }

    RtcTime {
        second,
        minute,
        hour,
        day,
        month,
        year
    }
}

#[unsafe(no_mangle)]
extern "C" fn read_rtc() -> RtcTime {
    read_realtime()
}