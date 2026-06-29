#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::mem::size_of;
use alloc::vec::Vec;

use kernel_intf::{
    driver::{
        DeviceObject, DriverObject, Irp, IrpMinor, Status,
        create_device, create_device_by_id,
        ResEntry, ResType, ResTypeDesc, IntDesc, PortDesc,
        IrqInfo, MAX_RESOURCE_ENTRIES
    },
    info
};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::hw::{pci_cfg_read32, pci_cfg_read8, pci_cfg_read16, pci_cfg_write32};
use common::MemoryRegion;
use kmod::dispatch_init;

struct PciChildEntry {
    bus: u8,
    device: u8,
    function: u8,
    pdo: *const DeviceObject
}

struct PciCtx {
    is_fdo: bool,
    bus: u8,
    device: u8,
    function: u8,
    root_bridge_pdo: *const DeviceObject,
    children: Vec<PciChildEntry, PoolAllocatorGlobal>,
    hw_id: [u8; 20],
    hw_id_len: usize
}

unsafe impl Send for PciCtx {}
unsafe impl Sync for PciCtx {}


fn hex_nibble(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + n - 10 }
}

fn format_hw_id(vendor: u16, device: u16, class: u8) -> ([u8; 20], usize) {
    let mut buf = [0u8; 20];
    buf[0] = b'P'; buf[1] = b'C'; buf[2] = b'I'; buf[3] = b'_';
    buf[4]  = hex_nibble((vendor >> 12) as u8 & 0xF);
    buf[5]  = hex_nibble((vendor >> 8)  as u8 & 0xF);
    buf[6]  = hex_nibble((vendor >> 4)  as u8 & 0xF);
    buf[7]  = hex_nibble(vendor          as u8 & 0xF);
    buf[8]  = b'_';
    buf[9]  = hex_nibble((device >> 12) as u8 & 0xF);
    buf[10] = hex_nibble((device >> 8)  as u8 & 0xF);
    buf[11] = hex_nibble((device >> 4)  as u8 & 0xF);
    buf[12] = hex_nibble(device          as u8 & 0xF);
    buf[13] = b'_';
    buf[14] = hex_nibble((class >> 4) & 0xF);
    buf[15] = hex_nibble(class & 0xF);
    buf[16] = 0;
    (buf, 16)
}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("Starting PCI driver init for {}", driver.get_name());
    dispatch_init!(driver, dispatch_add, dispatch_pnp);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    info!("PCI driver: adding device on top of root bridge PDO id={}", pdo.id);

    let ctx = alloc::boxed::Box::new_in(
        PciCtx {
            is_fdo: true,
            root_bridge_pdo: pdo as *const DeviceObject,
            children: alloc::vec::Vec::new_in(PoolAllocatorGlobal),
            bus: 0,
            device: 0,
            function: 0,
            hw_id: [0; 20],
            hw_id_len: 0
        },
        PoolAllocatorGlobal
    );
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;
    create_device(driver, None, ctx_ptr, Some(pdo), false);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, req: &mut Irp) -> Status {
    match req.minor_code {
        IrpMinor::Start => {
            req.complete_irp(Status::Success);
            Status::Success
        }
        IrpMinor::Enumerate => do_enumerate(device, req),
        IrpMinor::Query => do_query(device, req),
        IrpMinor::Resources => do_resources(device, req),
        IrpMinor::Stop => {
            req.complete_irp(Status::Success);
            Status::Success
        }
        IrpMinor::Remove => {
            unsafe { drop(alloc::boxed::Box::from_raw_in(device.ctx as *mut PciCtx, PoolAllocatorGlobal)); }
            req.complete_irp(Status::Success);
            Status::Success
        }
        _ => Status::Unsupported
    }
}

fn do_enumerate(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &mut *(device.ctx as *mut PciCtx) };
    assert!(ctx.is_fdo);

    let driver_id = device.get_driver_id();
    let entry_size = size_of::<*const DeviceObject>();
    let max_count = req.buffer.size / entry_size;
    let out_buf = req.buffer.base_address as *mut *const DeviceObject;
    let mut out_count = 0usize;

    let mut base_bus: u8 = 0;
    let mut segment: u16 = 0;
    unsafe { acpi::acpi_get_rb_info(ctx.root_bridge_pdo as *const u8, &mut base_bus, &mut segment); }

    if segment != 0 {
        info!("pci driver doesn't support scanning non 0 segment devices!");
        req.complete_irp(Status::Failed);
        return Status::Failed;
    }

    scan_bus(base_bus, ctx, driver_id, device, out_buf, &mut out_count, max_count);

    req.bytes_completed = out_count * entry_size;
    req.complete_irp(Status::Success);
    Status::Success
}

