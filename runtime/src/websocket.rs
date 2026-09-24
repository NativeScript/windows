//! Native `WebSocket` (the HELPER_SOURCE class wraps these natives). The iOS/Android runtimes get
//! theirs from `@valor/nativescript-websockets`, which has no Windows implementation; the Vite HMR
//! client needs one to receive updates.
//!
//! Each socket runs its WinHTTP handshake and blocking receive loop on its own thread. Events go
//! onto the owning JS thread's channel and are delivered by `pump()`, which runs with the timer
//! pump (the host drives it from its render/heartbeat loop), so JS is only ever called on the JS
//! thread. `send` runs on the JS thread; `close` sends the close frame without waiting, and the
//! receive loop reports the resulting close.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Networking::WinHttp::{
    WinHttpWebSocketQueryCloseStatus, WinHttpWebSocketReceive, WinHttpWebSocketSend,
    WinHttpWebSocketShutdown, WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE, WINHTTP_WEB_SOCKET_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE, WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE,
};

use crate::winhttp::Handle;
use crate::DELEGATE_ISOLATE_PTR;

enum Event {
    Open(String),
    Text(String),
    Binary(Vec<u8>),
    Error(String),
    Close { code: u16, reason: String, clean: bool },
}

struct Socket {
    ws: Handle,
    _connect: Handle,
}

fn sockets() -> &'static Mutex<HashMap<i32, Arc<Socket>>> {
    static SOCKETS: OnceLock<Mutex<HashMap<i32, Arc<Socket>>>> = OnceLock::new();
    SOCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicI32 = AtomicI32::new(1);

thread_local! {
    static CALLBACKS: RefCell<HashMap<i32, v8::Global<v8::Function>>> = RefCell::new(HashMap::new());
    static CHANNEL: (Sender<(i32, Event)>, Receiver<(i32, Event)>) = mpsc::channel();
}

/// Called from `Runtime::drop` while the isolate is alive.
pub(crate) fn clear_thread_sockets() {
    let ids: Vec<i32> = CALLBACKS.with(|c| c.borrow_mut().drain().map(|(id, _)| id).collect());
    if let Ok(mut map) = sockets().lock() {
        for id in ids {
            if let Some(s) = map.remove(&id) {
                unsafe {
                    let _ = WinHttpWebSocketShutdown(s.ws.raw(), 1001, None, 0);
                }
            }
        }
    }
}

fn run_socket(id: i32, url: String, protocols: Vec<String>, tx: Sender<(i32, Event)>) {
    let (ws, connect) = match crate::winhttp::websocket_connect(&url, &protocols) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx.send((id, Event::Error(e)));
            let _ = tx.send((id, Event::Close { code: 1006, reason: String::new(), clean: false }));
            return;
        }
    };
    let socket = Arc::new(Socket { ws, _connect: connect });
    if let Ok(mut map) = sockets().lock() {
        map.insert(id, socket.clone());
    }
    let _ = tx.send((id, Event::Open(protocols.first().cloned().unwrap_or_default())));

    let mut message: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let close = loop {
        let mut read = 0u32;
        let mut kind = WINHTTP_WEB_SOCKET_BUFFER_TYPE(0);
        let err = unsafe {
            WinHttpWebSocketReceive(
                socket.ws.raw(),
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
                &mut kind,
            )
        };
        if err != 0 {
            let reason = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(err)).message();
            let _ = tx.send((id, Event::Error(reason)));
            break Event::Close { code: 1006, reason: String::new(), clean: false };
        }
        message.extend_from_slice(&buf[..read as usize]);
        match kind {
            WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE | WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE => {}
            WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE => {
                let text = String::from_utf8_lossy(&std::mem::take(&mut message)).into_owned();
                let _ = tx.send((id, Event::Text(text)));
            }
            WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE => {
                let _ = tx.send((id, Event::Binary(std::mem::take(&mut message))));
            }
            WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE => {
                let mut code = 1005u16;
                let mut reason = [0u8; 123];
                let mut reason_len = 0u32;
                unsafe {
                    let _ = WinHttpWebSocketQueryCloseStatus(
                        socket.ws.raw(),
                        &mut code,
                        Some(reason.as_mut_ptr() as *mut c_void),
                        reason.len() as u32,
                        &mut reason_len,
                    );
                    // Complete the closing handshake if we didn't start it.
                    let _ = WinHttpWebSocketShutdown(socket.ws.raw(), code, None, 0);
                }
                let reason = String::from_utf8_lossy(&reason[..reason_len as usize]).into_owned();
                break Event::Close { code, reason, clean: true };
            }
            _ => {}
        }
    };
    if let Ok(mut map) = sockets().lock() {
        map.remove(&id);
    }
    let _ = tx.send((id, close));
}

