//! Native `WebSocket` (the HELPER_SOURCE class wraps these natives). The iOS/Android runtimes get
//! theirs from `@valor/nativescript-websockets`, which has no Windows implementation; the Vite HMR
//! client needs one to receive updates.
//!
//! Each socket's thread drives WinHTTP in async mode: the handshake, one pending receive, and one
//! send at a time from a queue. `send` and `close` only queue, so JS never waits on the network,
//! and closing a stuck socket's handles cancels its operations. Events go onto the owning JS
//! thread's channel and are delivered by `pump()`, which runs with the timer pump (the host drives
//! it from its render/heartbeat loop), so JS is only ever called on the JS thread.
//!
//! The socket threads and event channel (`connect` / `send` / `shutdown` / `take_events`) are
//! engine-neutral; the napi engines bind the same natives in `napi_engine::websocket`.

#[cfg(feature = "classic")]
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Networking::WinHttp::{
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetStatusCallback,
    WinHttpWebSocketCompleteUpgrade, WinHttpWebSocketQueryCloseStatus, WinHttpWebSocketReceive,
    WinHttpWebSocketSend, WinHttpWebSocketShutdown, WINHTTP_ASYNC_RESULT,
    WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS, WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING,
    WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE, WINHTTP_CALLBACK_STATUS_READ_COMPLETE,
    WINHTTP_CALLBACK_STATUS_REQUEST_ERROR, WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE,
    WINHTTP_CALLBACK_STATUS_SHUTDOWN_COMPLETE, WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE,
    WINHTTP_FLAG_ASYNC, WINHTTP_OPTION_CONTEXT_VALUE, WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE, WINHTTP_WEB_SOCKET_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE, WINHTTP_WEB_SOCKET_STATUS,
    WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE, WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE,
};

use crate::winhttp::Handle;
#[cfg(feature = "classic")]
use crate::DELEGATE_ISOLATE_PTR;

pub(crate) enum Event {
    Open(String),
    Text(String),
    Binary(Vec<u8>),
    Error(String),
    Close { code: u16, reason: String, clean: bool },
}

/// How long close() waits for the server's close frame before closing with 1006.
const CLOSE_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(2) } else { Duration::from_secs(20) };

/// After the close event, how long queued frames (our close reply) get to go out.
const LINGER: Duration = Duration::from_secs(1);

/// How long a closed handle gets to report HANDLE_CLOSING; past that its buffers are leaked.
const HANDLE_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Largest message a socket buffers (the `ws` package's default `maxPayload`).
const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;

const ERROR_IO_PENDING: u32 = 997;

/// What a socket's thread waits for: commands from JS and WinHTTP notifications, on one channel.
enum Msg {
    Send(Vec<u8>, bool),
    Close(u16, String),
    /// The JS side is gone: close (1001) with no one to tell.
    Detach,
    SendRequestComplete,
    HeadersAvailable,
    ReadComplete(u32, WINHTTP_WEB_SOCKET_BUFFER_TYPE),
    WriteComplete,
    ShutdownComplete,
    /// REQUEST_ERROR: an async operation failed (Win32 error code).
    Failed(u32),
    /// The last notification for a handle.
    HandleClosing(usize),
}

struct Entry {
    commands: Sender<Msg>,
    /// Bytes queued by `send` that WinHTTP hasn't written yet.
    buffered: Arc<AtomicU64>,
}

fn sockets() -> &'static Mutex<HashMap<i32, Entry>> {
    static SOCKETS: OnceLock<Mutex<HashMap<i32, Entry>>> = OnceLock::new();
    SOCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicI32 = AtomicI32::new(1);

thread_local! {
    #[cfg(feature = "classic")]
    static CALLBACKS: RefCell<HashMap<i32, v8::Global<v8::Function>>> = RefCell::new(HashMap::new());
    static CHANNEL: (Sender<(i32, Event)>, Receiver<(i32, Event)>) = mpsc::channel();
}

/// Called from `Runtime::drop` while the isolate is alive.
#[cfg(feature = "classic")]
pub(crate) fn clear_thread_sockets() {
    let ids: Vec<i32> = CALLBACKS.with(|c| c.borrow_mut().drain().map(|(id, _)| id).collect());
    drop_sockets(&ids);
}

/// Close (1001, going away) and forget sockets whose JS side is gone.
#[cfg(feature = "classic")]
pub(crate) fn drop_sockets(ids: &[i32]) {
    if let Ok(mut map) = sockets().lock() {
        for id in ids {
            if let Some(entry) = map.remove(id) {
                let _ = entry.commands.send(Msg::Detach);
            }
        }
    }
}

/// Start connecting `url` on its own thread. Its events arrive, tagged with the returned id,
/// through `take_events` on this (the JS) thread.
pub(crate) fn connect(url: String, protocols: Vec<String>) -> i32 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let events = CHANNEL.with(|(tx, _)| tx.clone());
    let (commands, rx) = mpsc::channel();
    let buffered = Arc::new(AtomicU64::new(0));
    let context = Arc::new(Context { tx: commands.clone() });
    if let Ok(mut map) = sockets().lock() {
        map.insert(id, Entry { commands, buffered: buffered.clone() });
    }
    let thread_events = events.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("ns-websocket-{id}"))
        .spawn(move || run_socket(id, url, protocols, rx, context, buffered, thread_events));
    if let Err(e) = spawned {
        let _ = events.send((id, Event::Error(format!("WebSocket: {e}"))));
        let _ = events.send((id, Event::Close { code: 1006, reason: String::new(), clean: false }));
    }
    id
}

