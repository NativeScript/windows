//! The Node-specific half of Node-API (`node_api.h`) for native addons on the classic engine.
//!
//! The engine half (`js_native_api.h`: values, objects, references, wrapping, …) comes from the
//! V8 shim in `napi-v8-shim`. What Node itself provides on top -- threadsafe functions, async
//! work, env cleanup hooks, buffers, callback scopes, `napi_module_register` -- is implemented here
//! and exported from `nativescript.dll`, so an addon built for Node (napi-rs, node-addon-api, …)
//! resolves every symbol it imports.
//!
//! Anything that must run on the JS thread goes through one job queue. Other threads push jobs and
//! wake the UI thread with a `DispatcherQueue` work item; `runtime_pump_timers` drains it too, so a
//! host without a dispatcher still makes progress. Each job runs in its own V8 scope and ends with
//! a microtask checkpoint, like a timer task.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::DELEGATE_ISOLATE_PTR;

#[allow(non_camel_case_types)]
pub type napi_env = *mut c_void;
#[allow(non_camel_case_types)]
pub type napi_value = *mut c_void;
#[allow(non_camel_case_types)]
type napi_ref = *mut c_void;
#[allow(non_camel_case_types)]
type napi_status = i32;

const NAPI_OK: napi_status = 0;
const NAPI_INVALID_ARG: napi_status = 1;
const NAPI_GENERIC_FAILURE: napi_status = 9;
const NAPI_CANCELLED: napi_status = 11;
const NAPI_QUEUE_FULL: napi_status = 15;
const NAPI_CLOSING: napi_status = 16;

const UINT8_ARRAY: i32 = 1;

#[allow(non_camel_case_types)]
type napi_finalize = Option<unsafe extern "C" fn(env: napi_env, data: *mut c_void, hint: *mut c_void)>;
#[allow(non_camel_case_types)]
type napi_threadsafe_function_call_js =
    Option<unsafe extern "C" fn(env: napi_env, js_callback: napi_value, context: *mut c_void, data: *mut c_void)>;
#[allow(non_camel_case_types)]
type napi_async_execute_callback = Option<unsafe extern "C" fn(env: napi_env, data: *mut c_void)>;
#[allow(non_camel_case_types)]
type napi_async_complete_callback = Option<unsafe extern "C" fn(env: napi_env, status: napi_status, data: *mut c_void)>;
#[allow(non_camel_case_types)]
type napi_cleanup_hook = Option<unsafe extern "C" fn(arg: *mut c_void)>;
#[allow(non_camel_case_types)]
type napi_async_cleanup_hook = Option<unsafe extern "C" fn(handle: *mut c_void, arg: *mut c_void)>;

