use std::sync::OnceLock;
use std::thread::ThreadId;
use windows::System::{DispatcherQueue, DispatcherQueueController, DispatcherQueueHandler};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};

static UI_QUEUE: OnceLock<DispatcherQueue> = OnceLock::new();
static UI_THREAD_ID: OnceLock<ThreadId> = OnceLock::new();
static UI_OS_THREAD_ID: OnceLock<u32> = OnceLock::new();
static UI_DQ_CONTROLLER: OnceLock<DispatcherQueueController> = OnceLock::new();

/// Must be called on the UI thread during `Runtime::new`. Idempotent.
pub fn init_ui_dispatcher() {
    if UI_QUEUE.get().is_some() {
        return;
    }
    if let Ok(dq) = DispatcherQueue::GetForCurrentThread() {
        let _ = UI_THREAD_ID.set(std::thread::current().id());
        // Record the OS thread id for diagnostics.
        let os_tid = unsafe { GetCurrentThreadId() };
        let _ = UI_OS_THREAD_ID.set(os_tid);
        let _ = UI_QUEUE.set(dq);
        return;
    }

    // If no DispatcherQueue exists for this thread, attempt to create one.
    // This is similar to playground behavior where a DispatcherQueueController
    // is created so WinRT UI APIs can post back to this thread.
    let options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    };

    if let Ok(controller) = unsafe { CreateDispatcherQueueController(options) } {
        // Keep the controller alive for the lifetime of the process.
        let _ = UI_DQ_CONTROLLER.set(controller);

        // After creating the controller, GetForCurrentThread should succeed.
        if let Ok(dq2) = DispatcherQueue::GetForCurrentThread() {
            let _ = UI_THREAD_ID.set(std::thread::current().id());
            let os_tid = unsafe { GetCurrentThreadId() };
            let _ = UI_OS_THREAD_ID.set(os_tid);
            let _ = UI_QUEUE.set(dq2);
        }
    }
}

/// Post a closure to the UI thread from any thread.
/// If already on the UI thread, calls `f` directly. Falls back to inline
/// execution if the dispatcher was never initialised.
pub fn post_to_ui_thread(f: impl FnOnce() + Send + 'static) {
    let Some(dq) = UI_QUEUE.get() else {
        f();
        return;
    };

    if UI_THREAD_ID.get().copied() == Some(std::thread::current().id()) {
        f();
        return;
    }

    let cell = std::sync::Mutex::new(Some(f));
    let handler = DispatcherQueueHandler::new(move || {
        if let Some(f) = cell.lock().unwrap().take() {
            f();
        }
        Ok(())
    });
    let _ = dq.TryEnqueue(&handler);
}

/// Enqueue `f` as an ordinary work item on the XAML host's `DispatcherQueue`,
/// even when already on the UI thread (`post_to_ui_thread` would run it inline
/// there). Used to move work out of re-entrancy-sensitive XAML callouts such as
/// the render walk: the work item runs only after the current callout returns
/// to the dispatcher.
///
/// Returns `false` without running `f` when deferral is unavailable — no XAML
/// host queue on this thread (console hosts, workers, unit tests; a
/// runtime-owned `DispatcherQueueController` has no render walk to stay out of)
/// or the enqueue failed. The caller should then run the work inline.
pub fn defer_on_ui_thread(f: impl FnOnce() + Send + 'static) -> bool {
    if needs_win32_pump() {
        return false;
    }
    let Some(dq) = UI_QUEUE.get() else {
        return false;
    };
    if UI_THREAD_ID.get().copied() != Some(std::thread::current().id()) {
        return false;
    }
    let cell = std::sync::Mutex::new(Some(f));
    let handler = DispatcherQueueHandler::new(move || {
        if let Some(f) = cell.lock().unwrap().take() {
            f();
        }
        Ok(())
    });
    dq.TryEnqueue(&handler).unwrap_or(false)
}

/// Queues `f` on the UI thread's dispatcher -- always as a separate work item, never inline, even
/// from the UI thread (so it can't run inside a XAML callout). `false` if there is no dispatcher.
pub fn enqueue_on_ui_thread(f: impl FnOnce() + Send + 'static) -> bool {
    let Some(dq) = UI_QUEUE.get() else {
        return false;
    };
    let cell = std::sync::Mutex::new(Some(f));
    let handler = DispatcherQueueHandler::new(move || {
        if let Some(f) = cell.lock().unwrap().take() {
            f();
        }
        Ok(())
    });
    dq.TryEnqueue(&handler).unwrap_or(false)
}

pub fn is_initialized() -> bool {
    UI_QUEUE.get().is_some()
}

/// Returns true if the current thread is the recorded UI thread.
pub fn is_ui_thread() -> bool {
    UI_THREAD_ID.get().copied() == Some(std::thread::current().id())
}

/// Return the recorded UI thread's OS thread id, if available.
pub fn get_ui_thread_os_tid() -> Option<u32> {
    UI_OS_THREAD_ID.get().copied()
}

/// Return the recorded UI thread's Rust `ThreadId` as a debug string, if available.
pub fn get_ui_thread_rust_tid() -> Option<String> {
    UI_THREAD_ID.get().map(|id| format!("{:?}", id))
}

/// Returns true when the runtime owns its own `DispatcherQueueController` (i.e. no XAML
/// host is present). In that case the caller is responsible for pumping Win32 messages
/// so that WinRT async `Completed` callbacks can fire.
pub fn needs_win32_pump() -> bool {
    UI_DQ_CONTROLLER.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn not_initialized_on_mta_test_thread() {
        // Test threads are MTA — GetForCurrentThread returns an error.
        assert!(!is_initialized());
        assert!(UI_THREAD_ID.get().is_none());
    }

    #[test]
    fn post_fallback_runs_closure_inline() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = fired.clone();
        post_to_ui_thread(move || fired2.store(true, Ordering::SeqCst));
        assert!(
            fired.load(Ordering::SeqCst),
            "fallback must call f() synchronously"
        );
    }

    #[test]
    fn post_fallback_multiple_closures_all_execute() {
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let c = count.clone();
            post_to_ui_thread(move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn post_fallback_closure_sees_captured_value() {
        let result = Arc::new(std::sync::Mutex::new(0u32));
        let r2 = result.clone();
        post_to_ui_thread(move || {
            *r2.lock().unwrap() = 42;
        });
        assert_eq!(*result.lock().unwrap(), 42);
    }
}