/// `__nsWebSocketOpen(url, protocols, onEvent) → id`. `onEvent(type, a, b, c)` receives
/// `('open', protocol)`, `('message', data)`, `('error', message)` and
/// `('close', code, reason, wasClean)`.
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
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let callback = v8::Global::new(scope, callback);
    CALLBACKS.with(|c| c.borrow_mut().insert(id, callback));
    let tx = CHANNEL.with(|(tx, _)| tx.clone());
    std::thread::Builder::new()
        .name(format!("ns-websocket-{id}"))
        .spawn(move || run_socket(id, url, protocols, tx))
        .ok();
    rv.set(v8::Integer::new(scope, id).into());
}

/// `__nsWebSocketSend(id, data)`: a string (text frame) or ArrayBuffer/view (binary frame).
/// Returns false when the socket isn't open.
pub(crate) fn handle_send(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let id = args.get(0).int32_value(scope).unwrap_or(0);
    let Some(socket) = sockets().lock().ok().and_then(|m| m.get(&id).cloned()) else {
        rv.set(v8::Boolean::new(scope, false).into());
        return;
    };
    let data = args.get(1);
    let (bytes, kind) = if data.is_string() {
        (data.to_rust_string_lossy(scope).into_bytes(), WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE)
    } else if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(data) {
        let mut v = vec![0u8; view.byte_length()];
        view.copy_contents(&mut v);
        (v, WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE)
    } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(data) {
        let len = ab.byte_length();
        let v = match ab.data() {
            Some(p) if len > 0 => unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, len) }.to_vec(),
            _ => Vec::new(),
        };
        (v, WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE)
    } else {
        (data.to_rust_string_lossy(scope).into_bytes(), WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE)
    };
    let err = unsafe { WinHttpWebSocketSend(socket.ws.raw(), kind, Some(&bytes)) };
    rv.set(v8::Boolean::new(scope, err == 0).into());
}

/// `__nsWebSocketClose(id, code, reason)`.
pub(crate) fn handle_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let id = args.get(0).int32_value(scope).unwrap_or(0);
    let code = args.get(1).uint32_value(scope).filter(|c| *c > 0).unwrap_or(1000) as u16;
    let reason = if args.get(2).is_undefined() { String::new() } else { args.get(2).to_rust_string_lossy(scope) };
    let reason = &reason.as_bytes()[..reason.len().min(123)];
    if let Some(socket) = sockets().lock().ok().and_then(|m| m.get(&id).cloned()) {
        unsafe {
            let _ = WinHttpWebSocketShutdown(
                socket.ws.raw(),
                code,
                if reason.is_empty() { None } else { Some(reason.as_ptr() as *const c_void) },
                reason.len() as u32,
            );
        }
    }
}

fn str_val<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
    match v8::String::new(scope, s) {
        Some(v) => v.into(),
        None => v8::undefined(scope).into(),
    }
}

/// Deliver queued socket events to JS. Runs from `timers::pump()` on the JS thread.
pub(crate) fn pump() {
    let events: Vec<(i32, Event)> = CHANNEL.with(|(_, rx)| rx.try_iter().collect());
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
        let Some(callback) = CALLBACKS.with(|c| c.borrow().get(&id).cloned()) else {
            continue;
        };
        let callback = v8::Local::new(tc, &callback);
        let closing = matches!(event, Event::Close { .. });
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
        }
    }
    if !crate::defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }
}