/// Queues a message; false for an unknown socket. Dropped once the socket's thread has finished.
pub(crate) fn send(id: i32, bytes: Vec<u8>, binary: bool) -> bool {
    let Ok(map) = sockets().lock() else {
        return false;
    };
    let Some(entry) = map.get(&id) else {
        return false;
    };
    entry.buffered.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    let _ = entry.commands.send(Msg::Send(bytes, binary));
    true
}

/// Queues the close frame; the close is 1006 if the server doesn't answer within `CLOSE_TIMEOUT`.
pub(crate) fn shutdown(id: i32, code: u16, reason: &str) {
    if let Ok(map) = sockets().lock() {
        if let Some(entry) = map.get(&id) {
            let _ = entry.commands.send(Msg::Close(code, reason.to_string()));
        }
    }
}

/// `bufferedAmount`: bytes queued by `send` that haven't been written to the network.
pub(crate) fn buffered_amount(id: i32) -> u64 {
    sockets()
        .lock()
        .ok()
        .and_then(|m| m.get(&id).map(|e| e.buffered.load(Ordering::Relaxed)))
        .unwrap_or(0)
}

/// Drop the JS side of a socket once its close event was delivered.
pub(crate) fn forget(id: i32) {
    if let Ok(mut map) = sockets().lock() {
        map.remove(&id);
    }
}

/// Socket events queued since the last call, in arrival order.
pub(crate) fn take_events() -> Vec<(i32, Event)> {
    CHANNEL.with(|(_, rx)| rx.try_iter().collect())
}

/// WinHTTP's callback context: one strong count per handle that carries it, released by that
/// handle's HANDLE_CLOSING, the last notification for it.
struct Context {
    tx: Sender<Msg>,
}

fn context_value(context: &Arc<Context>) -> usize {
    Arc::into_raw(context.clone()) as usize
}

unsafe fn release_context(value: usize) {
    drop(Arc::from_raw(value as *const Context));
}

/// WinHTTP status callback (any thread, possibly inside the call that started the operation):
/// forwards completions to the socket's thread.
unsafe extern "system" fn on_status(
    handle: *mut c_void,
    context: usize,
    status: u32,
    info: *mut c_void,
    _info_len: u32,
) {
    if context == 0 {
        return;
    }
    let ctx = &*(context as *const Context);
    let msg = match status {
        WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE => Msg::SendRequestComplete,
        WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => Msg::HeadersAvailable,
        WINHTTP_CALLBACK_STATUS_READ_COMPLETE if !info.is_null() => {
            let s = *(info as *const WINHTTP_WEB_SOCKET_STATUS);
            Msg::ReadComplete(s.dwBytesTransferred, s.eBufferType)
        }
        WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE => Msg::WriteComplete,
        WINHTTP_CALLBACK_STATUS_SHUTDOWN_COMPLETE => Msg::ShutdownComplete,
        // A WINHTTP_WEB_SOCKET_ASYNC_RESULT for socket operations; it starts with this.
        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR if !info.is_null() => {
            Msg::Failed((*(info as *const WINHTTP_ASYNC_RESULT)).dwError)
        }
        WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING => {
            let _ = ctx.tx.send(Msg::HandleClosing(handle as usize));
            release_context(context);
            return;
        }
        _ => return,
    };
    let _ = ctx.tx.send(msg);
}

