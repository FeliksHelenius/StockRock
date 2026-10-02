//! Frame pacing for the scroll animation.
//!
//! A dedicated thread sleeps on a high-resolution waitable timer and posts `WM_FRAME` to the
//! window while animation is wanted. When nothing needs to move the thread is parked, so between
//! data refreshes the process does no work at all.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::*};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW, INFINITE,
    SetWaitableTimer, TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::PCWSTR;

pub const WM_FRAME: u32 = WM_APP + 2;

pub struct Anim {
    active: AtomicBool,
    quit: AtomicBool,
    /// Set while a `WM_FRAME` is in the queue, so a busy UI thread never accumulates a backlog.
    frame_queued: AtomicBool,
    period_ms: AtomicU32,
    thread: OnceLock<Thread>,
}

impl Anim {
    pub fn start(hwnd: HWND, fps: u32) -> Arc<Self> {
        let anim = Arc::new(Self {
            active: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            frame_queued: AtomicBool::new(false),
            period_ms: AtomicU32::new(period_for(fps)),
            thread: OnceLock::new(),
        });
        let worker = anim.clone();
        let hwnd = hwnd.0 as isize;
        let handle = thread::Builder::new()
            .name("frames".into())
            .stack_size(128 * 1024)
            .spawn(move || run(&worker, hwnd))
            .expect("failed to spawn frame thread");
        let _ = anim.thread.set(handle.thread().clone());
        anim
    }

    pub fn set_fps(&self, fps: u32) {
        self.period_ms.store(period_for(fps), Relaxed);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Acquire)
    }

    /// Starts or stops frame delivery.
    pub fn set_active(&self, active: bool) {
        if self.active.swap(active, AcqRel) != active {
            self.wake();
        }
    }

    /// Call from the UI thread when a `WM_FRAME` has been handled.
    pub fn frame_done(&self) {
        self.frame_queued.store(false, Release);
    }

    pub fn quit(&self) {
        self.quit.store(true, Release);
        self.active.store(false, Release);
        self.wake();
    }

    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

fn period_for(fps: u32) -> u32 {
    (1000 / fps.max(1)).max(1)
}

fn run(anim: &Anim, hwnd: isize) {
    let hwnd = HWND(hwnd as *mut c_void);
    let timer = unsafe {
        // High-resolution timers need Windows 10 1803+; fall back to a regular timer otherwise.
        CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
        .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
    };
    let Ok(timer) = timer else { return };

    loop {
        while !anim.active.load(Acquire) {
            if anim.quit.load(Acquire) {
                unsafe {
                    let _ = CloseHandle(timer);
                }
                return;
            }
            thread::park();
        }

        let mut armed = 0;
        while anim.active.load(Acquire) {
            let period = anim.period_ms.load(Relaxed);
            if period != armed {
                // Negative due time = relative, in 100 ns units; then repeat every `period` ms.
                let due = -(period as i64) * 10_000;
                unsafe {
                    let _ = SetWaitableTimer(timer, &due, period as i32, None, None, false);
                }
                armed = period;
            }
            unsafe {
                WaitForSingleObject(timer, INFINITE);
            }
            if anim.active.load(Acquire) && !anim.frame_queued.swap(true, AcqRel) {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_FRAME, WPARAM(0), LPARAM(0));
                }
            }
        }
        unsafe {
            let _ = CancelWaitableTimer(timer);
        }
    }
}
