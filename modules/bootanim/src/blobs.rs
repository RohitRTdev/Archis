/* Animation credit goes to AI */

use alloc::vec;
use alloc::vec::Vec;
use common::{FBInfo, MemoryRegion};
use crate::draw;
use crate::trig;


const SPREAD_FRAMES: u32 = 28;
const ROTATE_FRAMES: u32 = 100;
const CONVERGE_FRAMES: u32 = 28;
const CYCLE_FRAMES: u32 = SPREAD_FRAMES + ROTATE_FRAMES + CONVERGE_FRAMES;

// More ring positions = a smoother-looking merged circle and a finer-grained
// (less chunky) reveal/disintegrate wave in draw_rotate.
const RING_POINTS: usize = 36;
const PRIMARY_INDICES: [usize; 3] = [0, RING_POINTS / 3, RING_POINTS * 2 / 3];

// Sizes/spacing below were tuned by eye against a screen whose shorter side
// is REFERENCE_DIM pixels, then scaled by the actual screen's shorter side
// so the animation looks proportionally right — and
// doesn't collide with the logo — at other resolutions. MIN_* floors keep it
// from disappearing on very small screens.
const REFERENCE_DIM: i32 = 1000;
const BASE_SPREAD_RADIUS: i32 = 30;
const BASE_BLOB_RADIUS: i32 = 4;
const BASE_DOT_RADIUS: i32 = 2;

const MIN_SPREAD_RADIUS: i32 = 24;
const MIN_BLOB_RADIUS: i32 = 2;
const MIN_DOT_RADIUS: i32 = 1;

fn scale_dim(base: i32, reference_px: i32) -> i32 {
    ((base as i64 * reference_px as i64) / REFERENCE_DIM as i64) as i32
}

const TOTAL_ROTATION_FP: u32 = trig::ONE;

// Color gradient (electric blue -> cyan -> spring green -> lime -> gold ->
// orange -> red), sampled at 7 stops. All blobs/points share one color at
// any instant, which slowly sweeps back and forth across this gradient 
// rather than each blob having its own hue.
const GRADIENT_STOPS: [(u8, u8, u8); 7] = [
    (0x0E, 0xA5, 0xFF),
    (0x00, 0xE5, 0xFF),
    (0x00, 0xFF, 0xC8),
    (0x8C, 0xFF, 0x00),
    (0xFF, 0xD5, 0x00),
    (0xFF, 0x7A, 0x00),
    (0xFF, 0x2A, 0x00),
];
const GRADIENT_SEGMENTS: u32 = (GRADIENT_STOPS.len() as u32) - 1;

// Frames to sweep from one end of the gradient to the other.
const GRADIENT_SWEEP_FRAMES: u32 = 300;

const DEG270: u32 = trig::ONE / 4 * 3;
const BASE_ANGLES_FP: [u32; 3] = [
    DEG270,
    (DEG270 + trig::ONE / 3) % trig::ONE,
    (DEG270 + trig::ONE * 2 / 3) % trig::ONE,
];