/// The async session all sockets share.
fn session() -> Result<&'static Handle, String> {
    static SESSION: OnceLock<Result<Handle, String>> = OnceLock::new();
    SESSION
        .get_or_init(|| {
            let session = crate::winhttp::new_session(WINHTTP_FLAG_ASYNC)?;
            unsafe {
                WinHttpSetStatusCallback(
                    session.raw(),
                    Some(on_status),
                    WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS,
                    0,
                );
            }
            Ok(session)
        })
        .as_ref()
        .map_err(|e| e.clone())
}

fn error_text(err: u32) -> String {
    let known = match err {
        12002 => "The operation timed out",
        12007 => "The server name could not be resolved",
        12017 => "The operation was cancelled",
        12029 => "A connection with the server could not be established",
        12030 => "The connection with the server was terminated abnormally",
        12152 => "The server returned an invalid or unrecognized response",
        12175 => "A security error occurred",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let message = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(err)).message();
    if message.is_empty() {
        format!("WebSocket error {err}")
    } else {
        message
    }
}

/// Reports a socket's events to JS: exactly one close event, even if the thread ends early.
struct Report {
    id: i32,
    events: Sender<(i32, Event)>,
    done: bool,
}

impl Report {
    fn send(&self, event: Event) {
        let _ = self.events.send((self.id, event));
    }

    /// The close event, after an error event when `error` is set. Only the first call counts.
    fn close(&mut self, error: Option<String>, code: u16, reason: String, clean: bool) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        if let Some(error) = error {
            self.send(Event::Error(error));
        }
        self.send(Event::Close { code, reason, clean });
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        self.close(Some("WebSocket connection failed".into()), 1006, String::new(), false);
    }
}

/// An outgoing frame, queued until WinHTTP has finished the one before it.
enum Out {
    Frame(Vec<u8>, bool),
    Shutdown(u16, String),
}

/// One connection's WinHTTP state, owned by its thread. Buffers WinHTTP may still read or write
/// live here until the handles using them report HANDLE_CLOSING.
struct Conn {
    rx: Receiver<Msg>,
    context: Arc<Context>,
    connect: Option<Handle>,
    request: Option<Handle>,
    ws: Option<Handle>,
    /// Handles carrying our context that haven't reported HANDLE_CLOSING.
    live: Vec<usize>,
    headers: Vec<u16>,
    recv: Vec<u8>,
    /// The frame WinHTTP is sending.
    sending: Option<Vec<u8>>,
    shutting_down: bool,
    reason: Vec<u8>,
}

impl Conn {
    /// The next command or notification; None once `deadline` passes.
    fn next(&mut self, deadline: Option<Instant>) -> Option<Msg> {
        loop {
            let msg = match deadline {
                Some(d) => self.rx.recv_timeout(d.saturating_duration_since(Instant::now())).ok()?,
                // never disconnected: `context` holds a sender
                None => self.rx.recv().ok()?,
            };
            if let Msg::HandleClosing(h) = msg {
                self.live.retain(|x| *x != h);
                continue;
            }
            return Some(msg);
        }
    }

    fn ws(&self) -> *mut c_void {
        self.ws.as_ref().map_or(std::ptr::null_mut(), |h| h.raw())
    }

    /// Waits for `want` during the handshake. Err(None): close() or detach came first.
    fn wait(&mut self, want: fn(&Msg) -> bool) -> Result<(), Option<String>> {
        loop {
            match self.next(None) {
                Some(m) if want(&m) => return Ok(()),
                Some(Msg::Failed(e)) => return Err(Some(error_text(e))),
                Some(Msg::Close(..)) => {
                    return Err(Some("WebSocket is closed before the connection is established.".into()))
                }
                Some(Msg::Detach) | None => return Err(None),
                Some(_) => {}
            }
        }
    }

