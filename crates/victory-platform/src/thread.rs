//! Threads: start with a chosen stack size, join for an exit code, identify.

use std::sync::Mutex;

/// Work to run on a thread of ours as it ends, while its title code's own state is still there:
/// `victory-hle`'s FLS destructors are one (a title's `FlsAlloc` with a callback arms it).
static ON_EXIT: Mutex<Vec<fn()>> = Mutex::new(Vec::new());

/// Registers `hook` to run on every thread of ours as it ends, by [`run_thread_exit_hooks`]. A
/// hook registered twice runs once, so the caller can arm it where it is first needed.
pub fn on_thread_exit(hook: fn()) {
    let mut hooks = ON_EXIT.lock().unwrap_or_else(|e| e.into_inner());
    if !hooks.contains(&hook) {
        hooks.push(hook);
    }
}

/// Runs the hooks [`on_thread_exit`] registered, on the calling thread as it ends. The hooks run
/// again for every thread, since what they look at is each thread's own.
pub fn run_thread_exit_hooks() {
    let hooks = lock().clone();
    for hook in hooks {
        hook();
    }
}

fn lock() -> std::sync::MutexGuard<'static, Vec<fn()>> {
    ON_EXIT.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
#[path = "win32/thread.rs"]
mod imp;
#[cfg(target_os = "macos")]
#[path = "macos/thread.rs"]
mod imp;

pub use imp::*;