// Linearly interpolates GRADIENT_STOPS at a Q16 position t_fp in [0, ONE].
fn gradient_at(t_fp: u32) -> (u8, u8, u8) {
    let t = t_fp.min(trig::ONE - 1);
    let scaled_fp = t as u64 * GRADIENT_SEGMENTS as u64;
    let seg = (scaled_fp / trig::ONE as u64) as usize;
    let local_frac = (scaled_fp % trig::ONE as u64) as u32;

    let (r0, g0, b0) = GRADIENT_STOPS[seg];
    let (r1, g1, b1) = GRADIENT_STOPS[seg + 1];

    let lerp = |a: u8, b: u8| -> u8 {
        (a as i64 + (b as i64 - a as i64) * local_frac as i64 / trig::ONE as i64) as u8
    };

    (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

// A slow triangle-wave sweep across the gradient — every blob/point shares
// this single color at a given frame, instead of each having its own offset.
fn current_gradient_t_fp(frame: u32) -> u32 {
    let period = GRADIENT_SWEEP_FRAMES * 2;
    let pos = frame % period;
    let tri = if pos <= GRADIENT_SWEEP_FRAMES { pos } else { period - pos };
    (tri as u64 * trig::ONE as u64 / GRADIENT_SWEEP_FRAMES as u64) as u32
}

// Fraction of `total_frames` elapsed, as a Q16 fixed-point value in [0, ONE].
fn phase_t_fp(local_frame: u32, total_frames: u32) -> u32 {
    if total_frames == 0 {
        return trig::ONE;
    }
    ((local_frame as u64 * trig::ONE as u64) / total_frames as u64) as u32
}

fn point_pos(center_x: usize, center_y: usize, angle_fp: u32, radius: i32) -> (i32, i32) {
    let dx = radius * trig::cos1000(angle_fp) / 1000;
    let dy = radius * trig::sin1000(angle_fp) / 1000;
    (center_x as i32 + dx, center_y as i32 + dy)
}

// Circular distance (in ring steps) from ring index k to the nearest primary.
fn ring_distance(k: usize) -> usize {
    PRIMARY_INDICES.iter().map(|&p| {
        let diff = k.abs_diff(p);
        diff.min(RING_POINTS - diff)
    }).min().unwrap()
}

// Smoothed rise/plateau/fall trapezoid within a phase: eases 0 -> ONE over
// the first quarter, holds ONE over the middle half, eases back to 0 over
// the last quarter.
fn merge_factor(t_fp: u32) -> u32 {
    let rise_end = trig::ONE / 4;
    let fall_start = trig::ONE * 3 / 4;
    if t_fp <= rise_end {
        trig::smoothstep((t_fp as u64 * trig::ONE as u64 / rise_end as u64) as u32)
    } else if t_fp >= fall_start {
        let fall_len = trig::ONE - fall_start;
        let local = t_fp - fall_start;
        trig::ONE - trig::smoothstep((local as u64 * trig::ONE as u64 / fall_len as u64) as u32)
    } else {
        trig::ONE
    }
}

pub struct BlobAnimation {
    center_x: usize,
    center_y: usize,
    spread_radius: i32,
    blob_radius: i32,
    dot_radius: i32,
    // Off-screen composition buffer: as wide as the real framebuffer, tall
    // enough to hold the animation's bounding box. Each frame is drawn here,
    // then blitted to the real framebuffer in one sweep instead of many
    // scattered per-circle writes straight to write-combining hardware memory.
    canvas: Vec<u32>,
    canvas_width: usize,
    canvas_pixel_mask: common::PixelMask,
    box_y: usize,
    box_h: usize,
}

impl BlobAnimation {
    // logo_x/y/w/h is the logo's bounding box
    // The animation is centered under it, sized and spaced proportionally
    // to the screen so it neither looks oversized/undersized
    pub fn new(fb: &FBInfo, logo_x: usize, logo_y: usize, logo_w: usize, logo_h: usize) -> Self {
        let reference_px = fb.width.min(fb.height) as i32;

        let spread_radius = scale_dim(BASE_SPREAD_RADIUS, reference_px).max(MIN_SPREAD_RADIUS);
        let blob_radius = scale_dim(BASE_BLOB_RADIUS, reference_px).max(MIN_BLOB_RADIUS);
        let dot_radius = scale_dim(BASE_DOT_RADIUS, reference_px).max(MIN_DOT_RADIUS);

        let center_x = logo_x + logo_w / 2;
        let real_center_y = logo_y + logo_h - 50;

        let half = (spread_radius + blob_radius + 1).max(0) as usize;
        let box_y = real_center_y.saturating_sub(half);
        let box_h = half * 2;

        let canvas = vec![0u32; fb.width * box_h];

        Self {
            center_x,
            center_y: real_center_y - box_y,
            spread_radius,
            blob_radius,
            dot_radius,
            canvas,
            canvas_width: fb.width,
            canvas_pixel_mask: fb.pixel_mask,
            box_y,
            box_h,
        }
    }

    // A synthetic FBInfo over `self.canvas`'s backing memory 
    fn canvas_fb(&mut self) -> FBInfo {
        FBInfo {
            fb: MemoryRegion {
                base_address: self.canvas.as_mut_ptr() as usize,
                size: self.canvas.len() * 4,
            },
            width: self.canvas_width,
            height: self.box_h,
            stride: self.canvas_width,
            pixel_mask: self.canvas_pixel_mask,
        }
    }

    // Copies the composed canvas into the real framebuffer at row box_y, one
    // linear row at a time. Uses fb.stride (not fb.width) for the
    // destination offset since the real framebuffer's stride can pad past
    // the visible width.
    fn blit_canvas(&self, fb: &FBInfo) {
        let row_bytes = self.canvas_width * 4;
        let src = self.canvas.as_ptr() as *const u8;
        let dst = fb.fb.base_address as *mut u8;
        for row in 0..self.box_h {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add(row * row_bytes),
                    dst.add((self.box_y + row) * fb.stride * 4),
                    row_bytes,
                );
            }
        }
    }

    fn draw_blob(&self, fb: &FBInfo, angle_fp: u32, radius: i32, draw_radius: i32, color: (u8, u8, u8)) {
        let (x, y) = point_pos(self.center_x, self.center_y, angle_fp, radius);
        let packed = draw::pack_pixel(fb, color.0, color.1, color.2);
        draw::fill_circle(fb, x as isize, y as isize, draw_radius as isize, packed);
    }

    fn draw_spread(&self, fb: &FBInfo, local_frame: u32, color: (u8, u8, u8)) {
        let eased = trig::ease_trapezoid(phase_t_fp(local_frame, SPREAD_FRAMES));
        let radius = (self.spread_radius as i64 * eased as i64 / trig::ONE as i64) as i32;
        for &angle_fp in BASE_ANGLES_FP.iter() {
            self.draw_blob(fb, angle_fp, radius, self.blob_radius, color);
        }
    }

    fn draw_rotate(&self, fb: &FBInfo, local_frame: u32, color: (u8, u8, u8)) {
        let t_fp = phase_t_fp(local_frame, ROTATE_FRAMES);
        let rot_eased = trig::ease_trapezoid(t_fp);
        let rot_offset_fp = ((TOTAL_ROTATION_FP as u64 * rot_eased as u64) / trig::ONE as u64) as u32;
        let merge = merge_factor(t_fp);

        let max_dist = (RING_POINTS / PRIMARY_INDICES.len() / 2) as u32;
        // How far the "reveal wave" has swept outward from the 3 primaries,
        // in ring-distance units (Q16). Goes 0 -> max_dist as merge goes
        // 0 -> ONE, and symmetrically back down as it disintegrates.
        let reveal_extent_fp = merge * max_dist;
        let radius_shrink = ((self.blob_radius - self.dot_radius) as i64 * merge as i64 / trig::ONE as i64) as i32;
        let point_radius_base = self.blob_radius - radius_shrink;

        for k in 0..RING_POINTS {
            let d = ring_distance(k) as u32;

            // The 3 primaries (d=0) are always fully present — only their
            // size shrinks with merge, via point_radius_base above. Every
            // other ring position fades continuously in (and back out) over
            // a one-distance-unit-wide window as reveal_extent sweeps past
            // it, instead of snapping to visible/invisible — this is what
            // makes the coalesce/disintegrate read as a smooth wave rather
            // than points popping in in batches.
            let progress = if d == 0 {
                trig::ONE
            } else {
                let window_start_fp = (d - 1) * trig::ONE;
                if reveal_extent_fp <= window_start_fp {
                    0
                } else {
                    trig::smoothstep((reveal_extent_fp - window_start_fp).min(trig::ONE))
                }
            };
            if progress == 0 {
                continue;
            }

            let point_radius = (point_radius_base as i64 * progress as i64 / trig::ONE as i64) as i32;
            if point_radius <= 0 {
                continue;
            }

            let point_angle_fp = (BASE_ANGLES_FP[0] + (k as u64 * trig::ONE as u64 / RING_POINTS as u64) as u32) % trig::ONE;
            let angle_fp = (point_angle_fp + rot_offset_fp) % trig::ONE;
            self.draw_blob(fb, angle_fp, self.spread_radius, point_radius, color);
        }
    }

    fn draw_converge(&self, fb: &FBInfo, local_frame: u32, color: (u8, u8, u8)) {
        let eased = trig::ease_trapezoid(phase_t_fp(local_frame, CONVERGE_FRAMES));
        let radius = (self.spread_radius as i64 * (trig::ONE - eased) as i64 / trig::ONE as i64) as i32;
        for &angle_fp in BASE_ANGLES_FP.iter() {
            let final_angle = (angle_fp + TOTAL_ROTATION_FP) % trig::ONE;
            self.draw_blob(fb, final_angle, radius, self.blob_radius, color);
        }
    }

    pub fn draw_frame(&mut self, fb: &FBInfo, frame: u32) {
        let canvas = self.canvas_fb();
        
        // Erase the canvas
        draw::fill_rect(&canvas, 0, 0, canvas.width, canvas.height, 0);

        let cycle_frame = frame % CYCLE_FRAMES;
        let color = gradient_at(current_gradient_t_fp(frame));

        if cycle_frame < SPREAD_FRAMES {
            self.draw_spread(&canvas, cycle_frame, color);
        } else if cycle_frame < SPREAD_FRAMES + ROTATE_FRAMES {
            self.draw_rotate(&canvas, cycle_frame - SPREAD_FRAMES, color);
        } else {
            self.draw_converge(&canvas, cycle_frame - SPREAD_FRAMES - ROTATE_FRAMES, color);
        }

        self.blit_canvas(fb);
        draw::sfence();
    }
}
