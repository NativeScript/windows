// Glue between the classic engine (rusty_v8, Rust) and the vendored Node-API shim (v8-api.cpp):
// create an env over an existing context, run a module's init inside the env's call guard, and
// drain finalizers the way Node does (outside GC, at a point where calling into JS is safe).

#include <cstring>

#include "js_native_api.h"
#include "v8-api.h"

// rusty_v8 hands us a `v8::Local` as the pointer it wraps; that only round-trips if a Local is a
// single pointer (true without V8_ENABLE_DIRECT_HANDLE, which the v8 crate's default build lacks).
static_assert(sizeof(v8::Local<v8::Context>) == sizeof(void*), "v8::Local must be pointer-sized");

// From node_api_types.h, which the vendored headers don't carry.
typedef napi_value (*napi_addon_register_func)(napi_env env, napi_value exports);

// v8-api.cpp.
void ns_napi_finalize_external_arraybuffers(napi_env env);

extern "C" {

// `context` is a `v8::Local<v8::Context>` (as its underlying pointer) of the isolate current on
// this thread. `module_api_version` is the addon's declared Node-API version.
napi_env ns_napi_env_create(void* context, int32_t module_api_version) {
    v8::Local<v8::Context> local;
    std::memcpy(static_cast<void*>(&local), &context, sizeof(void*));
    return new napi_env__(local, module_api_version);
}

// Calls `init(env, exports)` inside the env's call guard; an exception it throws is left pending
// on the isolate for the caller's TryCatch. Must run in a HandleScope with the env's context entered.
napi_value ns_napi_call_module_init(napi_env env, napi_addon_register_func init, napi_value exports) {
    napi_value result = nullptr;
    env->CallIntoModule([&](napi_env env) { result = init(env, exports); });
    return result;
}

// Runs finalizers the GC queued for `env` (module API versions below "experimental" defer them;
// see napi_env__::InvokeFinalizerFromGC). Node's `DrainFinalizerQueue`.
void ns_napi_drain_finalizers(napi_env env) {
    if (env->pending_finalizers.empty()) {
        return;
    }
    v8::HandleScope handle_scope(env->isolate);
    // env->context() aliases the env's persistent slot; take a real handle for the scope.
    v8::Local<v8::Context> context = v8::Local<v8::Context>::New(env->isolate, env->context());
    v8::Context::Scope context_scope(context);
    while (!env->pending_finalizers.empty()) {
        v8impl::RefTracker* tracker = *env->pending_finalizers.begin();
        env->pending_finalizers.erase(tracker);
        tracker->Finalize();
    }
}

bool ns_napi_has_pending_finalizers(napi_env env) {
    return !env->pending_finalizers.empty();
}

// Tears the env down: finalizes remaining references and external ArrayBuffers (running their
// finalizers) and frees it.
void ns_napi_env_teardown(napi_env env) {
    v8::Isolate* isolate = env->isolate;
    v8::HandleScope handle_scope(isolate);
    // A real handle, not env->context(): that aliases the env's persistent slot, which DeleteMe
    // frees before this scope exits.
    v8::Local<v8::Context> context = v8::Local<v8::Context>::New(isolate, env->context());
    v8::Context::Scope context_scope(context);
    ns_napi_finalize_external_arraybuffers(env);
    env->DeleteMe();
}

}  // extern "C"
