//! Node-API over the classic engine's V8. The `napi_*` engine functions (js_native_api) come from
//! the compiled shim and are exported from the final DLL; the Node-specific ones (threadsafe
//! functions, async work, cleanup hooks, buffers, …) live in `runtime::node_api`.

use std::ffi::c_void;

// Keeps rusty_v8's static library in the link for the C++ shim.
extern crate v8;

pub type NapiEnv = *mut c_void;
pub type NapiValue = *mut c_void;
pub type AddonInit = unsafe extern "C" fn(NapiEnv, NapiValue) -> NapiValue;

extern "C" {
    /// `context` is the pointer inside a `v8::Local<v8::Context>` of the current isolate.
    pub fn ns_napi_env_create(context: *const c_void, module_api_version: i32) -> NapiEnv;
    pub fn ns_napi_call_module_init(env: NapiEnv, init: AddonInit, exports: NapiValue) -> NapiValue;
    pub fn ns_napi_drain_finalizers(env: NapiEnv);
    pub fn ns_napi_has_pending_finalizers(env: NapiEnv) -> bool;
    pub fn ns_napi_env_teardown(env: NapiEnv);
}