    /// Upgrade handshake; returns the subprotocol the server selected.
    fn handshake(&mut self, url: &str, protocols: &[String]) -> Result<String, Option<String>> {
        let (connect, request, headers) =
            crate::winhttp::websocket_request(session().map_err(Some)?, url, protocols).map_err(Some)?;
        self.connect = Some(connect);
        self.headers = headers;
        let value = context_value(&self.context);
        let set = unsafe {
            WinHttpSetOption(Some(request.raw()), WINHTTP_OPTION_CONTEXT_VALUE, Some(&value.to_ne_bytes()))
        };
        if let Err(e) = set {
            unsafe { release_context(value) };
            return Err(Some(format!("WebSocket: {}", e.message())));
        }
        let raw = request.raw();
        self.live.push(raw as usize);
        self.request = Some(request);
        let headers = if self.headers.is_empty() { None } else { Some(self.headers.as_slice()) };
        unsafe { WinHttpSendRequest(raw, headers, None, 0, 0, value) }
            .map_err(|e| Some(format!("WebSocket connection failed: {}", e.message())))?;
        self.wait(|m| matches!(m, Msg::SendRequestComplete))?;
        unsafe { WinHttpReceiveResponse(raw, std::ptr::null_mut()) }
            .map_err(|e| Some(format!("WebSocket connection failed: {}", e.message())))?;
        self.wait(|m| matches!(m, Msg::HeadersAvailable))?;
        let (status, protocol) = crate::winhttp::websocket_response(self.request.as_ref().unwrap());
        if status != 101 {
            return Err(Some(format!("WebSocket handshake failed: HTTP {status}")));
        }
        let value = context_value(&self.context);
        let ws = unsafe { WinHttpWebSocketCompleteUpgrade(raw, Some(value)) };
        if ws.is_null() {
            let e = windows::core::Error::from_thread().message();
            unsafe { release_context(value) };
            return Err(Some(format!("WinHttpWebSocketCompleteUpgrade failed: {e}")));
        }
        self.live.push(ws as usize);
        self.ws = Some(Handle::new(ws, "WinHttpWebSocketCompleteUpgrade").map_err(Some)?);
        self.request = None;
        Ok(protocol)
    }

    fn receive(&mut self) -> Result<(), String> {
        let err = unsafe {
            WinHttpWebSocketReceive(
                self.ws(),
                self.recv.as_mut_ptr() as *mut c_void,
                self.recv.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if err == 0 {
            Ok(())
        } else {
            Err(error_text(err))
        }
    }

    /// Start the next queued send if none is in flight.
    fn flush(&mut self, out: &mut VecDeque<Out>) -> Result<(), String> {
        while self.sending.is_none() && !self.shutting_down {
            let Some(next) = out.pop_front() else {
                return Ok(());
            };
            let ws = self.ws();
            let err = match next {
                Out::Frame(bytes, binary) => {
                    let kind = if binary {
                        WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE
                    } else {
                        WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE
                    };
                    let bytes = self.sending.insert(bytes);
                    unsafe { WinHttpWebSocketSend(ws, kind, Some(bytes.as_slice())) }
                }
                Out::Shutdown(code, reason) => {
                    self.reason = reason.into_bytes();
                    self.reason.truncate(123);
                    let reason = if self.reason.is_empty() { None } else { Some(self.reason.as_ptr() as *const c_void) };
                    match unsafe { WinHttpWebSocketShutdown(ws, code, reason, self.reason.len() as u32) } {
                        ERROR_IO_PENDING => {
                            self.shutting_down = true;
                            0
                        }
                        err => err,
                    }
                }
            };
            if err != 0 {
                return Err(error_text(err));
            }
        }
        Ok(())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Closing the handles cancels their pending operations.
        self.ws = None;
        self.request = None;
        let deadline = Instant::now() + HANDLE_CLOSE_WAIT;
        while !self.live.is_empty() {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Msg::HandleClosing(h)) => self.live.retain(|x| *x != h),
                Ok(_) => {}
                Err(_) => {
                    std::mem::forget(std::mem::take(&mut self.recv));
                    std::mem::forget(self.sending.take());
                    std::mem::forget(std::mem::take(&mut self.headers));
                    std::mem::forget(std::mem::take(&mut self.reason));
                    break;
                }
            }
        }
        // `connect` closes after its children.
    }
}

fn run_socket(
    id: i32,
    url: String,
    protocols: Vec<String>,
    rx: Receiver<Msg>,
    context: Arc<Context>,
    buffered: Arc<AtomicU64>,
    events: Sender<(i32, Event)>,
) {
    let mut report = Report { id, events, done: false };
    let mut conn = Conn {
        rx,
        context,
        connect: None,
        request: None,
        ws: None,
        live: Vec::new(),
        headers: Vec::new(),
        recv: vec![0u8; 64 * 1024],
        sending: None,
        shutting_down: false,
        reason: Vec::new(),
    };
    match conn.handshake(&url, &protocols) {
        Ok(protocol) => report.send(Event::Open(protocol)),
        Err(error) => {
            report.close(error, 1006, String::new(), false);
            return;
        }
    }
    serve(&mut conn, &mut report, &buffered);
}

