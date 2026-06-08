use core::cell::UnsafeCell;
use crate::mem::{MapFetchType, PageDescriptor};
use crate::sync::Spinlock;
use crate::{RemapEntry, RemapType::*, BOOT_INFO, REMAP_LIST};
use kernel_intf::debug;
use common::{ceil_div, MemoryRegion, PAGE_SIZE};

const PSF_MAGIC: u32 = 0x864AB572;
const CURSOR_HEIGHT: f64 = 0.2;

#[repr(C)]
#[derive(Copy, Clone)]
struct PSFHeader {
    magic: u32,
    version: u32,
    headersize: u32,
    flags: u32,
    numglyph: u32,
    bytesperglyph: u32,
    height: u32,
    width: u32,
}

// Include PSF data as part of kernel binary
static FONT_DATA: &[u8] = include_bytes!("../../../resources/zap-ext-light20.psf");

const FRAMEBUFFER_MAX_SIZE: usize = PAGE_SIZE * PAGE_SIZE;

#[repr(C)]
#[cfg_attr(target_arch = "x86_64", repr(align(4096)))]
struct Framebuffer {
    buffer: UnsafeCell<[u8; FRAMEBUFFER_MAX_SIZE]>,
}

unsafe impl Sync for Framebuffer {}

// Scratch buffer
static FRAMEBUFFER: Framebuffer = Framebuffer {
    buffer: UnsafeCell::new([0; FRAMEBUFFER_MAX_SIZE]),
};

pub struct FramebufferLogger {
    fb_base: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    current_x: usize,
    current_y: usize,
    font_header: PSFHeader,
    font_glyphs: *const u8,
    dirty_min_y: usize,
    dirty_max_y: usize,
    display_start_row: usize,
    is_cursor_disabled: bool,
    
    // Should be between 0-1
    cursor_height: f64
}

unsafe impl Send for FramebufferLogger {}

pub static FRAMEBUFFER_LOGGER: Spinlock<FramebufferLogger> = Spinlock::new(FramebufferLogger {
    fb_base: core::ptr::null_mut(),
    width: 0,
    height: 0,
    stride: 0,
    current_x: 0,
    current_y: 0,
    font_header: PSFHeader {
        magic: 0,
        version: 0,
        headersize: 0,
        flags: 0,
        numglyph: 0,
        bytesperglyph: 0,
        height: 0,
        width: 0,
    },
    font_glyphs: core::ptr::null(),
    dirty_min_y: usize::MAX,
    dirty_max_y: 0,
    display_start_row: 0,
    is_cursor_disabled: false,
    cursor_height: CURSOR_HEIGHT
});

impl FramebufferLogger {
    fn init(&mut self) {
        let boot_info = BOOT_INFO.get().unwrap();
        let fb_info = boot_info.framebuffer_desc;

        self.fb_base = fb_info.fb.base_address as *mut u8;
        self.width = fb_info.width;
        self.height = fb_info.height;
        self.stride = fb_info.stride;

        self.load_font();
        self.clear_screen();
    }

