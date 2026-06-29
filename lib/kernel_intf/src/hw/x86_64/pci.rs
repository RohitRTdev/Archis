use super::{inl, outl};

const PCI_ADDR_PORT: u16 = 0xCF8;
const PCI_DATA_PORT: u16 = 0xCFC;

pub fn pci_cfg_read32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    let addr: u32 = 0x8000_0000
        | ((bus  as u32) << 16)
        | ((dev  as u32) << 11)
        | ((func as u32) << 8)
        | ((off  as u32) & 0xFC);
    unsafe {
        outl(PCI_ADDR_PORT, addr);
        inl(PCI_DATA_PORT)
    }
}

pub fn pci_cfg_read16(bus: u8, dev: u8, func: u8, off: u8) -> u16 {
    let dword = pci_cfg_read32(bus, dev, func, off & !3);
    (dword >> (((off & 2) as u32) * 8)) as u16
}

pub fn pci_cfg_read8(bus: u8, dev: u8, func: u8, off: u8) -> u8 {
    let dword = pci_cfg_read32(bus, dev, func, off & !3);
    (dword >> (((off & 3) as u32) * 8)) as u8
}

pub fn pci_cfg_write32(bus: u8, dev: u8, func: u8, off: u8, val: u32) {
    let addr: u32 = 0x8000_0000
        | ((bus  as u32) << 16)
        | ((dev  as u32) << 11)
        | ((func as u32) << 8)
        | ((off  as u32) & 0xFC);
    unsafe {
        outl(PCI_ADDR_PORT, addr);
        outl(PCI_DATA_PORT, val);
    }
}
