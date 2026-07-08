#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicBool, Ordering};

use kernel_intf::{
    ExitInfo, acquire_screen_ownership, info, release_screen_ownership,
    sched_delay_ms, sched_exit_process, sched_get_cur_process_arg, sched_get_num_process_args,
};

mod blobs;
mod draw;
mod logo;
mod trig;

use blobs::BlobAnimation;

const FRAME_INTERVAL_MS: usize = 16;

static STOP: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicBool = AtomicBool::new(false);

#[kmod::init]
fn module_init() {
    let num_args = sched_get_num_process_args();
    let run = num_args > 1 && unsafe { sched_get_cur_process_arg(1).as_str() } == "run";

    if run {
        run_animation();
    }
    // Otherwise this load is just the kernel resolving the stop_boot_animation
    // export (loader::load_image() caches by path, so this doesn't re-run
    // anything) — nothing to do.
}

fn run_animation() {
    let Some(fb) = acquire_screen_ownership() else {
        info!("bootanim: could not acquire screen ownership");
        sched_exit_process(ExitInfo::normal(1));
    };
    STOP.store(false, Ordering::Release);

    let (lx, ly, lw, lh) = logo::draw_logo(&fb);
    draw::sfence();

    let mut anim = BlobAnimation::new(&fb, lx, ly, lw, lh);

    let mut frame: u32 = 0;
    while !STOP.load(Ordering::Acquire) {
        anim.draw_frame(&fb, frame);
        frame = frame.wrapping_add(1);
        sched_delay_ms(FRAME_INTERVAL_MS);
    }

    release_screen_ownership();
    STOPPED.store(true, Ordering::Release);
    sched_exit_process(ExitInfo::normal(0));
}

#[kmod::export]
extern "C" fn stop_boot_animation() {
    STOP.store(true, Ordering::Release);
}

#[kmod::export]
extern "C" fn is_boot_animation_stopped() -> bool {
    STOPPED.load(Ordering::Acquire)
}