fn scan_bus(
    bus: u8,
    ctx: &mut PciCtx,
    driver_id: usize,
    fdo: &DeviceObject,
    out_buf: *mut *const DeviceObject,
    out_count: &mut usize,
    max_count: usize,
) {
    // DFS through the hierachy
    for dev in 0u8..32 {
        let id0 = pci_cfg_read32(bus, dev, 0, 0x00);
        if (id0 & 0xFFFF) == 0xFFFF { continue; }

        let header_type = pci_cfg_read8(bus, dev, 0, 0x0E);
        let max_func: u8 = if header_type & 0x80 != 0 { 8 } else { 1 };

        for func in 0..max_func {
            if func > 0 && pci_cfg_read16(bus, dev, func, 0x00) == 0xFFFF { continue; }

            let existing = ctx.children.iter()
                .find(|e| e.bus == bus && e.device == dev && e.function == func)
                .map(|e| e.pdo);

            let pdo = if let Some(p) = existing {
                p
            } else {
                let id_reg    = pci_cfg_read32(bus, dev, func, 0x00);
                let class_reg = pci_cfg_read32(bus, dev, func, 0x08);
                let vendor_id = (id_reg & 0xFFFF) as u16;
                let device_id = ((id_reg >> 16) & 0xFFFF) as u16;
                let class_code = (class_reg >> 24) as u8;
                let (hw_id, hw_id_len) = format_hw_id(vendor_id, device_id, class_code);
                info!("PCI: {:02X}:{:02X}.{} vendor={:04X} device={:04X} class={:02X}",
                    bus, dev, func, vendor_id, device_id, class_code);

                let pdo_ctx = alloc::boxed::Box::new_in(
                    PciCtx {
                        is_fdo: false,
                        bus,
                        device: dev,
                        function: func,
                        root_bridge_pdo: ctx.root_bridge_pdo,
                        hw_id,
                        hw_id_len,
                        children: alloc::vec::Vec::new_in(PoolAllocatorGlobal)
                    },
                    PoolAllocatorGlobal
                );
                let pdo_ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(pdo_ctx).0 as *mut c_void;
                let new_pdo = create_device_by_id(driver_id, None, pdo_ctx_ptr, Some(fdo), false);
                if new_pdo.is_null() {
                    unsafe { drop(alloc::boxed::Box::from_raw_in(pdo_ctx_ptr as *mut PciCtx, PoolAllocatorGlobal)); }
                    continue;
                }
                ctx.children.push(PciChildEntry { bus, device: dev, function: func, pdo: new_pdo });
                new_pdo
            };

            if *out_count < max_count {
                unsafe { *out_buf.add(*out_count) = pdo; }
                *out_count += 1;
            }

            // Recurse into secondary bus if this is a PCI-PCI bridge (header type 1)
            if pci_cfg_read8(bus, dev, func, 0x0E) & 0x7F == 1 {
                let secondary_bus = pci_cfg_read8(bus, dev, func, 0x19);
                if secondary_bus != 0 {
                    scan_bus(secondary_bus, ctx, driver_id, fdo, out_buf, out_count, max_count);
                }
            }
        }
    }
}

fn do_query(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const PciCtx) };
    req.buffer.base_address = ctx.hw_id.as_ptr() as usize;
    req.buffer.size = ctx.hw_id_len;
    req.complete_irp(Status::Success);
    Status::Success
}

