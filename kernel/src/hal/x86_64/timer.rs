use crate::cpu::{MAX_CPUS, PerCpu}; 
use crate::devices::HPET;
use crate::sched::QUANTUM;
use super::asm;
use kernel_intf::info;
use super::lapic;
use super::features::CPU_FEATURES;
use core::sync::atomic::{AtomicUsize, Ordering};

pub static BASE_COUNT: PerCpu<AtomicUsize> = PerCpu::new_with([const {AtomicUsize::new(0)}; MAX_CPUS]);
static TSC_FREQ_HZ: PerCpu<AtomicUsize> = PerCpu::new_with([const {AtomicUsize::new(0)}; MAX_CPUS]);

const MAX_SAMPLE_JITTER_FS: u64 = 50_000_000_000; // 50us (in femtoseconds)

// This is more useful when running on VM or an emulator. If there is significant difference
// between the time stamps of 2 close time measurements, we can concur that a vm vcpu schedule
// has happened. So we abandon that read and try again. This is used only for the timer
// calibration code in init() fn below and normal delay_ns path doesn't take it.
fn clean_sample(hpet: &crate::devices::Hpet, metric: &mut impl FnMut() -> u64) -> (u64, u64) {
    loop {
        let t0 = hpet.read_counter();
        let val = metric();
        let t1 = hpet.read_counter();

        if t1.wrapping_sub(t0) * hpet.clk_period as u64 <= MAX_SAMPLE_JITTER_FS {
            return (val, t1);
        }
    }
}

fn measure_over(min_ns: usize, mut metric: impl FnMut() -> u64, need_clean_sample: bool) -> (u64, u64, u64) {
    // This stops kernel from pre-empting, so only use for small infrequent delays
    // We use the platform HPET for this purpose
    let hpet = HPET.lock();

    // Convert the wait time required to femtoseconds
    let total_time = (min_ns * 1_000_000) as u64;

    let (old, start_ticks) = if need_clean_sample {
        clean_sample(&hpet, &mut metric)
    }
    else {
        (0, hpet.read_counter())
    };

    loop {
        let cur_ticks = hpet.read_counter();

        if cur_ticks.wrapping_sub(start_ticks) * hpet.clk_period as u64 >= total_time {
            break;
        }
        core::hint::spin_loop();
    }

    let (new, end_ticks) = if need_clean_sample {
        clean_sample(&hpet, &mut metric)
    }
    else {
        (0, hpet.read_counter())
    };

    let elapsed_fs = end_ticks.wrapping_sub(start_ticks) * hpet.clk_period as u64;

    (old, new, elapsed_fs)
}

// Smallest granularity timer
pub fn delay_ns(value: usize) {
    measure_over(value, || 0, false);
}

pub fn init() {
    // Measure the CPU clock frequency
    let (old, new, elapsed_fs) = measure_over(1_000_000, asm::rdtsc, true);

    let num_ticks_passed = new.wrapping_sub(old);
    // frequency = ticks / elapsed_seconds = ticks * fs_per_sec / elapsed_fs
    let base_freq = (num_ticks_passed as u128 * 1_000_000_000_000_000u128 / elapsed_fs as u128) as u64;
    TSC_FREQ_HZ.local().store(base_freq as usize, Ordering::Relaxed);

    info!("CPU Base Clock frequency measured as {}Hz", base_freq);

    // Now measure APIC timer. It's a 32-bit countdown timer that reloads and keeps
    // going once it hits 0 (periodic mode), so an unusually long stall during the
    // measurement window could wrap it. Detect that (count would appear to have
    // gone up) and retry rather than trust a corrupted delta.
    const MAX_ATTEMPTS: u32 = 5;
    let mut apic_freq = None;

    for attempt in 1..=MAX_ATTEMPTS {
        lapic::init_timer();

        let (old, new, elapsed_fs) = measure_over(1_000_000, || lapic::get_timer_value() as u64, true);
        let (old, new) = (old as u32, new as u32);

        if new > old {
            info!("APIC timer wrapped during calibration, retrying (attempt {})", attempt);
            continue;
        }

        // This is a countdown timer
        let num_ticks_passed = old.wrapping_sub(new) as u64;
        apic_freq = Some((num_ticks_passed as u128 * 1_000_000_000_000_000u128 / elapsed_fs as u128) as u64);
        break;
    }

    let apic_freq = apic_freq.expect("APIC timer calibration failed");
    info!("CPU APIC Clock frequency measured as {}Hz", apic_freq * 128);

    let init_count = apic_freq as usize / (1000 / QUANTUM);
    assert!(init_count <= 0xffffffff);
    info!("Init count calculated as {:#X}", init_count);

    BASE_COUNT.local().store(init_count, Ordering::Relaxed);

    lapic::setup_timer();
}

pub fn get_time_ms() -> u64 {
    if CPU_FEATURES.get().unwrap().lock().tsc_invariant {
        let freq = TSC_FREQ_HZ.local().load(Ordering::Relaxed) as u64;
        if freq == 0 {
            return 0;
        }
        return asm::rdtsc() * 1000 / freq;
    }

    // TSC isn't invariant on this CPU, so its rate can drift with P-states.
    // Fall back to the HPET
    let hpet = HPET.lock();
    (hpet.read_counter() as u128 * hpet.clk_period as u128 / 1_000_000_000_000) as u64
}
