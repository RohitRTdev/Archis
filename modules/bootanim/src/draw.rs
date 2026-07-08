use common::FBInfo;

#[inline]
fn channel_shift(mask: u32) -> u32 {
    if mask == 0 { 0 } else { mask.trailing_zeros() }
}

// Packs an 8-bit-per-channel color into the framebuffer's native pixel format.
pub fn pack_pixel(fb: &FBInfo, r: u8, g: u8, b: u8) -> u32 {
    let m = &fb.pixel_mask;
    (((r as u32) << channel_shift(m.red_mask)) & m.red_mask)
        | (((g as u32) << channel_shift(m.green_mask)) & m.green_mask)
        | (((b as u32) << channel_shift(m.blue_mask)) & m.blue_mask)
}

#[inline]
pub fn put_pixel(fb: &FBInfo, x: usize, y: usize, color: u32) {
    if x >= fb.width || y >= fb.height {
        return;
    }
    unsafe {
        let ptr = (fb.fb.base_address as *mut u32).add(y * fb.stride + x);
        *ptr = color;
    }
}

pub fn fill_rect(fb: &FBInfo, x0: usize, y0: usize, w: usize, h: usize, color: u32) {
    let x_end = (x0 + w).min(fb.width);
    let y_end = (y0 + h).min(fb.height);
    for y in y0..y_end {
        for x in x0..x_end {
            put_pixel(fb, x, y, color);
        }
    }
}

pub fn fill_circle(fb: &FBInfo, cx: isize, cy: isize, radius: isize, color: u32) {
    if radius <= 0 {
        return;
    }
    let r2 = radius * radius;
    let x0 = (cx - radius).max(0) as usize;
    let x1 = ((cx + radius + 1).max(0) as usize).min(fb.width);
    let y0 = (cy - radius).max(0) as usize;
    let y1 = ((cy + radius + 1).max(0) as usize).min(fb.height);
    for y in y0..y1 {
        let dy = y as isize - cy;
        for x in x0..x1 {
            let dx = x as isize - cx;
            if dx * dx + dy * dy <= r2 {
                put_pixel(fb, x, y, color);
            }
        }
    }
}

// Skip near-black source pixels: the BMP source has no alpha channel, and its
// background is pure/near-black, so this chroma-keys it transparent instead
// of drawing a visible black square.
const CHROMA_KEY_THRESHOLD: u8 = 8;

// Blits a bottom-up, BGR, 24-bit-per-pixel BMP pixel array 
// onto the framebuffer, flipping row order and swapping channels.
pub fn blit_bmp(fb: &FBInfo, dst_x: usize, dst_y: usize, w: usize, h: usize, row_stride: usize, pixels: &[u8]) {
    for row in 0..h {
        let y = dst_y + row;
        if y >= fb.height {
            break;
        }
        // BMP rows are stored bottom-up: file row 0 is the image's bottom row.
        let src_row_base = (h - 1 - row) * row_stride;
        for col in 0..w {
            let x = dst_x + col;
            if x >= fb.width {
                continue;
            }
            let idx = src_row_base + col * 3;
            let (b, g, r) = (pixels[idx], pixels[idx + 1], pixels[idx + 2]);
            if r < CHROMA_KEY_THRESHOLD && g < CHROMA_KEY_THRESHOLD && b < CHROMA_KEY_THRESHOLD {
                continue;
            }
            let color = pack_pixel(fb, r, g, b);
            put_pixel(fb, x, y, color);
        }
    }
}

// The framebuffer is mapped write-combining; drain the WC buffers after a
// batch of writes so they actually become visible on screen.
pub fn sfence() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}
