use crate::Runtime;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::UI::WindowsAndMessaging::{MsgWaitForMultipleObjectsEx, MWMO_INPUTAVAILABLE, QS_ALLINPUT};

enum WorkerCommand {
    PostMessage(Vec<u8>),
    /// Work handed to the worker's thread from another thread: a callback created in the worker
    /// (see [`run_on_worker_sync`]) or the release of one.
    Run(Box<dyn FnOnce() + Send>),
    Terminate,
}

/// An auto-reset event that wakes a worker's loop; the loop also wakes for Windows messages, which
/// a worker (an STA thread) needs pumped for WinRT to deliver marshaled calls and completions.
#[derive(Clone, Copy)]
struct WakeEvent(isize);

impl WakeEvent {
    fn new() -> Self {
        let handle = unsafe { CreateEventW(None, false, false, None) }.unwrap_or_default();
        WakeEvent(handle.0 as isize)
    }

    fn handle(self) -> HANDLE {
        HANDLE(self.0 as *mut std::ffi::c_void)
    }

    fn signal(self) {
        let _ = unsafe { SetEvent(self.handle()) };
    }
}

/// The command channel of each worker's runtime, keyed by its isolate, so a callback created in a
/// worker can be handed to that worker's thread.
static WORKER_EXECUTORS: OnceLock<Mutex<HashMap<usize, (Sender<WorkerCommand>, WakeEvent)>>> = OnceLock::new();

fn worker_executors() -> &'static Mutex<HashMap<usize, (Sender<WorkerCommand>, WakeEvent)>> {
    WORKER_EXECUTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `isolate` is a worker's (as opposed to the UI thread's).
pub(crate) fn is_worker_isolate(isolate: usize) -> bool {
    worker_executors().lock().contains_key(&isolate)
}

/// Runs `f` on the thread of the worker that owns `isolate` and waits for its result, the way
/// [`crate::ui_dispatcher::run_on_ui_thread_sync`] does for the UI thread. `None` when the worker
/// is gone.
pub(crate) fn run_on_worker_sync<R: Send>(isolate: usize, f: impl FnOnce() -> R + Send) -> Option<R> {
    let (tx, wake) = worker_executors().lock().get(&isolate).cloned()?;
    crate::ui_dispatcher::run_job_sync(f, |job| {
        let sent = tx.send(WorkerCommand::Run(job)).is_ok();
        wake.signal();
        sent
    })
}

/// Queues `f` on the thread of the worker that owns `isolate` without waiting. `false` when the
/// worker is gone.
pub(crate) fn post_to_worker(isolate: usize, f: impl FnOnce() + Send + 'static) -> bool {
    let Some((tx, wake)) = worker_executors().lock().get(&isolate).cloned() else {
        return false;
    };
    let sent = tx.send(WorkerCommand::Run(Box::new(f))).is_ok();
    wake.signal();
    sent
}

#[derive(Debug)]
enum WorkerEvent {
    Message(Vec<u8>),
    Error(String),
    Exited,
}

struct WorkerHandle {
    tx: Sender<WorkerCommand>,
    wake: WakeEvent,
    /// Wrapped in Arc<Mutex> so the WORKERS registry lock can be released before
    /// a blocking recv — otherwise poll_events_blocking would hold the global
    /// registry lock for the entire timeout duration, starving create/terminate.
    rx: Arc<Mutex<Receiver<WorkerEvent>>>,
    join: thread::JoinHandle<()>,
}

#[derive(Debug)]
pub enum PolledWorkerEvent {
    Message(Vec<u8>),
    Error(String),
    Exited,
}

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);
static WORKERS: OnceLock<RwLock<HashMap<u64, WorkerHandle>>> = OnceLock::new();