fn do_resources(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const PciCtx) };
    let res_list = unsafe { req.req_info.res_list };

    let entry_limit = res_list.count.min(MAX_RESOURCE_ENTRIES);
    let mut entries = [ResEntry::default(); MAX_RESOURCE_ENTRIES];
    let mut count = 0usize;

    let header_type = pci_cfg_read8(ctx.bus, ctx.device, ctx.function, 0x0E) & 0x7F;
    let num_bars: u8 = if header_type == 0 { 6 } else { 2 };

    let mut bar_idx = 0u8;
    while bar_idx < num_bars && count < entry_limit {
        let off = 0x10u8 + bar_idx * 4;
        let bar = pci_cfg_read32(ctx.bus, ctx.device, ctx.function, off);

        if bar == 0 || bar == 0xFFFF_FFFF {
            bar_idx += 1;
            continue;
        }

        if bar & 1 == 0 {
            // Memory BAR
            let is64 = (bar >> 1) & 3 == 2;
            let base_lo = (bar & !0xFu32) as u64;
            let (base, size) = if is64 && bar_idx + 1 < num_bars {
                let hi_off = 0x10u8 + (bar_idx + 1) * 4;
                let orig_hi = pci_cfg_read32(ctx.bus, ctx.device, ctx.function, hi_off);

                // Probe size
                pci_cfg_write32(ctx.bus, ctx.device, ctx.function, off, 0xFFFF_FFFF);
                let lo_mask = pci_cfg_read32(ctx.bus, ctx.device, ctx.function, off);
                pci_cfg_write32(ctx.bus, ctx.device, ctx.function, off, bar);

                pci_cfg_write32(ctx.bus, ctx.device, ctx.function, hi_off, 0xFFFF_FFFF);
                let hi_mask = pci_cfg_read32(ctx.bus, ctx.device, ctx.function, hi_off) as u64;
                pci_cfg_write32(ctx.bus, ctx.device, ctx.function, hi_off, orig_hi);

                let base = base_lo | ((orig_hi as u64) << 32);
                let combined = (hi_mask << 32) | ((lo_mask & !0xFu32) as u64);
                let size = (!combined).wrapping_add(1) as usize;
                bar_idx += 1;
                (base, size)
            } else {
                pci_cfg_write32(ctx.bus, ctx.device, ctx.function, off, 0xFFFF_FFFF);
                let lo_mask = pci_cfg_read32(ctx.bus, ctx.device, ctx.function, off);
                pci_cfg_write32(ctx.bus, ctx.device, ctx.function, off, bar);
                let size = (!(lo_mask & !0xFu32) as u64 & 0xFFFF_FFFF).wrapping_add(1) as usize;
                (base_lo, size)
            };

            if base != 0 {
                entries[count] = ResEntry {
                    res_type: ResType::Memory,
                    desc: ResTypeDesc { mem: MemoryRegion { base_address: base as usize, size } }
                };
                count += 1;
            }
        } else {
            // I/O port BAR
            let base = (bar & !0x3u32) as usize;
            pci_cfg_write32(ctx.bus, ctx.device, ctx.function, off, 0xFFFF_FFFF);
            let size_mask = pci_cfg_read32(ctx.bus, ctx.device, ctx.function, off);
            pci_cfg_write32(ctx.bus, ctx.device, ctx.function, off, bar);
            let size = (!(size_mask & !0x3u32) & 0xFFFF).wrapping_add(1) as usize;

            if base != 0 {
                entries[count] = ResEntry {
                    res_type: ResType::Port,
                    desc: ResTypeDesc { port: PortDesc { base, range: size } }
                };
                count += 1;
            }
        }

        bar_idx += 1;
    }

    // IRQ from ACPI _PRT
    if count < entry_limit {
        let int_pin = pci_cfg_read8(ctx.bus, ctx.device, ctx.function, 0x3D);
        if int_pin != 0 {
            let pin = (int_pin - 1) & 3;
            let addr = ((ctx.device as u32) << 16) | ctx.function as u32;
            let mut irq_info = IrqInfo { gsi: u32::MAX, active_high: false, edge_triggered: false };
            let found = unsafe {
                acpi::acpi_get_irq(
                    ctx.root_bridge_pdo as *const u8,
                    ctx.bus, addr, pin,
                    &mut irq_info as *mut IrqInfo as *mut u8
                )
            };
            if found {
                info!("PCI {:02X}:{:02X}.{} pin=INT{} gsi={}",
                    ctx.bus, ctx.device, ctx.function, b'A' + pin, irq_info.gsi);
                entries[count] = ResEntry {
                    res_type: ResType::Interrupt,
                    desc: ResTypeDesc { interrupt: IntDesc {
                        irq: irq_info.gsi as usize,
                        vector: 0,
                        active_high: irq_info.active_high,
                        edge_triggered: irq_info.edge_triggered
                    }}
                };
                count += 1;
            }
        }
    }

    let fill_count = count.min(entry_limit);
    if fill_count > 0 {
        let out = unsafe { core::slice::from_raw_parts_mut(res_list.base, fill_count) };
        out[..fill_count].copy_from_slice(&entries[..fill_count]);
    }

    req.req_info.res_list.count = fill_count;
    req.complete_irp(Status::Success);
    Status::Success
}