    fn load_font(&mut self) {
        if FONT_DATA.len() < 32 {
            panic!("Font data too small");
        }

        self.font_header.magic = u32::from_le_bytes([
            FONT_DATA[0], FONT_DATA[1], FONT_DATA[2], FONT_DATA[3],
        ]);
        self.font_header.version = u32::from_le_bytes([
            FONT_DATA[4], FONT_DATA[5], FONT_DATA[6], FONT_DATA[7],
        ]);
        self.font_header.headersize = u32::from_le_bytes([
            FONT_DATA[8], FONT_DATA[9], FONT_DATA[10], FONT_DATA[11],
        ]);
        self.font_header.flags = u32::from_le_bytes([
            FONT_DATA[12], FONT_DATA[13], FONT_DATA[14], FONT_DATA[15],
        ]);
        self.font_header.numglyph = u32::from_le_bytes([
            FONT_DATA[16], FONT_DATA[17], FONT_DATA[18], FONT_DATA[19],
        ]);
        self.font_header.bytesperglyph = u32::from_le_bytes([
            FONT_DATA[20], FONT_DATA[21], FONT_DATA[22], FONT_DATA[23],
        ]);
        self.font_header.height = u32::from_le_bytes([
            FONT_DATA[24], FONT_DATA[25], FONT_DATA[26], FONT_DATA[27],
        ]);
        self.font_header.width = u32::from_le_bytes([
            FONT_DATA[28], FONT_DATA[29], FONT_DATA[30], FONT_DATA[31],
        ]);

        // A panic at this stage is technically not correct, since panic internally calls
        // framebuffer_logger which results in double lock (since this call already holds lock)
        // This will cause system to hang. However, since we don't want system to continue boot
        // process if framebuffer init fails, this behaviour is fine
        if self.font_header.magic != PSF_MAGIC {
            panic!("Invalid PSF magic number: {:#X}", self.font_header.magic);
        }

        let glyph_offset = self.font_header.headersize as usize;
        if glyph_offset >= FONT_DATA.len() {
            panic!("Glyph offset beyond font data");
        }

        self.font_glyphs = unsafe { FONT_DATA.as_ptr().add(glyph_offset) };

        // PSF v2 might store height differently — derive it from bytes-per-glyph
        let expected_height =
            self.font_header.bytesperglyph / ceil_div(self.font_header.width, 8);
        if expected_height != self.font_header.height {
            self.font_header.height = expected_height;
        }
    }

    fn get_glyph(&self, char_code: u32) -> Option<*const u8> {
        if char_code >= self.font_header.numglyph {
            return None;
        }
        let glyph_offset = (char_code * self.font_header.bytesperglyph) as usize;
        Some(unsafe { self.font_glyphs.add(glyph_offset) })
    }

