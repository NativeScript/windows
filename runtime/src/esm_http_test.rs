//! HTTP ES-module loading + `ns:module` + native WebSocket against real local servers. The
//! runtime contract `@nativescript/vite`'s HMR client relies on (as on iOS/Android).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::Runtime;

type Routes = Arc<Mutex<HashMap<String, (u16, &'static str, String)>>>;

/// A tiny HTTP/1.1 server: `routes` maps a path (query stripped) to (status, content type, body);
/// every request line's path+query is recorded.
struct TestServer {
    origin: String,
    routes: Routes,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (r, q) = (routes.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (r, q) = (r.clone(), q.clone());
                std::thread::spawn(move || serve(stream, &r, &q));
            }
        });
        TestServer { origin, routes, requests }
    }
    fn route(&self, path: &str, content_type: &'static str, body: &str) {
        self.routes.lock().unwrap().insert(path.into(), (200, content_type, body.into()));
    }
    fn requests_for(&self, path: &str) -> Vec<String> {
        self.requests.lock().unwrap().iter().filter(|p| p.split('?').next() == Some(path)).cloned().collect()
    }
}

fn serve(stream: TcpStream, routes: &Routes, requests: &Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut stream = stream;
    if let Some(key) = headers.get("sec-websocket-key") {
        return websocket_echo(stream, reader, key);
    }
    requests.lock().unwrap().push(target.clone());
    let path = target.split('?').next().unwrap_or("").to_string();
    let (status, ctype, body) = routes.lock().unwrap().get(&path).cloned().unwrap_or((404, "text/plain", "not found".into()));
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn websocket_accept(stream: &mut TcpStream, key: &str) {
    let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    let _ = write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
}

/// One masked client frame: (opcode, unmasked payload). None at EOF.
fn read_frame(reader: &mut BufReader<TcpStream>) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head).ok()?;
    let opcode = head[0] & 0x0f;
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        reader.read_exact(&mut b).ok()?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        reader.read_exact(&mut b).ok()?;
        len = u64::from_be_bytes(b) as usize;
    }
    let mut mask = [0u8; 4];
    reader.read_exact(&mut mask).ok()?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Some((opcode, payload))
}

/// An unmasked server frame.
fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80 | opcode];
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

fn websocket_echo(mut stream: TcpStream, mut reader: BufReader<TcpStream>, key: &str) {
    websocket_accept(&mut stream, key);
    while let Some((opcode, payload)) = read_frame(&mut reader) {
        let reply = if opcode == 1 { [b"echo:".as_slice(), &payload].concat() } else { payload };
        let _ = stream.write_all(&server_frame(opcode, &reply));
        if opcode == 8 {
            return;
        }
    }
}

/// A WebSocket server for one connection: accepts the upgrade, then hands the socket to `run`.
/// Returns its ws:// URL.
fn websocket_server(run: impl FnOnce(TcpStream, BufReader<TcpStream>) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://127.0.0.1:{}/ws", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut key = String::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.trim().eq_ignore_ascii_case("sec-websocket-key") {
                    key = v.trim().to_string();
                }
            }
        }
        websocket_accept(&mut stream, &key);
        run(stream, reader);
    });
    url
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn eval(runtime: &mut Runtime, expr: &str) -> String {
    runtime.eval_script_to_string(expr).unwrap_or_else(|| "<no result>".to_string())
}

