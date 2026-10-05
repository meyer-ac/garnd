use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use nix::sys::signal::{kill, SIGKILL};
use nix::time::{clock_gettime, ClockId};
use nix::unistd::{getpid, pause};
use crate::constants;

static EPOCH_START_SECONDS: AtomicI64 = AtomicI64::new(0);
static EPOCH_START_NANOSECONDS: AtomicI64 = AtomicI64::new(0);
static RELOADS_SINCE_EPOCH_START: AtomicUsize = AtomicUsize::new(0);

/// Exits the program immediately if too many reloads happened in a short time
/// or an error occurred in this function (in order to prevent possible reload loops).
pub fn circuit_breaker() {
    let epoch_start_seconds = EPOCH_START_SECONDS.load(Ordering::Acquire);
    let epoch_start_nanoseconds = EPOCH_START_NANOSECONDS.load(Ordering::Acquire);
    let reloads_since_epoch_start = RELOADS_SINCE_EPOCH_START.load(Ordering::Acquire);

    let current_time = clock_gettime(ClockId::CLOCK_MONOTONIC_COARSE).unwrap_or_else(|_| instant_exit());

    let seconds_since_epoch_start = current_time.tv_sec()
        - epoch_start_seconds
        - i64::from(epoch_start_nanoseconds > current_time.tv_nsec());

    #[allow(clippy::cast_possible_wrap)] // Won't happen, this constant is a small number
    if seconds_since_epoch_start > constants::CIRCUIT_BREAKER_EPOCH_SECONDS as i64 {
        EPOCH_START_SECONDS.store(current_time.tv_sec(), Ordering::Release);
        EPOCH_START_NANOSECONDS.store(current_time.tv_nsec(), Ordering::Release);
        RELOADS_SINCE_EPOCH_START.store(1, Ordering::Release);
        return;
    }

    if reloads_since_epoch_start + 1 >= constants::CIRCUIT_BREAKER_RELOADS_PER_EPOCH {
        instant_exit();
    }

    let _ = RELOADS_SINCE_EPOCH_START.fetch_add(1, Ordering::Release);
}

fn instant_exit() -> ! {
    let _ = kill(getpid(), SIGKILL);
    loop {
        pause();
    }
}