    pub fn clear_screen(&mut self) {
        let fb_size = self.height * self.stride * 4;

        self.display_start_row = 0;

        unsafe {
            ((*FRAMEBUFFER.buffer.get()).as_ptr() as *mut u8).write_bytes(0, fb_size);
            
            core::ptr::copy_nonoverlapping(
                (*FRAMEBUFFER.buffer.get()).as_ptr(),
                self.fb_base,
                fb_size,
            );
        }

        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!(
                "sfence", 
                options(nostack, preserves_flags)
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        self.current_x = 0;
        self.current_y = 0;
        self.dirty_min_y = usize::MAX;
        self.dirty_max_y = 0;
    }

    fn adjust_cursor_pos(&mut self) {
        if self.current_x >= self.width {
            self.current_x = 0;
            self.current_y += self.font_header.height as usize;
            if self.current_y >= self.height {
                self.scroll_screen();
            }
        }
    }

    fn put_char(&mut self, c: char, is_first: bool, is_last: bool) {
        if is_first {
            self.draw_cursor(false);
        }
        match c {
            '\n' => {
                self.current_x = 0;
                self.current_y += self.font_header.height as usize;
                if self.current_y >= self.height {
                    self.scroll_screen();
                }
            },
            '\r' => {
                self.current_x = 0;
            },
            '\t' => {
                let tab_width = self.font_header.width as usize * 4;
                self.current_x += tab_width - (self.current_x % tab_width);
                self.adjust_cursor_pos();
            },
            '\x08' => {
                // We will make it such that the cursor cannot go up a line
                // That simplifies the logic a lot
                self.current_x = self.current_x.saturating_sub(self.font_header.width as usize);
            },
            _ => {
                self.draw_char(c);
                self.current_x += self.font_header.width as usize;
                self.adjust_cursor_pos();
            }
        }
        if is_last {
            self.draw_cursor(true);
        }
    }

    fn draw_cursor(&mut self, is_visible: bool) {
        if self.is_cursor_disabled {
            return;
        }
        let start_x     = self.current_x;
        let start_y     = self.current_y; 
        let font_width  = self.font_header.width  as usize;
        let font_height = self.font_header.height as usize;

        if start_x >= self.width || start_y >= self.height {
            return;
        }
        let draw_width  = font_width.min(self.width  - start_x);
        let draw_height = font_height.min(self.height - start_y);
        let start_height = (draw_height as f64 * (1.0 - self.cursor_height)) as usize;
        self.mark_dirty(start_y, start_y + font_height);

        let logical_row = start_y / font_height;
        let max_rows    = self.height / font_height;
        let phys_row    = (self.display_start_row + logical_row) % max_rows;
        let phys_top    = phys_row * font_height;
        
        let scratch = unsafe { 
            (*FRAMEBUFFER.buffer.get()).as_ptr() as *mut u32 
        };
        let pixel_val = if is_visible {
            0x00AA_AAAA
        }
        else {
            0
        };
        for y in start_height..draw_height {
            let row_base = (phys_top + y) * self.stride + start_x;

            for x in 0..draw_width {
                unsafe { 
                    *scratch.add(row_base + x) = pixel_val;
                }
            }
        }
    }


    fn draw_char(&mut self, c: char) {
        let char_code = c as u32;

        let glyph_data = match self.get_glyph(char_code) {
            Some(g) => g,
            None => return,
        };

        let start_x     = self.current_x;
        let start_y     = self.current_y; 
        let font_width  = self.font_header.width  as usize;
        let font_height = self.font_header.height as usize;
        let bytes_per_row = ceil_div(self.font_header.width, 8) as usize;

        if start_x >= self.width || start_y >= self.height {
            return;
        }
        let draw_width  = font_width.min(self.width  - start_x);
        let draw_height = font_height.min(self.height - start_y);

        self.mark_dirty(start_y, start_y + font_height);

        let logical_row = start_y / font_height;
        let max_rows    = self.height / font_height;
        let phys_row    = (self.display_start_row + logical_row) % max_rows;
        // Physical Y of the topmost scanline of this character in the scratch buffer
        let phys_top    = phys_row * font_height;

        let scratch = unsafe { 
            (*FRAMEBUFFER.buffer.get()).as_ptr() as *mut u32 
        };

        for y in 0..draw_height {
            let row_base = (phys_top + y) * self.stride + start_x;

            // Pointer to the first glyph byte for this scanline
            let glyph_row = unsafe { 
                glyph_data.add(y * bytes_per_row) 
            };

            // Process 8 pixels per byte; read each glyph byte exactly once
            for bx in 0..bytes_per_row {
                let glyph_byte = unsafe { *glyph_row.add(bx) };
                let x_start = bx * 8;
                let x_end   = (x_start + 8).min(draw_width);

                for bit_pos in x_start..x_end {
                    let bit_index = 7 - (bit_pos - x_start);
                    if (glyph_byte & (1u8 << bit_index)) != 0 {
                        unsafe { 
                            *scratch.add(row_base + bit_pos) = 0x00AA_AAAA;
                        }
                    }
                    else {
                        unsafe { 
                            *scratch.add(row_base + bit_pos) = 0;
                        }
                    }
                }
            }
        }
    }
    
    pub fn disable_cursor(&mut self) {
        self.is_cursor_disabled = true;
    }

    // Expand the dirty logical-scanline range to cover [y_start, y_end).
    fn mark_dirty(&mut self, y_start: usize, y_end: usize) {
        self.dirty_min_y = self.dirty_min_y.min(y_start);
        self.dirty_max_y = self.dirty_max_y.max(y_end);
    }

    fn scroll_screen(&mut self) {
        let font_height = self.font_header.height as usize;
        let max_rows = self.height / font_height;
        let row_bytes = font_height * self.stride * 4;

        // Advance ring-buffer head 
        self.display_start_row = (self.display_start_row + 1) % max_rows;

        // Clear last logical line 
        let new_bottom_phys = (self.display_start_row + max_rows - 1) % max_rows;
        unsafe {
            ((*FRAMEBUFFER.buffer.get())
                .as_ptr()
                .add(new_bottom_phys * row_bytes) as *mut u8)
                .write_bytes(0, row_bytes);
        }

        // Mark entire screen dirty 
        self.mark_dirty(0, self.height);

        self.current_y -= font_height;
    }

    pub fn write(&mut self, s: &str) {
        for (idx, c) in s.chars().enumerate() {
            let is_first = idx == 0;
            let is_last = idx == s.len() - 1;
            self.put_char(c, is_first, is_last);
        }
    }
}

impl core::fmt::Write for FramebufferLogger {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write(s);
        Ok(())
    }
}