/// Runs an open socket until its close event is reported (and our close frame had a moment to
/// go out).
fn serve(conn: &mut Conn, report: &mut Report, buffered: &AtomicU64) {
    let mut out: VecDeque<Out> = VecDeque::new();
    let mut message: Vec<u8> = Vec::new();
    // our close frame is queued or sent: nothing more goes out after it
    let mut close_queued = false;
    // close() timeout, then (after the close event) the linger time
    let mut deadline: Option<Instant> = None;
    if let Err(e) = conn.receive() {
        report.close(Some(e), 1006, String::new(), false);
        return;
    }
    loop {
        if let Err(e) = conn.flush(&mut out) {
            report.close(Some(e), 1006, String::new(), false);
            return;
        }
        if report.done && conn.sending.is_none() && !conn.shutting_down && out.is_empty() {
            return;
        }
        let Some(msg) = conn.next(deadline) else {
            // close() unanswered (a no-op once the close event was reported)
            report.close(None, 1006, String::new(), false);
            return;
        };
        match msg {
            Msg::Send(bytes, binary) => {
                if !close_queued {
                    out.push_back(Out::Frame(bytes, binary));
                }
            }
            Msg::Close(..) | Msg::Detach if close_queued => {}
            Msg::Close(code, reason) => {
                close_queued = true;
                out.push_back(Out::Shutdown(code, reason));
                deadline = Some(Instant::now() + CLOSE_TIMEOUT);
            }
            Msg::Detach => {
                close_queued = true;
                out.push_back(Out::Shutdown(1001, String::new()));
                deadline = Some(Instant::now() + CLOSE_TIMEOUT);
            }
            Msg::ReadComplete(read, kind) => {
                if report.done {
                    continue;
                }
                if message.len() + read as usize > MAX_MESSAGE_BYTES {
                    report.close(Some("WebSocket message exceeds 100 MiB".into()), 1009, String::new(), false);
                    out.retain(|o| matches!(o, Out::Shutdown(..)));
                    if !close_queued {
                        close_queued = true;
                        out.push_back(Out::Shutdown(1009, String::new()));
                    }
                    deadline = Some(Instant::now() + LINGER);
                    continue;
                }
                message.extend_from_slice(&conn.recv[..read as usize]);
                match kind {
                    WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE | WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE => {}
                    WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE => {
                        let text = String::from_utf8_lossy(&std::mem::take(&mut message)).into_owned();
                        report.send(Event::Text(text));
                    }
                    WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE => {
                        report.send(Event::Binary(std::mem::take(&mut message)));
                    }
                    WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE => {
                        let mut code = 1005u16;
                        let mut reason = [0u8; 123];
                        let mut reason_len = 0u32;
                        unsafe {
                            let _ = WinHttpWebSocketQueryCloseStatus(
                                conn.ws(),
                                &mut code,
                                Some(reason.as_mut_ptr() as *mut c_void),
                                reason.len() as u32,
                                &mut reason_len,
                            );
                        }
                        let reason = String::from_utf8_lossy(&reason[..reason_len as usize]).into_owned();
                        // The server is closing: drop unsent frames, answer with its code (unless
                        // our close frame is already queued or sent), and give that a moment.
                        out.retain(|o| matches!(o, Out::Shutdown(..)));
                        if !close_queued {
                            close_queued = true;
                            out.push_back(Out::Shutdown(code, String::new()));
                        }
                        report.close(None, code, reason, true);
                        deadline = Some(Instant::now() + LINGER);
                        continue;
                    }
                    _ => {}
                }
                if let Err(e) = conn.receive() {
                    report.close(Some(e), 1006, String::new(), false);
                    return;
                }
            }
            Msg::WriteComplete => {
                if let Some(sent) = conn.sending.take() {
                    buffered.fetch_sub(sent.len() as u64, Ordering::Relaxed);
                }
            }
            Msg::ShutdownComplete => conn.shutting_down = false,
            Msg::Failed(err) => {
                report.close(Some(error_text(err)), 1006, String::new(), false);
                return;
            }
            Msg::SendRequestComplete | Msg::HeadersAvailable | Msg::HandleClosing(_) => {}
        }
    }
}

