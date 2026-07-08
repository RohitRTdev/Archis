/* Animation credit goes to AI */

// Fixed-point trig/easing helpers. No libm is available in this no_std
// module, so sin/cos come from a precomputed lookup table instead of being
// computed at runtime.

// Q16 fixed point: ONE represents 1.0 (and, for angles, a full 360-degree turn).
pub const ONE: u32 = 1 << 16;

// sin(2*pi*i/256) * 1000, for i in 0..256 (one full turn per 256 steps).
const SIN_LUT: [i32; 256] = [
    0, 25, 49, 74, 98, 122, 147, 171, 195, 219, 243, 267, 290, 314, 337, 360,
    383, 405, 428, 450, 471, 493, 514, 535, 556, 576, 596, 615, 634, 653, 672, 690,
    707, 724, 741, 757, 773, 788, 803, 818, 831, 845, 858, 870, 882, 893, 904, 914,
    924, 933, 942, 950, 957, 964, 970, 976, 981, 985, 989, 992, 995, 997, 999, 1000,
    1000, 1000, 999, 997, 995, 992, 989, 985, 981, 976, 970, 964, 957, 950, 942, 933,
    924, 914, 904, 893, 882, 870, 858, 845, 831, 818, 803, 788, 773, 757, 741, 724,
    707, 690, 672, 653, 634, 615, 596, 576, 556, 535, 514, 493, 471, 450, 428, 405,
    383, 360, 337, 314, 290, 267, 243, 219, 195, 171, 147, 122, 98, 74, 49, 25,
    0, -25, -49, -74, -98, -122, -147, -171, -195, -219, -243, -267, -290, -314, -337, -360,
    -383, -405, -428, -450, -471, -493, -514, -535, -556, -576, -596, -615, -634, -653, -672, -690,
    -707, -724, -741, -757, -773, -788, -803, -818, -831, -845, -858, -870, -882, -893, -904, -914,
    -924, -933, -942, -950, -957, -964, -970, -976, -981, -985, -989, -992, -995, -997, -999, -1000,
    -1000, -1000, -999, -997, -995, -992, -989, -985, -981, -976, -970, -964, -957, -950, -942, -933,
    -924, -914, -904, -893, -882, -870, -858, -845, -831, -818, -803, -788, -773, -757, -741, -724,
    -707, -690, -672, -653, -634, -615, -596, -576, -556, -535, -514, -493, -471, -450, -428, -405,
    -383, -360, -337, -314, -290, -267, -243, -219, -195, -171, -147, -122, -98, -74, -49, -25,
];

// angle_fp is a Q16 fraction of a full turn (0..=ONE == 0..=360 degrees).
fn lut_index(angle_fp: u32) -> usize {
    ((angle_fp >> 8) & 0xFF) as usize
}

// Returns sin(angle) * 1000.
pub fn sin1000(angle_fp: u32) -> i32 {
    SIN_LUT[lut_index(angle_fp)]
}

// Returns cos(angle) * 1000. cos(x) = sin(x + quarter turn); a quarter turn
// is exactly 64 LUT steps (256 / 4), so this just offsets the index.
pub fn cos1000(angle_fp: u32) -> i32 {
    SIN_LUT[(lut_index(angle_fp) + 64) & 0xFF]
}

// Smoothstep easing (3t^2 - 2t^3), t and the result both Q16 fixed-point in
// [0, ONE]. Its derivative is zero at both ends and peaks at the midpoint —
// exactly the low-high-low "velocity" profile used throughout the boot
// animation (radial spread/converge, rotation speed, merge amount).
pub fn smoothstep(t_fp: u32) -> u32 {
    let t = t_fp.min(ONE) as u64;
    let one = ONE as u64;
    let t2 = t * t / one;
    let t3 = t2 * t / one;
    (3 * t2 - 2 * t3) as u32
}

// Ramp fraction on each end of ease_trapezoid: 25% accelerate, 50% cruise,
// 25% decelerate.
const RAMP_FRAC: u64 = (ONE / 4) as u64;

// Trapezoidal velocity ease: linear acceleration over the first RAMP_FRAC of
// t, constant cruise velocity through the middle, linear deceleration over
// the last RAMP_FRAC. Position and velocity are both continuous across the
// two internal boundaries, and velocity is zero at t=0 and t=ONE (same soft
// launch/stop as smoothstep) — but unlike smoothstep, which only reaches
// peak velocity for an instant at the midpoint, this holds a real cruise
// speed for half the duration, which reads as distinctly faster overall.
pub fn ease_trapezoid(t_fp: u32) -> u32 {
    let one = ONE as u64;
    let r = RAMP_FRAC;
    let t = t_fp.min(ONE) as u64;

    // Cruise (peak) velocity in Q16, chosen so the area under the
    // accelerate/cruise/decelerate velocity profile over [0, ONE] is
    // exactly ONE: v * (r + (one - 2r) + r) / one = one  =>  v = one^2 / (one - r).
    let v_peak = one * one / (one - r);

    // Position reached after accelerating for duration `dt` (dt in [0, r]):
    // p(dt) = v_peak * dt^2 / (2r), all Q16.
    let ramp_pos = |dt: u64| -> u64 { (v_peak * dt / r) * dt / (2 * one) };

    let pos = if t <= r {
        ramp_pos(t)
    } else if t >= one - r {
        one - ramp_pos(one - t)
    } else {
        ramp_pos(r) + v_peak * (t - r) / one
    };

    pos.min(one) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_trapezoid_boundaries() {
        assert_eq!(ease_trapezoid(0), 0);
        assert_eq!(ease_trapezoid(ONE), ONE);
    }

    #[test]
    fn ease_trapezoid_monotonic() {
        let mut prev = 0u32;
        let mut t = 0u32;
        while t <= ONE {
            let p = ease_trapezoid(t);
            assert!(p >= prev, "position went backwards at t={t}: {p} < {prev}");
            prev = p;
            t += ONE / 256;
        }
    }

    #[test]
    fn ease_trapezoid_matches_derivation() {
        // At the end of the ramp-up (t = ONE/4), the trapezoid should have
        // covered ~1/6 of the total distance (v_peak * r / 2 with
        // v_peak = 1/(1-r), r = 0.25 -> 1/6).
        let quarter = ease_trapezoid(ONE / 4);
        let expected = ONE / 6;
        let tolerance = ONE / 100;
        assert!(
            quarter.abs_diff(expected) <= tolerance,
            "expected ~{expected}, got {quarter}"
        );
    }
}