// The engine half, from the V8 shim linked into the same DLL.
extern "C" {
    fn napi_open_handle_scope(env: napi_env, result: *mut *mut c_void) -> napi_status;
    fn napi_close_handle_scope(env: napi_env, scope: *mut c_void) -> napi_status;
    fn napi_create_reference(env: napi_env, value: napi_value, initial: u32, result: *mut napi_ref) -> napi_status;
    fn napi_delete_reference(env: napi_env, reference: napi_ref) -> napi_status;
    fn napi_get_reference_value(env: napi_env, reference: napi_ref, result: *mut napi_value) -> napi_status;
    fn napi_get_undefined(env: napi_env, result: *mut napi_value) -> napi_status;
    fn napi_call_function(
        env: napi_env,
        recv: napi_value,
        func: napi_value,
        argc: usize,
        argv: *const napi_value,
        result: *mut napi_value,
    ) -> napi_status;
    fn napi_create_arraybuffer(env: napi_env, length: usize, data: *mut *mut c_void, result: *mut napi_value) -> napi_status;
    fn napi_create_external_arraybuffer(
        env: napi_env,
        data: *mut c_void,
        length: usize,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_value,
    ) -> napi_status;
    fn napi_create_typedarray(
        env: napi_env,
        kind: i32,
        length: usize,
        arraybuffer: napi_value,
        byte_offset: usize,
        result: *mut napi_value,
    ) -> napi_status;
    fn napi_is_typedarray(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
    fn napi_is_dataview(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
    fn napi_get_typedarray_info(
        env: napi_env,
        value: napi_value,
        kind: *mut i32,
        length: *mut usize,
        data: *mut *mut c_void,
        arraybuffer: *mut napi_value,
        byte_offset: *mut usize,
    ) -> napi_status;
    fn napi_get_dataview_info(
        env: napi_env,
        value: napi_value,
        byte_length: *mut usize,
        data: *mut *mut c_void,
        arraybuffer: *mut napi_value,
        byte_offset: *mut usize,
    ) -> napi_status;
    fn napi_is_exception_pending(env: napi_env, result: *mut bool) -> napi_status;
    fn napi_get_and_clear_last_exception(env: napi_env, result: *mut napi_value) -> napi_status;
    fn napi_coerce_to_string(env: napi_env, value: napi_value, result: *mut napi_value) -> napi_status;
    fn napi_get_value_string_utf8(
        env: napi_env,
        value: napi_value,
        buf: *mut c_char,
        bufsize: usize,
        result: *mut usize,
    ) -> napi_status;
}

// ---------------------------------------------------------------------------------------------
// The JS-thread job queue.

enum Job {
    Tsfn(Arc<Tsfn>),
    AsyncComplete(usize),
}

// Jobs only carry `Arc<Tsfn>` (Send+Sync below) and pointers used on the JS thread.
unsafe impl Send for Job {}

static QUEUE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
static WAKE_QUEUED: AtomicBool = AtomicBool::new(false);
/// Set by [`teardown`]: the runtime is going away, so late work (a cleanup hook releasing a
/// threadsafe function, a dispatcher item that runs during shutdown) must not touch V8.
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

fn push(job: Job) {
    if SHUT_DOWN.load(Ordering::Acquire) {
        return;
    }
    QUEUE.lock().unwrap().push_back(job);
    wake();
}

fn wake() {
    if SHUT_DOWN.load(Ordering::Acquire) || WAKE_QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    // A dispatcher work item runs between frames, never inside a XAML callout. Hosts without one
    // (console apps, tests) drain from their pump loop instead.
    if !crate::ui_dispatcher::enqueue_on_ui_thread(|| {
        let _ = std::panic::catch_unwind(drain);
    }) {
        WAKE_QUEUED.store(false, Ordering::Release);
    }
}

/// Runs every queued JS-thread job and every env's deferred finalizers. Called from the dispatcher
/// wake-up and from `runtime_pump_timers`; a no-op when there is nothing to do.
pub fn drain() {
    WAKE_QUEUED.store(false, Ordering::Release);
    if SHUT_DOWN.load(Ordering::Acquire) {
        return;
    }
    loop {
        let job = QUEUE.lock().unwrap().pop_front();
        let Some(job) = job else { break };
        match job {
            Job::Tsfn(tsfn) => in_js_scope(|| unsafe { tsfn.dispatch() }),
            Job::AsyncComplete(work) => in_js_scope(|| unsafe { AsyncWork::complete(work as *mut AsyncWork) }),
        }
    }
    drain_finalizers();
}

thread_local! {
    /// Envs created for native addons on this (the JS) thread.
    static ENVS: RefCell<Vec<napi_env>> = const { RefCell::new(Vec::new()) };
    /// `napi_open_callback_scope` depth; microtasks run when the outermost scope closes.
    static CALLBACK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) fn register_env(env: napi_env) {
    // A new runtime on this thread loads addons again.
    SHUT_DOWN.store(false, Ordering::Release);
    ENVS.with(|e| e.borrow_mut().push(env));
}

fn drain_finalizers() {
    let envs: Vec<napi_env> = ENVS.with(|e| e.borrow().clone());
    for env in envs {
        if unsafe { napi_v8_shim::ns_napi_has_pending_finalizers(env) } {
            in_js_scope(|| unsafe { napi_v8_shim::ns_napi_drain_finalizers(env) });
        }
    }
}

/// Tears down every addon env (runs cleanup hooks first, LIFO, as Node does at exit).
pub fn teardown() {
    let envs: Vec<napi_env> = ENVS.with(|e| std::mem::take(&mut *e.borrow_mut()));
    if envs.is_empty() {
        return;
    }
    // Deliver what is already queued while everything is still alive, then stop accepting work.
    drain();
    SHUT_DOWN.store(true, Ordering::Release);
    QUEUE.lock().unwrap().clear();
    for &env in envs.iter().rev() {
        run_cleanup_hooks(env);
        in_js_scope(|| unsafe { napi_v8_shim::ns_napi_env_teardown(env) });
    }
}

/// Runs `f` with the main context entered, reports an exception it leaves pending, and ends with a
/// microtask checkpoint (deferred when inside a XAML callout).
fn in_js_scope(f: impl FnOnce()) {
    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return;
    }
    let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
    v8::scope!(scope, isolate);
    let Some(context) = scope.get_slot::<v8::Global<v8::Context>>().cloned() else { return };
    let context = v8::Local::new(scope, &context);
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(tc, scope);

    f();

    if tc.has_caught() {
        if let Some(exception) = tc.exception() {
            let message = exception
                .to_string(tc)
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "<exception>".into());
            eprintln!("[NativeScript] uncaught exception in a native addon callback: {message}");
            crate::store_last_js_error(message);
        }
        tc.reset();
    }
    if !crate::defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }
}