/// Evaluate `expr` (a promise), pumping timers/sockets until `globalThis.__done` is set.
fn await_js(runtime: &mut Runtime, expr: &str) -> String {
    eval(
        runtime,
        &format!("globalThis.__done = undefined; Promise.resolve().then(() => {expr}).then(v => globalThis.__done = 'ok:' + v, e => globalThis.__done = 'err:' + (e && e.message || e)); 0"),
    );
    for _ in 0..500 {
        crate::timers::pump();
        let done = eval(runtime, "String(globalThis.__done)");
        if done != "undefined" {
            return done;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    "timeout".into()
}

#[test]
fn http_module_graph_import_map_invalidation_and_ns_module() {
    let server = TestServer::start();
    let o = server.origin.clone();
    server.route("/ns/m/entry.js", "text/javascript", "import { dep } from './dep.js?t=1';\nimport { abs } from '/ns/m/abs.js';\nimport { pkg } from 'pkg';\nimport data from './data.json';\nexport const value = [dep, abs, pkg, data.n].join(',');\nexport const meta = import.meta.url;");
    server.route("/ns/m/dep.js", "application/javascript; charset=utf-8", "export const dep = 'dep1';");
    server.route("/ns/m/abs.js", "text/javascript", "export const abs = 'abs';");
    server.route("/ns/vendor/pkg.js", "text/javascript", "export const pkg = 'pkg';");
    server.route("/ns/m/data.json", "application/json", "{\"n\": 7}");

    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let configure = format!(
        "require('ns:module').configureLoader({{ importMap: {{ imports: {{ pkg: '{o}/ns/vendor/pkg.js' }} }}, canonicalization: {{ stripParams: ['t'], forPathPrefixes: ['/ns/'] }} }}); 'configured'"
    );
    assert_eq!(eval(&mut runtime, &configure), "configured");

    let got = await_js(&mut runtime, &format!("import('{o}/ns/m/entry.js').then(m => m.value + '|' + m.meta)"));
    assert_eq!(got, format!("ok:dep1,abs,pkg,7|{o}/ns/m/entry.js"));

    // Loaded URLs are canonical (the `t` cache buster is gone).
    let loaded = eval(&mut runtime, "require('ns:module').getLoadedModuleUrls().join(' ')");
    assert!(loaded.contains(&format!("{o}/ns/m/dep.js ")) || loaded.ends_with(&format!("{o}/ns/m/dep.js")), "{loaded}");
    assert!(!loaded.contains("t=1"), "{loaded}");

    // Edit dep, invalidate, re-import: fresh body, fetched with a one-shot nonce.
    server.route("/ns/m/dep.js", "text/javascript", "export const dep = 'dep2';");
    assert_eq!(eval(&mut runtime, &format!("String(require('ns:module').invalidateModules(['{o}/ns/m/dep.js?t=99']))")), "1");
    let again = await_js(&mut runtime, &format!("import('{o}/ns/m/dep.js').then(m => m.dep)"));
    assert_eq!(again, "ok:dep2");
    let dep_requests = server.requests_for("/ns/m/dep.js");
    assert_eq!(dep_requests.len(), 2, "{dep_requests:?}");
    assert!(dep_requests[1].contains("__ns_dev_nonce="), "{dep_requests:?}");
    // The nonce is one-shot.
    eval(&mut runtime, &format!("require('ns:module').invalidateModules(['{o}/ns/m/abs.js'])"));
    await_js(&mut runtime, &format!("import('{o}/ns/m/abs.js')"));
    assert_eq!(server.requests_for("/ns/m/abs.js").len(), 2);

    // Static `import ... from "ns:module"` from a served module.
    server.route("/ns/m/uses-builtin.js", "text/javascript", "import ns, { getLoadedModuleUrls } from 'ns:module';\nexport const ok = typeof ns.configureLoader + typeof getLoadedModuleUrls;");
    assert_eq!(await_js(&mut runtime, &format!("import('{o}/ns/m/uses-builtin.js').then(m => m.ok)")), "ok:functionfunction");
}

#[test]
fn http_import_failures_reject_with_the_cause() {
    let server = TestServer::start();
    let o = server.origin.clone();
    server.route("/ns/m/broken.js", "text/javascript", "import './missing.js';");
    server.route("/ns/m/css.js", "text/css", "body{}");
    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let missing = await_js(&mut runtime, &format!("import('{o}/ns/m/broken.js')"));
    assert!(missing.starts_with("err:") && missing.contains("status=404") && missing.contains("missing.js"), "{missing}");
    let mime = await_js(&mut runtime, &format!("import('{o}/ns/m/css.js')"));
    assert!(mime.contains("MIME type 'text/css'"), "{mime}");
    let invalid = eval(&mut runtime, "try { require('ns:module').configureLoader({ bogus: 1 }); 'no' } catch (e) { e.name }");
    assert_eq!(invalid, "TypeError");
}

#[test]
fn worker_entry_served_over_http() {
    let server = TestServer::start();
    let o = server.origin.clone();
    server.route("/ns/m/echo.worker.js", "text/javascript", "import { add } from './add.js';\nself.onmessage = (event) => self.postMessage(add(event.data));");
    server.route("/ns/m/add.js", "text/javascript", "export const add = (n) => n + 1;");
    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let run = |path: &str| {
        format!(
            "new Promise((resolve, reject) => {{ const worker = new Worker(new URL('{o}{path}')); worker.onmessage = (event) => {{ worker.terminate(); event.type === 'messageerror' ? reject(event.error) : resolve(event.data); }}; worker.postMessage(41); }})"
        )
    };
    assert_eq!(await_js(&mut runtime, &run("/ns/m/echo.worker.js")), "ok:42");
    let missing = await_js(&mut runtime, &run("/ns/m/missing.worker.js"));
    assert!(missing.starts_with("err:") && missing.contains("status=404"), "{missing}");
}

#[test]
fn native_websocket_round_trip() {
    let server = TestServer::start();
    let ws = server.origin.replacen("http://", "ws://", 1);
    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let got = await_js(
        &mut runtime,
        &format!(
            "new Promise((resolve, reject) => {{
                const log = [];
                const s = new WebSocket('{ws}/ns-hmr');
                s.onopen = () => {{ log.push('open:' + s.readyState); s.send('hello'); }};
                s.addEventListener('message', (e) => {{ log.push(e.data); s.close(1000, 'bye'); }});
                s.onerror = (e) => log.push('error:' + e.message);
                s.onclose = (e) => {{ log.push('close:' + e.code + ':' + e.wasClean + ':' + s.readyState); resolve(log.join(' ')); }};
                __ns__setTimeout(() => reject(new Error('timeout ' + log.join(' '))), 4000);
            }})"
        ),
    );
    assert_eq!(got, "ok:open:1 echo:hello close:1000:true:3");

    let refused = await_js(
        &mut runtime,
        "new Promise((resolve) => { const s = new WebSocket('ws://127.0.0.1:1/x'); s.onclose = (e) => resolve(e.code + ':' + e.wasClean); })",
    );
    assert_eq!(refused, "ok:1006:false");
}