pub fn flush_log() {
    let mut l = FRAMEBUFFER_LOGGER.lock();
    if l.dirty_max_y <= l.dirty_min_y {
        return; // nothing to flush
    }

    let s = (
        l.dirty_min_y,
        l.dirty_max_y,
        l.fb_base as usize,
        l.stride,
        l.height,
        l.font_header.height as usize,
        l.display_start_row,
    );

    l.dirty_min_y = usize::MAX;
    l.dirty_max_y = 0;

    let (dirty_min_y, dirty_max_y, fb_base, stride, height, font_height, display_start_row) = s;

    let max_rows  = height / font_height;
    let row_bytes = font_height * stride * 4; // bytes for one full text row

    // Convert pixel-unit dirty range to text-row range.
    // dirty_min_y / dirty_max_y are always multiples of font_height (cursor
    // advances by font_height; scroll marks height which equals max_rows * font_height).
    let dirty_min_row = dirty_min_y / font_height;
    let dirty_max_row = dirty_max_y / font_height;
    let num_rows = dirty_max_row - dirty_min_row;

    // Physical scratch-buffer row that holds logical row dirty_min_row
    let phys_start = (display_start_row + dirty_min_row) % max_rows;
    // phys_end may exceed max_rows — that signals a ring-buffer wrap
    let phys_end   = phys_start + num_rows;

    let scratch = unsafe { (*FRAMEBUFFER.buffer.get()).as_ptr() };

    if phys_end <= max_rows {
        // No wrap => one contiguous copy
        let src = unsafe { scratch.add(phys_start * row_bytes) };
        let dst = (fb_base as *mut u8).wrapping_add(dirty_min_row * row_bytes);
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, num_rows * row_bytes);
        }
    } else {
        // Ring-buffer wrap => 2 contiguous copies
        let part1_rows = max_rows - phys_start; 
        let part2_rows = num_rows - part1_rows; 

        let src1 = unsafe { scratch.add(phys_start * row_bytes) };
        let dst1 = (fb_base as *mut u8).wrapping_add(dirty_min_row * row_bytes);
        unsafe {
            core::ptr::copy_nonoverlapping(src1, dst1, part1_rows * row_bytes);
        }

        let src2 = scratch;
        let dst2 = (fb_base as *mut u8).wrapping_add((dirty_min_row + part1_rows) * row_bytes);
        unsafe {
            core::ptr::copy_nonoverlapping(src2, dst2, part2_rows * row_bytes);
        }
    }

    // Drain all Write-Combining Buffers so pixels are visible on screen.
    // sfence is sufficient (and cheaper than mfence) for WC draining.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            "sfence", 
            options(nostack, preserves_flags)
        );
    }

    #[cfg(not(target_arch = "x86_64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

pub fn init() {
    let mut logger = FRAMEBUFFER_LOGGER.lock();
    logger.init();

    REMAP_LIST.lock().add_node(RemapEntry {
        value: MemoryRegion {
            base_address: logger.fb_base as usize,
            size: logger.height * logger.stride * 4,
        },
        map_type: OffsetMapped(|new_fb_base| {
            debug!("Framebuffer relocated to new base:{:#X}", new_fb_base);
            // Do not change framebuffer address here.
            // We will do it right before switching to the new address space.
        }),
        flags: PageDescriptor::WC,
    })
    .unwrap();
}

pub fn relocate_framebuffer() {
    let mut logger = FRAMEBUFFER_LOGGER.lock();
    let new_fb_base = crate::mem::get_virtual_address(
        logger.fb_base as usize,
        0,
        MapFetchType::Any,
    )
    .expect("Could not find virtual address for boot display framebuffer");

    let new_font_glyph_ptr = crate::mem::get_virtual_address(
        logger.font_glyphs as usize,
        0,
        MapFetchType::Kernel,
    )
    .expect("Could not find virtual address for boot font glyphs");

    logger.fb_base = new_fb_base as *mut u8;
    logger.font_glyphs = new_font_glyph_ptr as *const u8;
}