/// Reports an exception a callback left pending on `env` (outside `in_js_scope`'s TryCatch).
unsafe fn report_pending(env: napi_env) {
    let mut pending = false;
    napi_is_exception_pending(env, &mut pending);
    if !pending {
        return;
    }
    let mut error = ptr::null_mut();
    napi_get_and_clear_last_exception(env, &mut error);
    let mut text = ptr::null_mut();
    let mut message = String::from("<exception>");
    if napi_coerce_to_string(env, error, &mut text) == NAPI_OK {
        let mut len = 0usize;
        napi_get_value_string_utf8(env, text, ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u8; len + 1];
        napi_get_value_string_utf8(env, text, buf.as_mut_ptr() as *mut c_char, len + 1, &mut len);
        buf.truncate(len);
        message = String::from_utf8_lossy(&buf).into_owned();
    }
    eprintln!("[NativeScript] uncaught exception in a native addon callback: {message}");
    crate::store_last_js_error(message);
}

struct HandleScope(napi_env, *mut c_void);

impl HandleScope {
    unsafe fn open(env: napi_env) -> HandleScope {
        let mut scope = ptr::null_mut();
        napi_open_handle_scope(env, &mut scope);
        HandleScope(env, scope)
    }
}

impl Drop for HandleScope {
    fn drop(&mut self) {
        unsafe {
            napi_close_handle_scope(self.0, self.1);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Worker pool for async work.

type Task = Box<dyn FnOnce() + Send>;

fn pool() -> &'static Mutex<Sender<Task>> {
    static POOL: OnceLock<Mutex<Sender<Task>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = channel::<Task>();
        let rx = Arc::new(Mutex::new(rx));
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4);
        for i in 0..workers {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("napi-worker-{i}"))
                .spawn(move || loop {
                    let task = match rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    match task {
                        Ok(task) => {
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                        }
                        Err(_) => return,
                    }
                });
        }
        Mutex::new(tx)
    })
}

// ---------------------------------------------------------------------------------------------
// Threadsafe functions.

struct TsfnState {
    queue: VecDeque<*mut c_void>,
    thread_count: usize,
    closing: bool,
    aborted: bool,
    finalize_queued: bool,
    finalized: bool,
}

struct Tsfn {
    env: napi_env,
    func: napi_ref,
    context: *mut c_void,
    call_js: napi_threadsafe_function_call_js,
    finalize_cb: napi_finalize,
    finalize_data: *mut c_void,
    max_queue_size: usize,
    state: Mutex<TsfnState>,
    space: Condvar,
}

// Queued data pointers and env/ref handles are only dereferenced on the JS thread.
unsafe impl Send for Tsfn {}
unsafe impl Sync for Tsfn {}