/// The server never reads: `send` returns at once, reads keep arriving, and the server's FIN
/// surfaces as an error and one close (1006).
#[test]
fn native_websocket_reads_while_writes_stall() {
    let url = websocket_server(|mut stream, _reader| {
        for i in 0..20 {
            if stream.write_all(&server_frame(1, format!("tick {i}").as_bytes())).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // FIN without a close frame; keep the socket so no RST unblocks the client's writes
        let _ = stream.shutdown(std::net::Shutdown::Write);
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let got = await_js(
        &mut runtime,
        &format!(
            "new Promise((resolve, reject) => {{
                const s = new WebSocket('{url}');
                let got = 0, stalled = 0, sendMs = -1, closes = 0, errors = 0;
                s.onopen = () => {{
                    const t0 = Date.now();
                    s.send('hi');
                    // far more than the socket buffers take while the server isn't reading
                    const big = new Uint8Array(1 << 20);
                    for (let i = 0; i < 48; i++) s.send(big);
                    sendMs = Date.now() - t0;
                }};
                s.onmessage = () => {{ got++; if (s.bufferedAmount > 0) stalled++; }};
                s.onerror = () => errors++;
                s.onclose = (e) => {{
                    closes++;
                    s.send('after close');
                    __ns__setTimeout(() => resolve([got, stalled, sendMs >= 0 && sendMs < 1000, e.code, e.wasClean, errors, closes, s.bufferedAmount > 0, s.readyState].join(':')), 300);
                }};
                __ns__setTimeout(() => reject(new Error('timeout ' + got)), 4500);
            }})"
        ),
    );
    assert_eq!(got, "ok:20:20:true:1006:false:1:1:true:3");
}

