use runtime_devtools::{DevtoolsServer, DevtoolsServerConfig};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tungstenite::Message;

fn init_v8() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

fn new_context(isolate: &mut v8::Isolate) -> v8::Global<v8::Context> {
    v8::scope!(scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    v8::Global::new(scope, context)
}

fn run(isolate: &mut v8::Isolate, context: &v8::Global<v8::Context>, code: &str) -> i32 {
    v8::scope!(scope, isolate);
    let context = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context);
    let code = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap().int32_value(scope).unwrap()
}

#[test]
fn wait_for_debugger_times_out_without_frontend() {
    init_v8();
    let isolate = &mut v8::Isolate::new(Default::default());
    let context = new_context(isolate);
    let config = DevtoolsServerConfig {
        host: "127.0.0.1".to_string(),
        port: 43200,
    };
    let mut server = DevtoolsServer::attach(&config, isolate, &context, None, None).unwrap();

    assert!(!server.wait_for_debugger(Duration::from_millis(200)));
}

#[test]
fn wait_for_debugger_pauses_on_next_statement_once_frontend_attaches() {
    init_v8();
    let isolate = &mut v8::Isolate::new(Default::default());
    let context = new_context(isolate);
    let config = DevtoolsServerConfig {
        host: "127.0.0.1".to_string(),
        port: 43100,
    };
    let mut server = DevtoolsServer::attach(&config, isolate, &context, None, None).unwrap();
    let ws_url = server.endpoint().websocket_url.clone();

    // Mimics the DevTools startup handshake, then resumes the pause.
    let (paused_tx, paused_rx) = mpsc::channel::<String>();
    let frontend = thread::spawn(move || {
        let (mut ws, _) = tungstenite::connect(ws_url.as_str()).unwrap();
        for (id, method) in [
            (1, "Runtime.enable"),
            (2, "Debugger.enable"),
            (3, "Runtime.runIfWaitingForDebugger"),
        ] {
            ws.send(Message::Text(format!(r#"{{"id":{id},"method":"{method}"}}"#)))
                .unwrap();
        }
        loop {
            if let Message::Text(text) = ws.read().unwrap() {
                if text.contains(r#""method":"Debugger.paused""#) {
                    paused_tx.send(text).unwrap();
                    ws.send(Message::Text(r#"{"id":4,"method":"Debugger.resume"}"#.to_string()))
                        .unwrap();
                    // Keep the socket open until the pause loop reads the resume.
                    thread::sleep(Duration::from_millis(500));
                    return;
                }
            }
        }
    });

    assert!(server.wait_for_debugger(Duration::from_secs(10)));

    // The pause blocks inside `run` until the frontend sends Debugger.resume.
    assert_eq!(run(isolate, &context, "var x = 1 + 1; x"), 2);

    let paused = paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(paused.contains("Debugger.paused"));
    frontend.join().unwrap();
}

#[test]
fn evaluate_without_context_id_uses_default_context() {
    init_v8();
    let isolate = &mut v8::Isolate::new(Default::default());
    let context = new_context(isolate);
    let config = DevtoolsServerConfig {
        host: "127.0.0.1".to_string(),
        port: 43400,
    };
    let mut server = DevtoolsServer::attach(&config, isolate, &context, None, None).unwrap();
    let ws_url = server.endpoint().websocket_url.clone();

    let (reply_tx, reply_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let (mut ws, _) = tungstenite::connect(ws_url.as_str()).unwrap();
        ws.send(Message::Text(
            r#"{"id":1,"method":"Runtime.evaluate","params":{"expression":"6 * 7","returnByValue":true}}"#
                .to_string(),
        ))
        .unwrap();
        loop {
            if let Message::Text(text) = ws.read().unwrap() {
                if text.contains(r#""id":1"#) {
                    reply_tx.send(text.to_string()).unwrap();
                    return;
                }
            }
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let reply = loop {
        server.pump_messages();
        if let Ok(reply) = reply_rx.try_recv() {
            break reply;
        }
        assert!(std::time::Instant::now() < deadline, "no Runtime.evaluate reply");
        thread::sleep(Duration::from_millis(5));
    };
    assert!(reply.contains(r#""value":42"#), "{reply}");
}