impl Tsfn {
    fn from_handle(handle: *mut c_void) -> Option<&'static Tsfn> {
        (!handle.is_null()).then(|| unsafe { &*(handle as *const Tsfn) })
    }

    unsafe fn arc(handle: *mut c_void) -> Arc<Tsfn> {
        Arc::increment_strong_count(handle as *const Tsfn);
        Arc::from_raw(handle as *const Tsfn)
    }

    /// JS thread: deliver queued calls, then finalize if the function is done.
    unsafe fn dispatch(self: &Arc<Self>) {
        loop {
            let (data, done) = {
                let mut state = self.state.lock().unwrap();
                if state.finalized {
                    return;
                }
                if state.aborted {
                    (None, true)
                } else {
                    let data = state.queue.pop_front();
                    self.space.notify_one();
                    let done = data.is_none() && state.thread_count == 0;
                    (data, done)
                }
            };
            match data {
                Some(data) => self.call(data),
                None => {
                    if done {
                        self.finalize();
                    }
                    return;
                }
            }
        }
    }

    unsafe fn call(&self, data: *mut c_void) {
        let _scope = HandleScope::open(self.env);
        let mut func = ptr::null_mut();
        if !self.func.is_null() {
            napi_get_reference_value(self.env, self.func, &mut func);
        }
        match self.call_js {
            Some(call_js) => call_js(self.env, func, self.context, data),
            None if !func.is_null() => {
                let mut recv = ptr::null_mut();
                napi_get_undefined(self.env, &mut recv);
                let mut result = ptr::null_mut();
                napi_call_function(self.env, recv, func, 0, ptr::null(), &mut result);
            }
            None => {}
        }
        report_pending(self.env);
    }

    unsafe fn finalize(self: &Arc<Self>) {
        let leftover = {
            let mut state = self.state.lock().unwrap();
            if state.finalized {
                return;
            }
            state.finalized = true;
            state.closing = true;
            self.space.notify_all();
            std::mem::take(&mut state.queue)
        };
        // Items never delivered (abort) still go to call_js, with a null env, so the addon can
        // free them -- Node's contract.
        if let Some(call_js) = self.call_js {
            for data in leftover {
                call_js(ptr::null_mut(), ptr::null_mut(), self.context, data);
            }
        }
        let _scope = HandleScope::open(self.env);
        if let Some(finalize) = self.finalize_cb {
            finalize(self.env, self.finalize_data, self.context);
            report_pending(self.env);
        }
        if !self.func.is_null() {
            napi_delete_reference(self.env, self.func);
        }
        // The handle's own reference.
        drop(Arc::from_raw(Arc::as_ptr(self)));
    }

    fn queue_finalize(self: &Arc<Self>, state: &mut TsfnState) {
        if !state.finalize_queued {
            state.finalize_queued = true;
            push(Job::Tsfn(self.clone()));
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_threadsafe_function(
    env: napi_env,
    func: napi_value,
    _async_resource: napi_value,
    _async_resource_name: napi_value,
    max_queue_size: usize,
    initial_thread_count: usize,
    thread_finalize_data: *mut c_void,
    thread_finalize_cb: napi_finalize,
    context: *mut c_void,
    call_js_cb: napi_threadsafe_function_call_js,
    result: *mut *mut c_void,
) -> napi_status {
    if env.is_null() || result.is_null() || initial_thread_count == 0 || (func.is_null() && call_js_cb.is_none()) {
        return NAPI_INVALID_ARG;
    }
    let mut reference = ptr::null_mut();
    if !func.is_null() && napi_create_reference(env, func, 1, &mut reference) != NAPI_OK {
        return NAPI_GENERIC_FAILURE;
    }
    let tsfn = Arc::new(Tsfn {
        env,
        func: reference,
        context,
        call_js: call_js_cb,
        finalize_cb: thread_finalize_cb,
        finalize_data: thread_finalize_data,
        max_queue_size,
        state: Mutex::new(TsfnState {
            queue: VecDeque::new(),
            thread_count: initial_thread_count,
            closing: false,
            aborted: false,
            finalize_queued: false,
            finalized: false,
        }),
        space: Condvar::new(),
    });
    *result = Arc::into_raw(tsfn) as *mut c_void;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_call_threadsafe_function(func: *mut c_void, data: *mut c_void, is_blocking: i32) -> napi_status {
    let Some(tsfn) = Tsfn::from_handle(func) else { return NAPI_INVALID_ARG };
    let mut state = tsfn.state.lock().unwrap();
    loop {
        if state.closing || state.aborted {
            return NAPI_CLOSING;
        }
        if tsfn.max_queue_size == 0 || state.queue.len() < tsfn.max_queue_size {
            break;
        }
        if is_blocking == 0 {
            return NAPI_QUEUE_FULL;
        }
        state = tsfn.space.wait(state).unwrap();
    }
    state.queue.push_back(data);
    drop(state);
    push(Job::Tsfn(Tsfn::arc(func)));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(func: *mut c_void) -> napi_status {
    let Some(tsfn) = Tsfn::from_handle(func) else { return NAPI_INVALID_ARG };
    let mut state = tsfn.state.lock().unwrap();
    if state.closing || state.aborted {
        return NAPI_CLOSING;
    }
    state.thread_count += 1;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_release_threadsafe_function(func: *mut c_void, mode: i32) -> napi_status {
    let Some(tsfn) = Tsfn::from_handle(func) else { return NAPI_INVALID_ARG };
    let arc = Tsfn::arc(func);
    let mut state = tsfn.state.lock().unwrap();
    if state.thread_count == 0 {
        return NAPI_INVALID_ARG;
    }
    state.thread_count -= 1;
    if mode == 1 {
        // napi_tsfn_abort
        state.closing = true;
        state.aborted = true;
        tsfn.space.notify_all();
    }
    if state.thread_count == 0 || state.aborted {
        state.closing = true;
        arc.queue_finalize(&mut state);
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_threadsafe_function_context(func: *mut c_void, result: *mut *mut c_void) -> napi_status {
    let Some(tsfn) = Tsfn::from_handle(func) else { return NAPI_INVALID_ARG };
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    *result = tsfn.context;
    NAPI_OK
}

/// The runtime's lifetime is the app's, not an event loop's, so ref/unref have nothing to keep
/// alive; they are accepted for API compatibility.
#[no_mangle]
pub unsafe extern "C" fn napi_ref_threadsafe_function(_env: napi_env, func: *mut c_void) -> napi_status {
    if func.is_null() { NAPI_INVALID_ARG } else { NAPI_OK }
}

#[no_mangle]
pub unsafe extern "C" fn napi_unref_threadsafe_function(_env: napi_env, func: *mut c_void) -> napi_status {
    if func.is_null() { NAPI_INVALID_ARG } else { NAPI_OK }
}

// ---------------------------------------------------------------------------------------------
// Async work.

const WORK_IDLE: u8 = 0;
const WORK_QUEUED: u8 = 1;
const WORK_RUNNING: u8 = 2;
const WORK_CANCELLED: u8 = 3;

struct AsyncWork {
    env: napi_env,
    execute: napi_async_execute_callback,
    complete: napi_async_complete_callback,
    data: *mut c_void,
    state: AtomicU8,
}

impl AsyncWork {
    /// JS thread.
    unsafe fn complete(work: *mut AsyncWork) {
        let work = &*work;
        let status = if work.state.swap(WORK_IDLE, Ordering::AcqRel) == WORK_CANCELLED {
            NAPI_CANCELLED
        } else {
            NAPI_OK
        };
        if let Some(complete) = work.complete {
            let _scope = HandleScope::open(work.env);
            complete(work.env, status, work.data);
            report_pending(work.env);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_async_work(
    env: napi_env,
    _async_resource: napi_value,
    _async_resource_name: napi_value,
    execute: napi_async_execute_callback,
    complete: napi_async_complete_callback,
    data: *mut c_void,
    result: *mut *mut c_void,
) -> napi_status {
    if env.is_null() || execute.is_none() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    *result = Box::into_raw(Box::new(AsyncWork {
        env,
        execute,
        complete,
        data,
        state: AtomicU8::new(WORK_IDLE),
    })) as *mut c_void;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_delete_async_work(_env: napi_env, work: *mut c_void) -> napi_status {
    if work.is_null() {
        return NAPI_INVALID_ARG;
    }
    drop(Box::from_raw(work as *mut AsyncWork));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_queue_async_work(_env: napi_env, work: *mut c_void) -> napi_status {
    if work.is_null() {
        return NAPI_INVALID_ARG;
    }
    let entry = &*(work as *const AsyncWork);
    if entry
        .state
        .compare_exchange(WORK_IDLE, WORK_QUEUED, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return NAPI_GENERIC_FAILURE;
    }
    let address = work as usize;
    let task: Task = Box::new(move || {
        let work = unsafe { &*(address as *const AsyncWork) };
        if work
            .state
            .compare_exchange(WORK_QUEUED, WORK_RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if let Some(execute) = work.execute {
                unsafe { execute(work.env, work.data) };
            }
        }
        push(Job::AsyncComplete(address));
    });
    match pool().lock() {
        Ok(tx) if tx.send(task).is_ok() => NAPI_OK,
        _ => NAPI_GENERIC_FAILURE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_cancel_async_work(_env: napi_env, work: *mut c_void) -> napi_status {
    if work.is_null() {
        return NAPI_INVALID_ARG;
    }
    let work = &*(work as *const AsyncWork);
    match work
        .state
        .compare_exchange(WORK_QUEUED, WORK_CANCELLED, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => NAPI_OK,
        Err(_) => NAPI_GENERIC_FAILURE,
    }
}

// ---------------------------------------------------------------------------------------------
// Cleanup hooks.

#[derive(Clone, Copy, PartialEq)]
enum Hook {
    Sync(unsafe extern "C" fn(*mut c_void), usize),
    Async(usize),
}

struct AsyncHook {
    hook: unsafe extern "C" fn(*mut c_void, *mut c_void),
    arg: *mut c_void,
    env: napi_env,
}

static HOOKS: Mutex<Option<HashMap<usize, Vec<Hook>>>> = Mutex::new(None);

fn with_hooks<R>(f: impl FnOnce(&mut HashMap<usize, Vec<Hook>>) -> R) -> R {
    let mut guard = HOOKS.lock().unwrap();
    f(guard.get_or_insert_with(HashMap::new))
}

#[no_mangle]
pub unsafe extern "C" fn napi_add_env_cleanup_hook(env: napi_env, fun: napi_cleanup_hook, arg: *mut c_void) -> napi_status {
    let (Some(fun), false) = (fun, env.is_null()) else { return NAPI_INVALID_ARG };
    with_hooks(|hooks| hooks.entry(env as usize).or_default().push(Hook::Sync(fun, arg as usize)));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_remove_env_cleanup_hook(env: napi_env, fun: napi_cleanup_hook, arg: *mut c_void) -> napi_status {
    let (Some(fun), false) = (fun, env.is_null()) else { return NAPI_INVALID_ARG };
    with_hooks(|hooks| {
        if let Some(list) = hooks.get_mut(&(env as usize)) {
            if let Some(i) = list.iter().rposition(|h| *h == Hook::Sync(fun, arg as usize)) {
                list.remove(i);
            }
        }
    });
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_add_async_cleanup_hook(
    env: napi_env,
    hook: napi_async_cleanup_hook,
    arg: *mut c_void,
    remove_handle: *mut *mut c_void,
) -> napi_status {
    let (Some(hook), false) = (hook, env.is_null()) else { return NAPI_INVALID_ARG };
    let handle = Box::into_raw(Box::new(AsyncHook { hook, arg, env }));
    with_hooks(|hooks| hooks.entry(env as usize).or_default().push(Hook::Async(handle as usize)));
    if !remove_handle.is_null() {
        *remove_handle = handle as *mut c_void;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_remove_async_cleanup_hook(remove_handle: *mut c_void) -> napi_status {
    if remove_handle.is_null() {
        return NAPI_INVALID_ARG;
    }
    let handle = Box::from_raw(remove_handle as *mut AsyncHook);
    with_hooks(|hooks| {
        if let Some(list) = hooks.get_mut(&(handle.env as usize)) {
            list.retain(|h| *h != Hook::Async(remove_handle as usize));
        }
    });
    NAPI_OK
}

fn run_cleanup_hooks(env: napi_env) {
    let hooks = with_hooks(|hooks| hooks.remove(&(env as usize))).unwrap_or_default();
    for hook in hooks.into_iter().rev() {
        match hook {
            Hook::Sync(fun, arg) => unsafe { fun(arg as *mut c_void) },
            // The hook calls napi_remove_async_cleanup_hook (freeing the handle) when it is done.
            Hook::Async(handle) => unsafe {
                let entry = &*(handle as *const AsyncHook);
                (entry.hook)(handle as *mut c_void, entry.arg)
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Buffers (Node's Buffer is a Uint8Array).

#[no_mangle]
pub unsafe extern "C" fn napi_create_buffer(env: napi_env, size: usize, data: *mut *mut c_void, result: *mut napi_value) -> napi_status {
    let mut buffer = ptr::null_mut();
    let status = napi_create_arraybuffer(env, size, data, &mut buffer);
    if status != NAPI_OK {
        return status;
    }
    napi_create_typedarray(env, UINT8_ARRAY, size, buffer, 0, result)
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_buffer_copy(
    env: napi_env,
    length: usize,
    data: *const c_void,
    result_data: *mut *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    let mut out = ptr::null_mut();
    let status = napi_create_buffer(env, length, &mut out, result);
    if status != NAPI_OK {
        return status;
    }
    if length > 0 && !data.is_null() {
        ptr::copy_nonoverlapping(data as *const u8, out as *mut u8, length);
    }
    if !result_data.is_null() {
        *result_data = out;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_external_buffer(
    env: napi_env,
    length: usize,
    data: *mut c_void,
    finalize_cb: napi_finalize,
    finalize_hint: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    let mut buffer = ptr::null_mut();
    let status = napi_create_external_arraybuffer(env, data, length, finalize_cb, finalize_hint, &mut buffer);
    if status != NAPI_OK {
        return status;
    }
    napi_create_typedarray(env, UINT8_ARRAY, length, buffer, 0, result)
}

#[no_mangle]
pub unsafe extern "C" fn node_api_create_buffer_from_arraybuffer(
    env: napi_env,
    arraybuffer: napi_value,
    byte_offset: usize,
    byte_length: usize,
    result: *mut napi_value,
) -> napi_status {
    napi_create_typedarray(env, UINT8_ARRAY, byte_length, arraybuffer, byte_offset, result)
}

fn element_size(kind: i32) -> usize {
    match kind {
        0..=2 => 1,
        3 | 4 => 2,
        5..=7 => 4,
        _ => 8,
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_buffer_info(env: napi_env, value: napi_value, data: *mut *mut c_void, length: *mut usize) -> napi_status {
    let mut is_typed = false;
    napi_is_typedarray(env, value, &mut is_typed);
    let (mut out_data, mut out_len) = (ptr::null_mut(), 0usize);
    let mut arraybuffer = ptr::null_mut();
    let mut offset = 0usize;
    if is_typed {
        let mut kind = 0;
        let status = napi_get_typedarray_info(env, value, &mut kind, &mut out_len, &mut out_data, &mut arraybuffer, &mut offset);
        if status != NAPI_OK {
            return status;
        }
        out_len *= element_size(kind);
    } else {
        let status = napi_get_dataview_info(env, value, &mut out_len, &mut out_data, &mut arraybuffer, &mut offset);
        if status != NAPI_OK {
            return status;
        }
    }
    if !data.is_null() {
        *data = out_data;
    }
    if !length.is_null() {
        *length = out_len;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_is_buffer(env: napi_env, value: napi_value, result: *mut bool) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let mut typed = false;
    let mut view = false;
    napi_is_typedarray(env, value, &mut typed);
    if !typed {
        napi_is_dataview(env, value, &mut view);
    }
    *result = typed || view;
    NAPI_OK
}

// ---------------------------------------------------------------------------------------------
// Callbacks, async context, versions, errors, legacy registration.

fn microtask_checkpoint() {
    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() || crate::defer_microtask_drain() {
        return;
    }
    let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
    isolate.perform_microtask_checkpoint();
}

#[no_mangle]
pub unsafe extern "C" fn napi_make_callback(
    env: napi_env,
    _async_context: *mut c_void,
    recv: napi_value,
    func: napi_value,
    argc: usize,
    argv: *const napi_value,
    result: *mut napi_value,
) -> napi_status {
    let mut ignored = ptr::null_mut();
    let result = if result.is_null() { &mut ignored } else { result };
    let status = napi_call_function(env, recv, func, argc, argv, result);
    if CALLBACK_DEPTH.with(|d| d.get()) == 0 {
        microtask_checkpoint();
    }
    status
}

#[no_mangle]
pub unsafe extern "C" fn napi_open_callback_scope(
    _env: napi_env,
    _resource: napi_value,
    _context: *mut c_void,
    result: *mut *mut c_void,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let depth = CALLBACK_DEPTH.with(|d| {
        d.set(d.get() + 1);
        d.get()
    });
    *result = depth as usize as *mut c_void;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_close_callback_scope(_env: napi_env, scope: *mut c_void) -> napi_status {
    if scope.is_null() {
        return NAPI_INVALID_ARG;
    }
    let depth = CALLBACK_DEPTH.with(|d| {
        d.set(d.get().saturating_sub(1));
        d.get()
    });
    if depth == 0 {
        microtask_checkpoint();
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_async_init(
    _env: napi_env,
    _async_resource: napi_value,
    _async_resource_name: napi_value,
    result: *mut *mut c_void,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // No async_hooks: the context is an opaque non-null token.
    *result = ptr::NonNull::<u8>::dangling().as_ptr() as *mut c_void;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_async_destroy(_env: napi_env, _async_context: *mut c_void) -> napi_status {
    NAPI_OK
}

#[repr(C)]
pub struct NapiNodeVersion {
    major: u32,
    minor: u32,
    patch: u32,
    release: *const c_char,
}

unsafe impl Sync for NapiNodeVersion {}

/// Addons use this for feature checks; report the Node-API level of a current Node LTS.
static NODE_VERSION: NapiNodeVersion = NapiNodeVersion {
    major: 22,
    minor: 0,
    patch: 0,
    release: c"nativescript".as_ptr(),
};

#[no_mangle]
pub unsafe extern "C" fn napi_get_node_version(_env: napi_env, result: *mut *const NapiNodeVersion) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    *result = &NODE_VERSION;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_uv_event_loop(_env: napi_env, result: *mut *mut c_void) -> napi_status {
    if !result.is_null() {
        *result = ptr::null_mut();
    }
    NAPI_GENERIC_FAILURE
}

unsafe fn lossy(text: *const c_char, len: usize) -> String {
    if text.is_null() {
        return String::new();
    }
    let bytes = if len == usize::MAX {
        std::ffi::CStr::from_ptr(text).to_bytes()
    } else {
        std::slice::from_raw_parts(text as *const u8, len)
    };
    String::from_utf8_lossy(bytes).into_owned()
}

#[no_mangle]
pub unsafe extern "C" fn napi_fatal_error(location: *const c_char, location_len: usize, message: *const c_char, message_len: usize) -> ! {
    let report = format!(
        "FATAL ERROR: {} {}",
        lossy(location, location_len),
        lossy(message, message_len)
    );
    eprintln!("[NativeScript] {report}");
    crate::store_last_js_error(report);
    std::process::abort();
}

#[no_mangle]
pub unsafe extern "C" fn napi_fatal_exception(env: napi_env, error: napi_value) -> napi_status {
    let mut text = ptr::null_mut();
    let mut message = String::from("<exception>");
    if napi_coerce_to_string(env, error, &mut text) == NAPI_OK {
        let mut len = 0usize;
        napi_get_value_string_utf8(env, text, ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u8; len + 1];
        napi_get_value_string_utf8(env, text, buf.as_mut_ptr() as *mut c_char, len + 1, &mut len);
        buf.truncate(len);
        message = String::from_utf8_lossy(&buf).into_owned();
    }
    eprintln!("[NativeScript] uncaught exception from a native addon: {message}");
    crate::store_last_js_error(message);
    NAPI_OK
}

/// `NAPI_MODULE` registration from a static constructor, as older addons do. The loader reads it
/// right after `LoadLibrary` returns.
#[repr(C)]
pub struct NapiModule {
    pub nm_version: i32,
    pub nm_flags: u32,
    pub nm_filename: *const c_char,
    pub nm_register_func: Option<napi_v8_shim::AddonInit>,
    pub nm_modname: *const c_char,
    pub nm_priv: *mut c_void,
    pub reserved: [*mut c_void; 4],
}

static LEGACY_MODULE: Mutex<usize> = Mutex::new(0);

#[no_mangle]
pub unsafe extern "C" fn napi_module_register(module: *mut NapiModule) {
    *LEGACY_MODULE.lock().unwrap() = module as usize;
}

pub(crate) fn take_legacy_module() -> Option<napi_v8_shim::AddonInit> {
    let module = std::mem::take(&mut *LEGACY_MODULE.lock().unwrap());
    (module != 0).then(|| unsafe { (*(module as *const NapiModule)).nm_register_func }).flatten()
}

thread_local! {
    static MODULE_FILE_NAMES: RefCell<HashMap<usize, std::ffi::CString>> = RefCell::new(HashMap::new());
}

pub(crate) fn set_module_file_name(env: napi_env, url: &str) {
    if let Ok(name) = std::ffi::CString::new(url) {
        MODULE_FILE_NAMES.with(|m| m.borrow_mut().insert(env as usize, name));
    }
}

#[no_mangle]
pub unsafe extern "C" fn node_api_get_module_file_name(env: napi_env, result: *mut *const c_char) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    *result = MODULE_FILE_NAMES.with(|m| m.borrow().get(&(env as usize)).map_or(ptr::null(), |s| s.as_ptr()));
    if (*result).is_null() { NAPI_GENERIC_FAILURE } else { NAPI_OK }
}
