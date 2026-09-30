use anyhow::Result;
use parking_lot::Mutex;
use serde_json::Value as JsonValue;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use tungstenite::Message;
use v8::inspector::{
    Channel, ChannelImpl, StringBuffer, StringView, V8Inspector, V8InspectorClient,
    V8InspectorClientImpl, V8InspectorClientTrustLevel, V8InspectorSession,
};
use v8::UniquePtr;

// ─── Public types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevtoolsServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for DevtoolsServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 43000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevtoolsEndpoint {
    pub websocket_url: String,
    pub frontend_url: String,
}

// ─── Channel impl (V8 → WS outbound) ─────────────────────────────────────────

struct DevtoolsChannel {
    tx: SyncSender<String>,
    forwarder: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

fn string_buffer_to_string(buf: &StringBuffer) -> String {
    match buf.string() {
        StringView::U8(chars) => String::from_utf8_lossy(&chars).into_owned(),
        StringView::U16(chars) => String::from_utf16_lossy(&chars),
    }
}

fn format_remote_object(arg: &JsonValue, depth: usize) -> String {
    // Avoid deep recursion; increase if you need more detail
    if depth > 2 {
        if let Some(d) = arg.get("description").and_then(|d| d.as_str()) {
            return d.to_string();
        }
        return "[Object]".to_string();
    }

    if arg.is_null() {
        return "null".to_string();
    }

    if let Some(v) = arg.get("value") {
        if v.is_string() {
            return v.as_str().unwrap().to_string();
        } else if v.is_array() {
            let mut items = Vec::new();
            for it in v.as_array().unwrap().iter() {
                if it.is_string() {
                    items.push(it.as_str().unwrap().to_string())
                } else {
                    items.push(it.to_string())
                }
            }
            return format!("[{}]", items.join(", "));
        } else {
            return v.to_string();
        }
    }

    if let Some(desc) = arg.get("description").and_then(|d| d.as_str()) {
        return desc.to_string();
    }

    if let Some(preview) = arg.get("preview") {
        if let Some(items) = preview.get("items").and_then(|i| i.as_array()) {
            let mut elems = Vec::new();
            for it in items.iter() {
                if let Some(vv) = it.get("value") {
                    elems.push(format_remote_object(vv, depth + 1));
                } else if let Some(desc) = it.get("description").and_then(|d| d.as_str()) {
                    elems.push(desc.to_string());
                } else {
                    elems.push(it.to_string());
                }
            }
            return format!("[{}]", elems.join(", "));
        }

        if let Some(props) = preview.get("properties").and_then(|p| p.as_array()) {
            let mut pairs = Vec::new();
            for p in props.iter() {
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                let val = if let Some(vv) = p.get("value") {
                    format_remote_object(vv, depth + 1)
                } else if let Some(dv) = p.get("description").and_then(|d| d.as_str()) {
                    dv.to_string()
                } else {
                    p.to_string()
                };
                pairs.push(format!("{}: {}", name, val));
            }
            return format!("{{{}}}", pairs.join(", "));
        }

        return preview.to_string();
    }

    arg.to_string()
}

fn format_stack_trace(stack: &JsonValue) -> String {
    // stack: { callFrames: [ { functionName, url, lineNumber, columnNumber }, ... ] }
    if let Some(frames) = stack.get("callFrames").and_then(|f| f.as_array()) {
        let mut out = String::new();
        for f in frames.iter() {
            let fn_name = f
                .get("functionName")
                .and_then(|s| s.as_str())
                .unwrap_or("(anonymous)");
            let url = f.get("url").and_then(|s| s.as_str()).unwrap_or("");
            let line = f.get("lineNumber").and_then(|n| n.as_i64()).unwrap_or(0);
            let col = f.get("columnNumber").and_then(|n| n.as_i64()).unwrap_or(0);
            if url.is_empty() {
                out.push_str(&format!("    at {}\n", fn_name));
            } else {
                out.push_str(&format!("    at {} ({}:{}:{})\n", fn_name, url, line, col));
            }
        }
        return out;
    }
    String::new()
}

fn try_format_inspector_message(s: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<JsonValue>(s) {
        if let Some(method) = json.get("method").and_then(|m| m.as_str()) {
            match method {
                "Runtime.consoleAPICalled" => {
                    if let Some(params) = json.get("params") {
                        let typ = params.get("type").and_then(|t| t.as_str()).unwrap_or("log");
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(args) = params.get("args").and_then(|a| a.as_array()) {
                            for arg in args.iter() {
                                parts.push(format_remote_object(arg, 0));
                            }
                        }
                        let mut out =
                            format!("[DEVTOOLS] [{}] {}", typ.to_uppercase(), parts.join(" "));
                        if let Some(stack) = params.get("stackTrace") {
                            out.push_str("\n");
                            out.push_str(&format_stack_trace(stack));
                        }
                        out.push('\n');
                        return Some(out);
                    }
                }
                "Console.messageAdded" => {
                    if let Some(msg) = json.get("params").and_then(|p| p.get("message")) {
                        let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("info");
                        let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        let out = format!("[DEVTOOLS] [{}] {}\n", level.to_uppercase(), text);
                        return Some(out);
                    }
                }
                "Runtime.exceptionThrown" => {
                    if let Some(params) = json.get("params") {
                        if let Some(details) = params.get("exceptionDetails") {
                            let text = details
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("(exception)");
                            let mut out = format!("[DEVTOOLS] [EXCEPTION] {}\n", text);
                            if let Some(stack) = details.get("stackTrace") {
                                out.push_str(&format_stack_trace(stack));
                            }
                            out.push('\n');
                            return Some(out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

impl ChannelImpl for DevtoolsChannel {
    fn send_response(&self, _call_id: i32, message: UniquePtr<StringBuffer>) {
        if let Some(buf) = message.as_ref() {
            let s = string_buffer_to_string(buf);
            if let Some(fwd) = &self.forwarder {
                if let Some(out) = try_format_inspector_message(&s) {
                    fwd(&out);
                }
            }
            let _ = self.tx.try_send(s);
        }
    }
    fn send_notification(&self, message: UniquePtr<StringBuffer>) {
        if let Some(buf) = message.as_ref() {
            let s = string_buffer_to_string(buf);
            if let Some(fwd) = &self.forwarder {
                if let Some(out) = try_format_inspector_message(&s) {
                    fwd(&out);
                }
            }
            let _ = self.tx.try_send(s);
        }
    }
    fn flush_protocol_notifications(&self) {}
}

// ─── Client impl (breakpoint pause loop) ─────────────────────────────────────

struct DevtoolsClient {
    isolate: *mut v8::Isolate,
    context: v8::Global<v8::Context>,
    paused: Arc<AtomicBool>,
    debugger_released: Arc<AtomicBool>,
    inbound_rx: Arc<Mutex<Receiver<String>>>,
    session_ptr: Arc<AtomicUsize>,
}

impl V8InspectorClientImpl for DevtoolsClient {
    fn run_message_loop_on_pause(&self, _ctx_group_id: i32) {
        self.paused.store(true, Ordering::Release);
        while self.paused.load(Ordering::Acquire) {
            if let Ok(msg) = self.inbound_rx.lock().try_recv() {
                let ptr = self.session_ptr.load(Ordering::Acquire);
                if ptr != 0 {
                    // SAFETY: session_ptr is published via Release from the V8 thread
                    // and zeroed in DevtoolsServer::drop before the Box is released.
                    // This callback also runs on the V8 thread, so there is no
                    // concurrent access to the session.
                    unsafe {
                        let view = StringView::from(msg.as_bytes());
                        (*(ptr as *const V8InspectorSession)).dispatch_protocol_message(view);
                    }
                }
            } else {
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    fn quit_message_loop_on_pause(&self) {
        self.paused.store(false, Ordering::Release);
    }

    // Default context for CDP calls sent without a `contextId`.
    fn ensure_default_context_in_group(&self, _ctx_group_id: i32) -> Option<v8::Local<'_, v8::Context>> {
        // SAFETY: V8 calls this inside its HandleScope; CallbackScope adds none.
        let isolate = unsafe { &mut *self.isolate };
        v8::callback_scope!(unsafe scope, isolate);
        let context = v8::Local::new(scope, &self.context);
        Some(unsafe { std::mem::transmute::<v8::Local<'_, v8::Context>, v8::Local<'_, v8::Context>>(context) })
    }

    // Sent by the frontend once its startup handshake is done.
    fn run_if_waiting_for_debugger(&self, _ctx_group_id: i32) {
        self.debugger_released.store(true, Ordering::Release);
    }
}

// ─── DevtoolsServer ───────────────────────────────────────────────────────────

pub struct DevtoolsServer {
    endpoint: DevtoolsEndpoint,
    inbound_rx: Arc<Mutex<Receiver<String>>>,
    session_ptr: Arc<AtomicUsize>,
    outbound_tx: SyncSender<String>,
    message_dispatcher: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
    debugger_released: Arc<AtomicBool>,
    // Fields drop in order: the session must drop before its inspector.
    session: Box<V8InspectorSession>,
    _inspector: V8Inspector,
}

impl DevtoolsServer {
    /// Bind a TCP listener, start the background WS server thread, and wire the
    /// V8 inspector to `isolate`/`global_context`.  Must be called from the V8
    /// thread.  Takes a `Global<Context>` so that no active scope is required at
    /// the call site — a short-lived scope is created internally for the
    /// `context_created` registration.
    pub fn attach(
        config: &DevtoolsServerConfig,
        isolate: &mut v8::Isolate,
        global_context: &v8::Global<v8::Context>,
        console_forwarder: Option<Arc<dyn Fn(&str) + Send + Sync>>,
        message_dispatcher: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
    ) -> Result<Self> {
        // Try the configured port first; if it is already in use (e.g. from a
        // previous run that did not shut down cleanly), scan upward for a free
        // one so the runtime does not crash on reconnect.
        let listener = Self::bind_listener(&config.host, config.port)?;
        let addr = listener.local_addr()?;

        let websocket_url = format!("ws://{}/devtools/page/runtime", addr);
        let frontend_url = format!(
            "devtools://devtools/bundled/inspector.html?ws={}/devtools/page/runtime",
            addr
        );

        let (outbound_tx, outbound_rx) = mpsc::sync_channel::<String>(512);
        let (inbound_tx, inbound_rx) = mpsc::channel::<String>();
        let inbound_rx = Arc::new(Mutex::new(inbound_rx));

        let paused = Arc::new(AtomicBool::new(false));
        let debugger_released = Arc::new(AtomicBool::new(false));
        let session_ptr = Arc::new(AtomicUsize::new(0));

        let client = V8InspectorClient::new(Box::new(DevtoolsClient {
            isolate: isolate as *mut v8::Isolate,
            context: global_context.clone(),
            paused,
            debugger_released: Arc::clone(&debugger_released),
            inbound_rx: Arc::clone(&inbound_rx),
            session_ptr: Arc::clone(&session_ptr),
        }));

        // create() only needs &mut Isolate — no active scope required.
        let inspector = V8Inspector::create(isolate, client);

        // Short-lived scope to localise the Global context for registration.
        {
            let name = b"NativeScript Runtime";
            v8::scope!(scope, isolate);
            let context = v8::Local::new(scope, global_context);
            inspector.context_created(context, 1, StringView::from(&name[..]), StringView::empty());
        }

        let session = Box::new(inspector.connect(
            1,
            Channel::new(Box::new(DevtoolsChannel {
                tx: outbound_tx.clone(),
                forwarder: console_forwarder.clone(),
            })),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        ));

        // The session is heap-allocated (Box), so its address is stable after this.
        // Release so any subsequent Acquire load on another thread sees the fully
        // initialized session.
        session_ptr.store(
            session.as_ref() as *const V8InspectorSession as usize,
            Ordering::Release,
        );

        let ws_url = websocket_url.clone();
        thread::Builder::new()
            .name("ns-devtools-server".to_string())
            .spawn(move || run_server(listener, inbound_tx, outbound_rx, ws_url))?;

        Ok(Self {
            endpoint: DevtoolsEndpoint {
                websocket_url,
                frontend_url,
            },
            inbound_rx,
            session_ptr,
            outbound_tx,
            message_dispatcher,
            debugger_released,
            session,
            _inspector: inspector,
        })
    }

    /// Try to bind a TCP listener on `host:port`, falling back to up to 10
    /// subsequent ports if the preferred port is already in use.
    fn bind_listener(host: &str, port: u16) -> Result<TcpListener> {
        for offset in 0u16..=10 {
            let p = port.saturating_add(offset);
            match TcpListener::bind(format!("{}:{}", host, p)) {
                Ok(l) => {
                    if offset > 0 {
                        eprintln!(
                            "[devtools] port {} in use, bound to {}:{} instead",
                            port, host, p
                        );
                    }
                    return Ok(l);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(anyhow::anyhow!(
            "Could not bind devtools listener: ports {}–{} on {} are all in use",
            port,
            port.saturating_add(10),
            host
        ))
    }

    /// Send a pre-formed DevTools protocol message string to connected clients.
    /// Returns an error if sending fails (e.g. channel full or no clients).
    pub fn send(&self, message: &str) -> Result<()> {
        match self.outbound_tx.try_send(message.to_string()) {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("failed to send inspector message: {:?}", e)),
        }
    }

    pub fn endpoint(&self) -> &DevtoolsEndpoint {
        &self.endpoint
    }

    /// Pumps CDP until a frontend attaches, then pauses; `false` on timeout.
    pub fn wait_for_debugger(&mut self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.pump_messages();
            if self.debugger_released.load(Ordering::Acquire) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let reason = b"Break on start";
        self.session
            .schedule_pause_on_next_statement(StringView::from(&reason[..]), StringView::empty());
        true
    }

    /// Dispatch any pending CDP messages that arrived from a connected DevTools
    /// client to V8.  Call this periodically from the V8 thread.
    pub fn pump_messages(&mut self) {
        let ptr = self.session_ptr.load(Ordering::Acquire);
        if ptr == 0 {
            return;
        }
        while let Ok(msg) = self.inbound_rx.lock().try_recv() {
            // First allow optional embedder-provided JS dispatcher to handle
            // the message. If it returns `true` we skip default dispatch.
            if let Some(dispatcher) = &self.message_dispatcher {
                if dispatcher(&msg) {
                    continue;
                }
            }
            // SAFETY: same guarantee as in run_message_loop_on_pause.
            unsafe {
                let view = StringView::from(msg.as_bytes());
                (*(ptr as *const V8InspectorSession)).dispatch_protocol_message(view);
            }
        }
    }
}

impl Drop for DevtoolsServer {
    fn drop(&mut self) {
        // Zero before field destructors free the session (LIFO field drop order).
        self.session_ptr.store(0, Ordering::Release);
    }
}

// ─── Background TCP/WS server ─────────────────────────────────────────────────

fn run_server(
    listener: TcpListener,
    inbound_tx: mpsc::Sender<String>,
    outbound_rx: Receiver<String>,
    ws_url: String,
) {
    loop {
        let (stream, _addr) = match listener.accept() {
            Ok(s) => s,
            Err(_) => continue,
        };
        handle_connection(stream, &inbound_tx, &outbound_rx, &ws_url);
    }
}

fn handle_connection(
    stream: TcpStream,
    inbound_tx: &mpsc::Sender<String>,
    outbound_rx: &Receiver<String>,
    ws_url: &str,
) {
    // Peek at the request line to route without consuming any bytes.
    // TcpStream::peek() leaves data in the kernel receive buffer so that
    // tungstenite can re-read the full HTTP upgrade request.
    let mut peek_buf = [0u8; 256];
    let n = match stream.peek(&mut peek_buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let peek_str = String::from_utf8_lossy(&peek_buf[..n]);
    let path = peek_str
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    if path.starts_with("/json") {
        serve_json(stream, &path, ws_url);
    } else {
        stream.set_nonblocking(false).ok();
        if let Ok(ws) = tungstenite::accept(stream) {
            handle_ws(ws, inbound_tx, outbound_rx);
        }
    }
}

fn serve_json(mut stream: TcpStream, path: &str, ws_url: &str) {
    let body = if path == "/json/version" {
        build_version_json(ws_url)
    } else {
        build_list_json(ws_url)
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn handle_ws(
    mut ws: tungstenite::WebSocket<TcpStream>,
    inbound_tx: &mpsc::Sender<String>,
    outbound_rx: &Receiver<String>,
) {
    ws.get_mut().set_nonblocking(true).ok();

    loop {
        // Receive from the DevTools client.
        match ws.read() {
            Ok(Message::Text(text)) => {
                let _ = inbound_tx.send(text.to_string());
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        // Flush outbound messages (V8 → DevTools).
        while let Ok(msg) = outbound_rx.try_recv() {
            if ws.send(Message::Text(msg)).is_err() {
                return;
            }
        }

        thread::sleep(std::time::Duration::from_millis(1));
    }
}

// ─── CDP discovery JSON ───────────────────────────────────────────────────────

fn build_version_json(ws_url: &str) -> String {
    serde_json::json!({
        "Browser": "NativeScript/1.0",
        "Protocol-Version": "1.3",
        "V8-Version": "14.7",
        "webSocketDebuggerUrl": ws_url
    })
    .to_string()
}

fn build_list_json(ws_url: &str) -> String {
    let http_url = ws_url.replacen("ws://", "http://", 1);
    serde_json::json!([{
        "description": "NativeScript Runtime",
        "devtoolsFrontendUrl": format!(
            "devtools://devtools/bundled/inspector.html?ws={}",
            ws_url.trim_start_matches("ws://")
        ),
        "id": "nativescript-runtime",
        "title": "NativeScript Runtime",
        "type": "node",
        "url": http_url,
        "webSocketDebuggerUrl": ws_url
    }])
    .to_string()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = DevtoolsServerConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 43000);
    }

    #[test]
    fn version_json_is_valid() {
        let ws = "ws://127.0.0.1:43000/devtools/page/runtime";
        let v: serde_json::Value = serde_json::from_str(&build_version_json(ws)).unwrap();
        assert!(v.get("Browser").is_some());
        assert!(v.get("Protocol-Version").is_some());
        assert_eq!(
            v.get("webSocketDebuggerUrl").and_then(|u| u.as_str()),
            Some(ws)
        );
    }

    #[test]
    fn list_json_contains_ws_url() {
        let ws = "ws://127.0.0.1:43000/devtools/page/runtime";
        let v: serde_json::Value = serde_json::from_str(&build_list_json(ws)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["webSocketDebuggerUrl"], ws);
    }

    #[test]
    fn format_console_api_called_simple() {
        let json = serde_json::json!({
            "method": "Runtime.consoleAPICalled",
            "params": {
                "type": "log",
                "args": [ { "type": "string", "value": "hello" } ]
            }
        })
        .to_string();
        let out = try_format_inspector_message(&json).unwrap();
        assert_eq!(out, "[DEVTOOLS] [LOG] hello\n");
    }

    #[test]
    fn format_console_message_added_error() {
        let json = serde_json::json!({
            "method": "Console.messageAdded",
            "params": { "message": { "level": "error", "text": "oops" } }
        })
        .to_string();
        let out = try_format_inspector_message(&json).unwrap();
        assert_eq!(out, "[DEVTOOLS] [ERROR] oops\n");
    }

    #[test]
    fn format_exception_thrown_with_stack() {
        let json = serde_json::json!({
            "method": "Runtime.exceptionThrown",
            "params": {
                "exceptionDetails": {
                    "text": "TypeError: x is not a function",
                    "stackTrace": { "callFrames": [ { "functionName": "foo", "url": "file.js", "lineNumber": 10, "columnNumber": 5 } ] }
                }
            }
        })
        .to_string();
        let out = try_format_inspector_message(&json).unwrap();
        assert!(out.contains("[DEVTOOLS] [EXCEPTION] TypeError"));
        assert!(out.contains("at foo (file.js:10:5)") || out.contains("at foo"));
    }
}
