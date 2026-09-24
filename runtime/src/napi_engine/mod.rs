//! Node-API backend for the runtime: implements the same WinRT interop surface as the rusty_v8
//! engine, written directly against Node-API instead of V8's C++ API.
//!
//! Enabled by the `napi_engine` cargo feature; the default build is unaffected and keeps using
//! rusty_v8. The engine boundary is Node-API (`node_api.h`, via napi-rs), so the same code runs
//! on any host/engine that implements it (Node/Deno = V8, Bun = JSC, and standalone QuickJS/
//! Hermes/V8 via vendored shims).

pub mod composition_window;
pub mod console;
pub mod crash_reporter;
pub mod delegate;
pub mod dotnet;
pub mod event_loop;
pub mod globals;
pub mod host_abi;
pub mod interop;
pub mod invoke;
pub mod items_source;
pub mod module_natives;
#[cfg(feature = "module_runner")]
pub mod module_runner;
pub mod ns_hostobject;
pub mod ns_proxy;
pub mod timers;
pub mod url;
pub mod value;
pub mod websocket;

// Engine-neutral marshaling types the napi backend and standalone hosts build against.
pub use crate::value::{NativeType, NativeValue};