/// close() the server never answers: one close (1006) after the timeout, no messages after it.
#[test]
fn native_websocket_close_timeout_and_no_messages_after_close() {
    let url = websocket_server(|mut stream, _reader| {
        // keeps sending and never reads (so never sees, or answers, the close frame)
        for i in 0..200 {
            if stream.write_all(&server_frame(1, format!("tick {i}").as_bytes())).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let got = await_js(
        &mut runtime,
        &format!(
            "new Promise((resolve, reject) => {{
                const s = new WebSocket('{url}');
                let closedAt = 0, closeCallMs = -1, after = 0, closes = 0;
                s.onmessage = () => {{
                    if (closedAt) {{ after++; return; }}
                    closedAt = Date.now();
                    s.close(1000, 'bye');
                    closeCallMs = Date.now() - closedAt;
                    s.send('ignored');
                }};
                s.onclose = (e) => {{
                    closes++;
                    const waited = Date.now() - closedAt;
                    __ns__setTimeout(() => resolve([after, e.code, e.wasClean, closes, closeCallMs < 100, waited >= 1500 && waited < 4000].join(':')), 300);
                }};
                __ns__setTimeout(() => reject(new Error('timeout')), 4500);
            }})"
        ),
    );
    assert_eq!(got, "ok:0:1006:false:1:true:true");
}

#[link(name = "ws2_32")]
extern "system" {
    fn getpeername(socket: usize, name: *mut u8, len: *mut i32) -> i32;
    fn getsockopt(socket: usize, level: i32, name: i32, value: *mut u8, len: *mut i32) -> i32;
}

/// WinHTTP has no TCP_NODELAY option but sets it itself: finds the client socket among the
/// process's handles (by its peer port) and checks.
#[test]
fn native_websocket_uses_tcp_nodelay() {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let url = websocket_server(move |stream, _reader| {
        let _ = port_tx.send(stream.local_addr().unwrap().port());
        std::thread::sleep(std::time::Duration::from_secs(3));
    });
    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let got = await_js(
        &mut runtime,
        &format!("new Promise((resolve) => {{ const s = new WebSocket('{url}'); s.onopen = () => resolve('open'); }})"),
    );
    assert_eq!(got, "ok:open");
    let port = port_rx.recv().unwrap();
    let mut nodelay = Vec::new();
    for handle in (4usize..0x10000).step_by(4) {
        let mut addr = [0u8; 128];
        let mut len = addr.len() as i32;
        // fails (WSAENOTSOCK) for handles that aren't sockets
        if unsafe { getpeername(handle, addr.as_mut_ptr(), &mut len) } != 0 || u16::from_be_bytes([addr[2], addr[3]]) != port {
            continue;
        }
        let mut value = [0u8; 4];
        let mut len = 4i32;
        let ok = unsafe { getsockopt(handle, 6 /* IPPROTO_TCP */, 1 /* TCP_NODELAY */, value.as_mut_ptr(), &mut len) } == 0;
        nodelay.push(ok && value[0] != 0);
    }
    assert_eq!(nodelay, [true]);
}

#[test]
fn legacy_dynamic_import_uses_the_module_loader() {
    let server = TestServer::start();
    let o = server.origin.clone();
    server.route("/remote/mod.js", "text/javascript", "export default 'remote'; export const n = 2;");
    server.route("/remote/style.css", "text/css", "body{}");

    let mut runtime = Runtime::new(".");
    runtime.register_delegate_isolate_ptr();
    let got = await_js(&mut runtime, &format!("NSWinRT.dynamicImport('{o}/remote/mod.js').then(m => m.default + m.n)"));
    assert_eq!(got, "ok:remote2");
    // The loader's MIME check applies too.
    let css = await_js(&mut runtime, &format!("NSWinRT.dynamicImport('{o}/remote/style.css')"));
    assert!(css.starts_with("err:"), "{css}");
}