fn workers() -> &'static RwLock<HashMap<u64, WorkerHandle>> {
    WORKERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn worker_bootstrap_script(source: &str, filename: &str) -> Result<String, String> {
    let source_json = serde_json::to_string(source)
        .map_err(|e| format!("Failed to serialize worker source: {e}"))?;
    let filename_json = serde_json::to_string(filename)
        .map_err(|e| format!("Failed to serialize worker filename: {e}"))?;

    Ok(format!(
        r#"
            (function () {{
                const __workerSource = {source};
                const __workerFilename = {filename};
                const __listeners = [];

                globalThis.__nsWorkerOutbox = [];
                globalThis.self = globalThis;
                globalThis.postMessage = function (data) {{
                    globalThis.__nsWorkerOutbox.push(data);
                }};

                globalThis.addEventListener = function (type, listener) {{
                    if (type !== 'message' || typeof listener !== 'function') {{
                        return;
                    }}
                    if (__listeners.indexOf(listener) < 0) {{
                        __listeners.push(listener);
                    }}
                }};

                globalThis.removeEventListener = function (type, listener) {{
                    if (type !== 'message' || typeof listener !== 'function') {{
                        return;
                    }}
                    const index = __listeners.indexOf(listener);
                    if (index >= 0) {{
                        __listeners.splice(index, 1);
                    }}
                }};

                globalThis.__nsDispatchToWorker = function (data) {{
                    const event = {{
                        type: 'message',
                        data: data,
                        target: globalThis,
                        currentTarget: globalThis,
                        ports: []
                    }};

                    if (typeof globalThis.onmessage === 'function') {{
                        globalThis.onmessage(event);
                    }}

                    __listeners.slice().forEach(function (listener) {{
                        listener.call(globalThis, event);
                    }});
                }};

                if (typeof globalThis.__nsEvalAsModule === 'function') {{
                    globalThis.__nsEvalAsModule(__workerSource, __workerFilename || '[worker]');
                }} else {{
                    const exec = new Function('__filename', __workerSource);
                    exec(__workerFilename || '[worker]');
                }}
            }})();
        "#,
        source = source_json,
        filename = filename_json
    ))
}

pub fn create_worker(app_root: String, source: String, filename: String) -> Result<u64, String> {
    let worker_id = NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed);

    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
    let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
    let evt_rx = Arc::new(Mutex::new(evt_rx));
    let wake = WakeEvent::new();
    let executor_tx = cmd_tx.clone();
    // Events reach the creating thread's `Worker` without it having to poll: each batch queues a
    // delivery there (one at a time).
    let creator = crate::DELEGATE_ISOLATE_PTR.with(|c| c.get()) as usize;
    let delivery_pending = Arc::new(AtomicBool::new(false));
    let notify_creator = move || {
        if delivery_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = delivery_pending.clone();
        let queued = crate::post_to_js_thread(creator as *mut v8::Isolate, move || {
            pending.store(false, Ordering::Release);
            crate::global_fns::deliver_worker_events(worker_id);
        });
        if !queued {
            delivery_pending.store(false, Ordering::Release);
        }
    };

    let join = thread::Builder::new()
        .name(format!("ns-worker-{worker_id}"))
        .spawn(move || {
            let mut runtime = Runtime::new(app_root.as_str());
            // `runtime` stays put on this thread's stack from here on.
            runtime.register_delegate_isolate_ptr();
            let isolate = crate::DELEGATE_ISOLATE_PTR.with(|c| c.get()) as usize;
            worker_executors().lock().insert(isolate, (executor_tx, wake));

            match worker_bootstrap_script(source.as_str(), filename.as_str()) {
                Ok(script) => runtime.run_script(script.as_str(), filename.as_str()),
                Err(err) => {
                    worker_executors().lock().remove(&isolate);
                    let _ = evt_tx.send(WorkerEvent::Error(err));
                    let _ = evt_tx.send(WorkerEvent::Exited);
                    notify_creator();
                    return;
                }
            }

            let forward_outbox = |runtime: &mut Runtime| {
                let mut sent = false;
                for result in runtime.drain_outbox_bytes() {
                    sent = true;
                    let _ = evt_tx.send(match result {
                        Ok(b) => WorkerEvent::Message(b),
                        Err(e) => WorkerEvent::Error(e),
                    });
                }
                if sent {
                    notify_creator();
                }
            };
            forward_outbox(&mut runtime);
            // Commands, Windows messages (WinRT calls marshaled to this STA thread, async
            // completions) and the worker's timers, until terminated.
            'run: loop {
                loop {
                    match cmd_rx.try_recv() {
                        Ok(WorkerCommand::PostMessage(bytes)) => runtime.dispatch_to_worker(&bytes),
                        Ok(WorkerCommand::Run(job)) => job(),
                        Ok(WorkerCommand::Terminate) | Err(TryRecvError::Disconnected) => break 'run,
                        Err(TryRecvError::Empty) => break,
                    }
                    forward_outbox(&mut runtime);
                }
                crate::pump_messages();
                crate::timers::pump();
                forward_outbox(&mut runtime);
                let timeout = if crate::timers::has_pending() { 10 } else { INFINITE };
                unsafe {
                    MsgWaitForMultipleObjectsEx(Some(&[wake.handle()]), timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }

            worker_executors().lock().remove(&isolate);
            let _ = evt_tx.send(WorkerEvent::Exited);
            notify_creator();
        })
        .map_err(|e| format!("Failed to spawn worker thread: {e}"))?;

    workers().write().insert(
        worker_id,
        WorkerHandle {
            tx: cmd_tx,
            wake,
            rx: evt_rx,
            join,
        },
    );

    Ok(worker_id)
}

