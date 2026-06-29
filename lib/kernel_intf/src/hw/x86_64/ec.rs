use super::{inb, outb};

pub fn ec_wait_ibf(cmd: u16) -> bool {
    for _ in 0..0x10000 {
        if unsafe { inb(cmd) } & 0x02 == 0 {
            return true;
        }
    }
    false
}

pub fn ec_wait_obf(cmd: u16) -> bool {
    for _ in 0..0x10000 {
        if unsafe { inb(cmd) } & 0x01 != 0 {
            return true;
        }
    }
    false
}

pub fn ec_read(data: u16, cmd: u16, offset: u8) -> u8 {
    if !ec_wait_ibf(cmd) { return 0; }
    unsafe { outb(cmd, 0x80); }
    if !ec_wait_ibf(cmd) { return 0; }
    unsafe { outb(data, offset); }
    if !ec_wait_obf(cmd) { return 0; }
    unsafe { inb(data) }
}

pub fn ec_write(data: u16, cmd: u16, offset: u8, value: u8) {
    if !ec_wait_ibf(cmd) { return; }
    unsafe { outb(cmd, 0x81); }
    if !ec_wait_ibf(cmd) { return; }
    unsafe { outb(data, offset); }
    if !ec_wait_ibf(cmd) { return; }
    unsafe { outb(data, value); }
}
