# runtime-devtools

V8 Inspector bridge that lets Chrome DevTools attach to a running NativeScript isolate.

## Usage

```rust
use runtime_devtools::{DevtoolsServer, DevtoolsServerConfig};

// On the V8 thread, after creating the isolate and context:
let mut server = DevtoolsServer::attach(
    &DevtoolsServerConfig::default(), // host: 127.0.0.1, port: 43000
    isolate,
    context,
)?;

println!("DevTools: {}", server.endpoint().frontend_url);

// In the JS execution loop:
server.pump_messages(); // dispatches pending CDP messages to V8
```

## What it does

1. Binds a `V8Inspector` + `V8InspectorSession` to the provided isolate/context.
2. Starts a background TCP server on the configured port.
3. Serves CDP discovery endpoints (`/json/version`, `/json/list`) as plain HTTP.
4. Upgrades `/devtools/page/runtime` connections to WebSocket and bridges them to V8.
5. Implements `run_message_loop_on_pause` so breakpoints work correctly.
6. `wait_for_debugger(timeout)` blocks until a frontend attaches, then pauses on the next statement (`--debug-brk`).

## Connecting

Open the `frontend_url` from `DevtoolsEndpoint` in Chrome, or use:

```
chrome://inspect  →  Configure  →  add 127.0.0.1:43000
```