pub fn post_message(worker_id: u64, payload_bytes: Vec<u8>) -> Result<(), String> {
    let workers = workers().read();
    let Some(worker) = workers.get(&worker_id) else {
        return Err(format!("Unknown worker id: {worker_id}"));
    };

    let sent = worker
        .tx
        .send(WorkerCommand::PostMessage(payload_bytes))
        .map_err(|e| format!("Failed to send worker message: {e}"));
    worker.wake.signal();
    sent
}

fn collect_events(rx: &Receiver<WorkerEvent>) -> Vec<PolledWorkerEvent> {
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(WorkerEvent::Message(bytes)) => events.push(PolledWorkerEvent::Message(bytes)),
            Ok(WorkerEvent::Error(err)) => events.push(PolledWorkerEvent::Error(err)),
            Ok(WorkerEvent::Exited) => {
                events.push(PolledWorkerEvent::Exited);
                break;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                events.push(PolledWorkerEvent::Exited);
                break;
            }
        }
    }
    events
}

pub fn poll_events(worker_id: u64) -> Result<Vec<PolledWorkerEvent>, String> {
    // Clone the Arc so we can release the registry lock before draining.
    let rx = {
        let workers = workers().read();
        let Some(worker) = workers.get(&worker_id) else {
            return Err(format!("Unknown worker id: {worker_id}"));
        };
        Arc::clone(&worker.rx)
    };

    let guard = rx.lock();
    Ok(collect_events(&guard))
}

pub fn poll_events_blocking(
    worker_id: u64,
    timeout_ms: u64,
) -> Result<Vec<PolledWorkerEvent>, String> {
    // Clone the Arc and immediately release the registry lock so that
    // create_worker / terminate_worker are not starved for the full timeout.
    let rx = {
        let workers = workers().read();
        let Some(worker) = workers.get(&worker_id) else {
            return Err(format!("Unknown worker id: {worker_id}"));
        };
        Arc::clone(&worker.rx)
    };

    let rx = rx.lock();
    let mut events = Vec::new();

    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(WorkerEvent::Message(bytes)) => events.push(PolledWorkerEvent::Message(bytes)),
        Ok(WorkerEvent::Error(err)) => events.push(PolledWorkerEvent::Error(err)),
        Ok(WorkerEvent::Exited) => events.push(PolledWorkerEvent::Exited),
        Err(_) => return Ok(events),
    }

    // Drain any additional events that arrived without blocking.
    events.extend(collect_events(&rx));

    Ok(events)
}

pub fn terminate_worker(worker_id: u64) -> Result<(), String> {
    let mut workers_guard = workers().write();
    let Some(worker) = workers_guard.remove(&worker_id) else {
        return Err(format!("Unknown worker id: {worker_id}"));
    };
    drop(workers_guard);

    let _ = worker.tx.send(WorkerCommand::Terminate);
    worker.wake.signal();
    let _ = worker.join.join();

    Ok(())
}
