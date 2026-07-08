use common::{FBInfo, align_up};
use crate::draw::blit_bmp;

// Due to our current selection logic, need to put smallest bmp as the first entry
const LOGO_VARIANTS: &[&[u8]] = &[
    include_bytes!("../../../resources/boot_logo/boot_logo_256.bmp"),
    include_bytes!("../../../resources/boot_logo/boot_logo_512.bmp"),
    include_bytes!("../../../resources/boot_logo/boot_logo_1024.bmp")
];

struct ParsedLogo<'a> {
    width: usize,
    height: usize,
    row_stride: usize,
    pixels: &'a [u8],
}

fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn read_i32(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

// Simple bitmap parser
fn parse(data: &[u8]) -> Option<ParsedLogo<'_>> {
    if data.len() < 54 || &data[0..2] != b"BM" {
        return None;
    }

    let off_bits = read_u32(data, 10) as usize;
    let bit_count = read_u16(data, 28);
    let compression = read_u32(data, 30);

    // We will only consider 3 byte pixel format for now
    if bit_count != 24 || compression != 0 {
        return None;
    }

    let width = read_i32(data, 18);
    let height = read_i32(data, 22);
    // height <= 0 covers both invalid data and top-down row order, which
    // this parser doesn't support — neither current asset needs it.
    if width <= 0 || height <= 0 {
        return None;
    }
    let (width, height) = (width as usize, height as usize);

    // Each row is padded to a 4-byte boundary.
    let row_stride = align_up(width * 3, 4);
    let expected = off_bits.checked_add(row_stride.checked_mul(height)?)?;
    if data.len() < expected {
        return None;
    }

    Some(ParsedLogo { width, height, row_stride, pixels: &data[off_bits..expected] })
}

// Picks the largest variant which fits within the screen,
// falling back to the smallest variant if none qualify.
fn select_logo(screen_width: usize, screen_height: usize) -> Option<ParsedLogo<'static>> {
    let mut best: Option<ParsedLogo<'static>> = None;
    for &variant in LOGO_VARIANTS {
        if let Some(parsed) = parse(variant) {
            if parsed.width <= screen_width && parsed.height <= screen_height {
                best = Some(parsed);
            }
        }
    }
    if best.is_some() {
        return best;
    }
    LOGO_VARIANTS.first().and_then(|&v| parse(v))
}

// Draws the logo centered horizontally, in the upper-middle of the screen so
// there's room for the animation below it. Returns the logo's bounding box
// (x, y, w, h) so the caller can position the animation relative to it.
pub fn draw_logo(fb: &FBInfo) -> (usize, usize, usize, usize) {
    let Some(logo) = select_logo(fb.width, fb.height) else {
        return (fb.width / 2, fb.height / 2, 0, 0);
    };
    let x = fb.width.saturating_sub(logo.width) / 2;
    let y = fb.height.saturating_sub(logo.height) / 3;
    blit_bmp(fb, x, y, logo.width, logo.height, logo.row_stride, logo.pixels);
    (x, y, logo.width, logo.height)
}