/// `__nsWebSocketOpen(url, protocols, onEvent) → id`. `onEvent(type, a, b, c)` receives
/// `('open', protocol)`, `('message', data)`, `('error', message)` and
/// `('close', code, reason, wasClean)`.
#[cfg(feature = "classic")]
pub(crate) fn handle_open(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let url = args.get(0).to_rust_string_lossy(scope);
    let mut protocols = Vec::new();
    if let Ok(list) = v8::Local::<v8::Array>::try_from(args.get(1)) {
        for i in 0..list.length() {
            if let Some(p) = list.get_index(scope, i) {
                protocols.push(p.to_rust_string_lossy(scope));
            }
        }
    }
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(2)) else {
        return;
    };
    let callback = v8::Global::new(scope, callback);
    let id = connect(url, protocols);
    CALLBACKS.with(|c| c.borrow_mut().insert(id, callback));
    rv.set(v8::Integer::new(scope, id).into());
}

/// `__nsWebSocketSend(id, data)`: a string (text frame) or ArrayBuffer/view (binary frame).
/// Returns false when the socket isn't open.
#[cfg(feature = "classic")]
pub(crate) fn handle_send(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let id = args.get(0).int32_value(scope).unwrap_or(0);
    let data = args.get(1);
    let (bytes, binary) = if data.is_string() {
        (data.to_rust_string_lossy(scope).into_bytes(), false)
    } else if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(data) {
        let mut v = vec![0u8; view.byte_length()];
        view.copy_contents(&mut v);
        (v, true)
    } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(data) {
        let len = ab.byte_length();
        let v = match ab.data() {
            Some(p) if len > 0 => unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, len) }.to_vec(),
            _ => Vec::new(),
        };
        (v, true)
    } else {
        (data.to_rust_string_lossy(scope).into_bytes(), false)
    };
    rv.set(v8::Boolean::new(scope, send(id, bytes, binary)).into());
}

/// `__nsWebSocketBufferedAmount(id)`: bytes queued by `send` and not yet written.
#[cfg(feature = "classic")]
pub(crate) fn handle_buffered_amount(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let id = args.get(0).int32_value(scope).unwrap_or(0);
    rv.set(v8::Number::new(scope, buffered_amount(id) as f64).into());
}

/// `__nsWebSocketClose(id, code, reason)`.
#[cfg(feature = "classic")]
pub(crate) fn handle_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let id = args.get(0).int32_value(scope).unwrap_or(0);
    let code = args.get(1).uint32_value(scope).filter(|c| *c > 0).unwrap_or(1000) as u16;
    let reason = if args.get(2).is_undefined() { String::new() } else { args.get(2).to_rust_string_lossy(scope) };
    shutdown(id, code, &reason);
}

#[cfg(feature = "classic")]
fn str_val<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
    match v8::String::new(scope, s) {
        Some(v) => v.into(),
        None => v8::undefined(scope).into(),
    }
}

/// Deliver queued socket events to JS. Runs from `timers::pump()` on the JS thread.
#[cfg(feature = "classic")]
pub(crate) fn pump() {
    let events = take_events();
    if events.is_empty() {
        return;
    }
    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return;
    }
    let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
    v8::scope!(scope, isolate);
    let Some(ctx_global) = scope.get_slot::<v8::Global<v8::Context>>().cloned() else {
        return;
    };
    let context = v8::Local::new(scope, &ctx_global);
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(tc, scope);
    for (id, event) in events {
        let closing = matches!(event, Event::Close { .. });
        let Some(callback) = CALLBACKS.with(|c| c.borrow().get(&id).cloned()) else {
            if closing {
                forget(id);
            }
            continue;
        };
        let callback = v8::Local::new(tc, &callback);
        let argv: Vec<v8::Local<v8::Value>> = match event {
            Event::Open(protocol) => vec![str_val(tc, "open"), str_val(tc, &protocol)],
            Event::Text(text) => vec![str_val(tc, "message"), str_val(tc, &text)],
            Event::Binary(bytes) => {
                let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
                let ab = v8::ArrayBuffer::with_backing_store(tc, &store);
                vec![str_val(tc, "message"), ab.into()]
            }
            Event::Error(message) => vec![str_val(tc, "error"), str_val(tc, &message)],
            Event::Close { code, reason, clean } => vec![
                str_val(tc, "close"),
                v8::Integer::new(tc, code as i32).into(),
                str_val(tc, &reason),
                v8::Boolean::new(tc, clean).into(),
            ],
        };
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let _ = callback.call(tc, recv, &argv);
        if tc.has_caught() {
            if let Some(ex) = tc.exception() {
                let msg = ex.to_rust_string_lossy(tc);
                crate::debug_output(&format!("[NativeScript] WebSocket event handler error: {msg}\n"));
            }
            tc.reset();
        }
        if closing {
            CALLBACKS.with(|c| c.borrow_mut().remove(&id));
            forget(id);
        }
    }
    if !crate::defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }
}
