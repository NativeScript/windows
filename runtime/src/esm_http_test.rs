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

fn websocket_echo(mut stream: TcpStream, mut reader: BufReader<TcpStream>, key: &str) {
    let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    let _ = write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    loop {
        let mut head = [0u8; 2];
        if reader.read_exact(&mut head).is_err() {
            return;
        }
        let opcode = head[0] & 0x0f;
        let mut len = (head[1] & 0x7f) as usize;
        if len == 126 {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b).unwrap();
            len = u16::from_be_bytes(b) as usize;
        }
        let mut mask = [0u8; 4];
        reader.read_exact(&mut mask).unwrap();
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).unwrap();
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        let reply = if opcode == 1 { [b"echo:".as_slice(), &payload].concat() } else { payload };
        let mut frame = vec![0x80 | opcode];
        if reply.len() < 126 {
            frame.push(reply.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(reply.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&reply);
        let _ = stream.write_all(&frame);
        if opcode == 8 {
            return;
        }
    }
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
