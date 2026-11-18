//! Throttling and run control for the dedupe workers.
//!
//! The pool this module targets is a raidz of 5400 rpm drives where a plain
//! `find` saturates I/O for minutes, so every disk operation goes through a
//! [`RateLimiter`]. The limiter's sleeps are interruptible through
//! [`RunControl`] so pause and cancel take effect within a fraction of a second
//! even at a very low rate.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const WANT_RUN: u8 = 0;
const WANT_PAUSE: u8 = 1;
const WANT_STOP: u8 = 2;

/// Longest single sleep, so control requests are noticed quickly.
const SLEEP_SLICE: Duration = Duration::from_millis(200);

/// What the user asked a running worker to do. Shared between the worker
/// thread and the HTTP handlers.
pub struct RunControl {
    want: AtomicU8,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RunControl {
    pub fn new() -> Self {
        Self {
            want: AtomicU8::new(WANT_RUN),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    fn set(&self, value: u8) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // A stop request is final: a late "resume" must not undo it.
        if self.want.load(Ordering::SeqCst) != WANT_STOP {
            self.want.store(value, Ordering::SeqCst);
        }
        self.cv.notify_all();
    }

    pub fn pause(&self) {
        self.set(WANT_PAUSE);
    }

    pub fn resume(&self) {
        self.set(WANT_RUN);
    }

    pub fn stop(&self) {
        self.set(WANT_STOP);
    }

    pub fn stop_requested(&self) -> bool {
        self.want.load(Ordering::SeqCst) == WANT_STOP
    }

    pub fn pause_requested(&self) -> bool {
        self.want.load(Ordering::SeqCst) == WANT_PAUSE
    }

    /// Block while paused. Returns `false` when the worker must stop.
    pub fn wait_while_paused(&self) -> bool {
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        while self.want.load(Ordering::SeqCst) == WANT_PAUSE {
            guard = self
                .cv
                .wait_timeout(guard, Duration::from_secs(1))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        !self.stop_requested()
    }

    /// Sleep for `duration`, waking early on a stop request. Returns `false`
    /// when the worker must stop.
    pub fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.stop_requested() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = self
                .cv
                .wait_timeout(guard, (deadline - now).min(SLEEP_SLICE))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Token bucket. `rate` units per second with at most one second of burst;
/// `rate == 0` disables limiting.
pub struct RateLimiter {
    rate: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(rate: f64) -> Self {
        let rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            0.0
        };
        Self {
            rate,
            // Start empty: a resumed scan must not open with a burst.
            tokens: 0.0,
            last: Instant::now(),
        }
    }

    pub fn unlimited(&self) -> bool {
        self.rate == 0.0
    }

    /// Take `units` tokens, sleeping (interruptibly) until they are available.
    /// Returns `false` when a stop was requested while waiting.
    pub fn acquire(&mut self, units: f64, control: &RunControl) -> bool {
        if self.unlimited() {
            return !control.stop_requested();
        }
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate)
            .min(self.rate.max(units));
        self.last = now;
        self.tokens -= units;
        if self.tokens < 0.0 {
            let wait = Duration::from_secs_f64(-self.tokens / self.rate);
            if !control.sleep(wait) {
                return false;
            }
            // The deficit is paid by the sleep; refill restarts from here.
            self.tokens = 0.0;
            self.last = Instant::now();
        }
        true
    }
}

/// Lower the calling thread's I/O priority to the IDLE class.
///
/// Best effort: the CFQ/BFQ idle class has little effect on ZFS (which has its
/// own I/O scheduler), so the [`RateLimiter`] remains the real guard. Returns
/// whether the call succeeded, for the status line.
pub fn set_idle_io_priority() -> bool {
    #[cfg(target_os = "linux")]
    {
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        // `who = 0` targets the calling thread on Linux.
        // SAFETY: plain syscall with integer arguments.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS,
                0,
                IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
            )
        };
        rc == 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn limiter_enforces_rate() {
        let control = RunControl::new();
        let mut limiter = RateLimiter::new(100.0);
        let start = Instant::now();
        for _ in 0..30 {
            assert!(limiter.acquire(1.0, &control));
        }
        // 30 units at 100/s from an empty bucket: about 300 ms.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    }

    #[test]
    fn unlimited_never_sleeps() {
        let control = RunControl::new();
        let mut limiter = RateLimiter::new(0.0);
        let start = Instant::now();
        for _ in 0..10_000 {
            assert!(limiter.acquire(1.0, &control));
        }
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn stop_interrupts_a_long_wait() {
        let control = Arc::new(RunControl::new());
        let c2 = control.clone();
        let handle = std::thread::spawn(move || {
            let mut limiter = RateLimiter::new(0.1);
            limiter.acquire(1.0, &c2)
        });
        std::thread::sleep(Duration::from_millis(50));
        control.stop();
        let start = Instant::now();
        assert!(!handle.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stop_is_final() {
        let control = RunControl::new();
        control.stop();
        control.resume();
        assert!(control.stop_requested());
        assert!(!control.wait_while_paused());
    }
}
