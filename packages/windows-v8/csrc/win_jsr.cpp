// [windows port] A clean Windows bring-up for the napi-android V8 shim (v8-api.cpp), replacing its
// Android/JNI-coupled jsr.cpp. Creates a V8 platform + isolate + context and wraps them in the
// shim's `napi_env__`. Single-threaded host: the isolate/context/handle-scope are entered once and
// kept open for the process lifetime (like the Hermes host's env scope), so napi calls from Rust
// work without per-call scope wrapping. Built against the `v8` crate's V8 14.7 headers; our v8 crate
// uses the default config (NO pointer compression / NO sandbox), so there is no ABI-define matching.

#include "v8-api.h"        // napi_env__, plus napi types
#include "js_native_api.h" // napi_run_script_source
#include "libplatform/libplatform.h"
#include "SimpleAllocator.h"
#include <algorithm>
#include <memory>
#include <string>
#include <unordered_map>
#include <vector>

struct napi_runtime__ {
    tns::SimpleAllocator* allocator;
    v8::Isolate* isolate;
};

// Forwards V8's fatal/OOM errors (CHECK failures otherwise crash silently via __debugbreak) to
// the Rust trace log; defined in windows-v8's lib.rs.
extern "C" void ns_v8_fatal_error(const char* location, const char* message);

static void OnV8FatalError(const char* location, const char* message) {
    ns_v8_fatal_error(location, message);
}

static void OnV8OOMError(const char* location, const v8::OOMDetails& details) {
    ns_v8_fatal_error(location, details.detail ? details.detail : "out of memory");
}

// ES module loader. Node-API has no module surface, so module records are compiled, linked and
// evaluated here against V8 directly. Resolution, source loading (files, the sealed app.nsbundle,
// HTTP dev-server modules, the `ns:module` builtin) and error text come from windows-v8's
// src/esm.rs, which forwards to the classic runtime's loader so both engines resolve a graph the
// same way. Strings returned by the ns_esm_* callbacks are freed with ns_esm_free.
extern "C" char* ns_esm_resolve(const char* spec, const char* referrer);
extern "C" char* ns_esm_read_source(const char* key);
extern "C" char* ns_esm_not_found(const char* spec, const char* referrer, const char* key);
extern "C" int ns_esm_is_volatile(const char* key);
extern "C" void ns_esm_forget(const char* key);
extern "C" void ns_esm_prefetch(const char* const* keys, size_t count);
extern "C" void ns_esm_report_error(const char* report);
extern "C" void ns_esm_free(char* s);

namespace esm {

class RustString {
public:
    explicit RustString(char* p) : p_(p) {}
    ~RustString() {
        if (p_) ns_esm_free(p_);
    }
    RustString(const RustString&) = delete;
    RustString& operator=(const RustString&) = delete;
    explicit operator bool() const { return p_ != nullptr; }
    std::string str() const { return p_ ? std::string(p_) : std::string(); }

private:
    char* p_;
};

// Single-threaded host: one isolate, one registry. Registry key -> module record, plus identity
// hash -> keys to map a referrer back to its key (a hash can collide, so candidates are compared
// by handle).
std::unordered_map<std::string, v8::Global<v8::Module>> g_registry;
std::unordered_multimap<int, std::string> g_hash_to_key;

v8::Local<v8::String> Str(v8::Isolate* iso, const std::string& s) {
    return v8::String::NewFromUtf8(iso, s.data(), v8::NewStringType::kNormal,
                                   static_cast<int>(s.size()))
        .ToLocalChecked();
}

std::string ToStd(v8::Isolate* iso, v8::Local<v8::Value> v) {
    v8::String::Utf8Value utf8(iso, v);
    return *utf8 ? std::string(*utf8, utf8.length()) : std::string();
}

void ThrowError(v8::Isolate* iso, const std::string& msg) {
    iso->ThrowException(v8::Exception::Error(Str(iso, msg)));
}

std::string KeyOf(v8::Isolate* iso, v8::Local<v8::Module> module) {
    auto range = g_hash_to_key.equal_range(module->GetIdentityHash());
    for (auto it = range.first; it != range.second; ++it) {
        auto rec = g_registry.find(it->second);
        if (rec != g_registry.end() && rec->second.Get(iso) == module) return it->second;
    }
    return std::string();
}

std::string Resolve(const std::string& spec, const std::string* referrer) {
    RustString key(ns_esm_resolve(spec.c_str(), referrer ? referrer->c_str() : nullptr));
    return key.str();
}

std::string NotFound(const std::string& spec, const std::string* referrer, const std::string& key) {
    RustString msg(
        ns_esm_not_found(spec.c_str(), referrer ? referrer->c_str() : nullptr, key.c_str()));
    return msg.str();
}

bool Evict(const std::string& key) {
    bool removed = g_registry.erase(key) > 0;
    for (auto it = g_hash_to_key.begin(); it != g_hash_to_key.end();) {
        it = it->second == key ? g_hash_to_key.erase(it) : std::next(it);
    }
    ns_esm_forget(key.c_str());
    return removed;
}

// Message + stack for a thrown/rejected value, with the source location V8 reports for errors
// (a SyntaxError's stack carries none) when the TryCatch has it.
std::string ExceptionReport(v8::Isolate* iso, v8::Local<v8::Context> ctx,
                            v8::Local<v8::Value> exc,
                            v8::Local<v8::Message> message = v8::Local<v8::Message>()) {
    std::string report;
    {
        v8::TryCatch inner(iso);
        v8::Local<v8::Value> stack;
        if (exc->IsObject() &&
            exc.As<v8::Object>()->Get(ctx, Str(iso, "stack")).ToLocal(&stack) &&
            stack->IsString()) {
            report = ToStd(iso, stack);
        }
    }
    if (report.empty()) report = ToStd(iso, exc);
    if (!message.IsEmpty()) {
        std::string resource = ToStd(iso, message->GetScriptResourceName());
        int line = message->GetLineNumber(ctx).FromMaybe(0);
        std::string at = resource + ":" + std::to_string(line);
        if (report.find(at) == std::string::npos) report += "\n    at " + at;
    }
    return report;
}

// Compile the module at `key` and, depth-first, every module it imports that isn't registered
// yet. A SyntaxError leaves its exception pending and stops the walk. An unloadable dependency
// stays unregistered, so linking then throws from ResolveModule with the recorded reason.
bool CompileGraph(v8::Isolate* iso, const std::string& source, const std::string& key) {
    if (g_registry.count(key)) return true;
    v8::ScriptOrigin origin(Str(iso, key), 0, 0, false, -1, v8::Local<v8::Value>(), false, false,
                            true);
    v8::ScriptCompiler::Source compiler_source(Str(iso, source), origin);
    v8::Local<v8::Module> module;
    if (!v8::ScriptCompiler::CompileModule(iso, &compiler_source).ToLocal(&module)) return false;
    // Registered before its dependencies so an import cycle finds it.
    g_registry.emplace(key, v8::Global<v8::Module>(iso, module));
    g_hash_to_key.emplace(module->GetIdentityHash(), key);

    v8::Local<v8::FixedArray> requests = module->GetModuleRequests();
    std::vector<std::string> children;
    for (int i = 0; i < requests->Length(); i++) {
        v8::Local<v8::ModuleRequest> request = requests->Get(i).As<v8::ModuleRequest>();
        std::string child = Resolve(ToStd(iso, request->GetSpecifier()), &key);
        if (!child.empty() && !g_registry.count(child) &&
            std::find(children.begin(), children.end(), child) == children.end()) {
            children.push_back(child);
        }
    }
    // Fetch the ones served over HTTP concurrently, then compile in import order.
    std::vector<const char*> child_keys;
    for (const std::string& child : children) child_keys.push_back(child.c_str());
    if (!child_keys.empty()) ns_esm_prefetch(child_keys.data(), child_keys.size());
    for (const std::string& child : children) {
        // Compiled meanwhile, through a sibling's subtree.
        if (g_registry.count(child)) continue;
        RustString text(ns_esm_read_source(child.c_str()));
        if (text && !CompileGraph(iso, text.str(), child)) return false;
    }
    return true;
}

v8::MaybeLocal<v8::Module> ResolveModule(v8::Local<v8::Context> ctx,
                                         v8::Local<v8::String> specifier,
                                         v8::Local<v8::FixedArray> import_attributes,
                                         v8::Local<v8::Module> referrer) {
    v8::Isolate* iso = v8::Isolate::GetCurrent();
    std::string spec = ToStd(iso, specifier);
    std::string ref = KeyOf(iso, referrer);
    const std::string* ref_ptr = ref.empty() ? nullptr : &ref;
    std::string key = Resolve(spec, ref_ptr);
    auto it = g_registry.find(key);
    if (it != g_registry.end()) return it->second.Get(iso);
    // V8 requires a pending exception when resolution fails.
    ThrowError(iso, NotFound(spec, ref_ptr, key));
    return v8::MaybeLocal<v8::Module>();
}

// `import.meta`: `url` (a file:/// URL, as bundlers' `new URL(x, import.meta.url)` expect),
// `filename` and `dirname`. A served module's identity is its URL alone.
void InitImportMeta(v8::Local<v8::Context> ctx, v8::Local<v8::Module> module,
                    v8::Local<v8::Object> meta) {
    v8::Isolate* iso = v8::Isolate::GetCurrent();
    std::string key = KeyOf(iso, module);
    if (key.empty()) return;
    auto set = [&](const char* name, const std::string& value) {
        (void)meta->CreateDataProperty(ctx, Str(iso, name), Str(iso, value)).FromMaybe(false);
    };
    if (key.rfind("http://", 0) == 0 || key.rfind("https://", 0) == 0 || key == "ns:module") {
        set("url", key);
        return;
    }
    std::string path = key.rfind("\\\\?\\", 0) == 0 ? key.substr(4) : key;
    std::string url = "file:///" + path;
    std::replace(url.begin(), url.end(), '\\', '/');
    size_t sep = path.find_last_of("\\/");
    set("url", url);
    set("filename", path);
    set("dirname", sep == std::string::npos ? std::string() : path.substr(0, sep));
}

// Load (unless registered), link and evaluate the graph rooted at `key`, from `source` when the
// caller already has the root's text. False leaves an exception pending on `tc`.
bool LoadAndEvaluate(v8::Isolate* iso, v8::Local<v8::Context> ctx, v8::TryCatch& tc,
                     const std::string& spec, const std::string* referrer, const std::string& key,
                     const std::string* source, v8::Local<v8::Module>* module_out,
                     v8::Local<v8::Value>* result_out) {
    auto fail = [&](const char* what) {
        if (!tc.HasCaught()) ThrowError(iso, std::string("ESM: failed to ") + what + " " + key);
        return false;
    };
    if (!g_registry.count(key)) {
        std::string text;
        if (source) {
            text = *source;
        } else {
            RustString read(ns_esm_read_source(key.c_str()));
            if (!read) {
                ThrowError(iso, NotFound(spec, referrer, key));
                return false;
            }
            text = read.str();
        }
        if (!CompileGraph(iso, text, key) || !g_registry.count(key)) return fail("compile");
    }
    v8::Local<v8::Module> module = g_registry.find(key)->second.Get(iso);
    if (module->InstantiateModule(ctx, ResolveModule).IsNothing()) return fail("link");
    v8::Local<v8::Value> result;
    if (!module->Evaluate(ctx).ToLocal(&result)) return fail("evaluate");
    *module_out = module;
    *result_out = result;
    return true;
}

void ReportRejection(const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Isolate* iso = info.GetIsolate();
    std::string report = ExceptionReport(iso, iso->GetCurrentContext(), info[0]);
    ns_esm_report_error(report.c_str());
}

// Under top-level await, Evaluate returns a promise and an exception thrown while the graph
// evaluates rejects it rather than reaching a TryCatch: report a settled rejection now and a
// pending graph whenever it rejects.
void ReportEvaluation(v8::Isolate* iso, v8::Local<v8::Context> ctx, v8::Local<v8::Value> result) {
    if (!result->IsPromise()) return;
    v8::Local<v8::Promise> promise = result.As<v8::Promise>();
    if (promise->State() == v8::Promise::kRejected) {
        promise->MarkAsHandled();
        ns_esm_report_error(ExceptionReport(iso, ctx, promise->Result()).c_str());
    } else if (promise->State() == v8::Promise::kPending) {
        v8::Local<v8::Function> on_reject;
        if (v8::Function::New(ctx, ReportRejection).ToLocal(&on_reject)) {
            (void)promise->Catch(ctx, on_reject);
        }
    }
}

void ReturnData(const v8::FunctionCallbackInfo<v8::Value>& info) {
    info.GetReturnValue().Set(info.Data());
}

// Dynamic `import()`: load + link + evaluate the requested graph, then settle with its namespace
// (after the graph's own TLA promise settles), or reject with the real exception.
v8::MaybeLocal<v8::Promise> ImportDynamically(v8::Local<v8::Context> ctx,
                                              v8::Local<v8::Data> host_defined_options,
                                              v8::Local<v8::Value> resource_name,
                                              v8::Local<v8::String> specifier,
                                              v8::Local<v8::FixedArray> import_attributes) {
    v8::Isolate* iso = v8::Isolate::GetCurrent();
    v8::EscapableHandleScope scope(iso);
    v8::Local<v8::Promise::Resolver> resolver;
    if (!v8::Promise::Resolver::New(ctx).ToLocal(&resolver)) return v8::MaybeLocal<v8::Promise>();

    std::string spec = ToStd(iso, specifier);
    std::string referrer = resource_name->IsString() ? ToStd(iso, resource_name) : std::string();
    const std::string* ref_ptr = referrer.empty() ? nullptr : &referrer;
    std::string key = Resolve(spec, ref_ptr);
    // Volatile URLs (configureLoader) are re-fetched on every import.
    if (ns_esm_is_volatile(key.c_str())) Evict(key);

    v8::Local<v8::Module> module;
    v8::Local<v8::Value> result;
    bool ok;
    v8::Local<v8::Value> exception;
    {
        v8::TryCatch tc(iso);
        ok = LoadAndEvaluate(iso, ctx, tc, spec, ref_ptr, key, nullptr, &module, &result);
        if (!ok) exception = tc.Exception();
    }
    if (!ok) {
        if (exception.IsEmpty()) exception = v8::Exception::Error(Str(iso, "ESM: failed to load " + key));
        (void)resolver->Reject(ctx, exception);
        return scope.Escape(resolver->GetPromise());
    }
    v8::Local<v8::Value> ns = module->GetModuleNamespace();
    v8::Local<v8::Promise> evaluation =
        result->IsPromise() ? result.As<v8::Promise>() : v8::Local<v8::Promise>();
    if (!evaluation.IsEmpty() && evaluation->State() == v8::Promise::kRejected) {
        (void)resolver->Reject(ctx, evaluation->Result());
    } else if (!evaluation.IsEmpty() && evaluation->State() == v8::Promise::kPending) {
        v8::Local<v8::Function> to_ns;
        v8::Local<v8::Promise> chained;
        if (v8::Function::New(ctx, ReturnData, ns).ToLocal(&to_ns) &&
            evaluation->Then(ctx, to_ns).ToLocal(&chained)) {
            (void)resolver->Resolve(ctx, chained);
        } else {
            (void)resolver->Resolve(ctx, ns);
        }
    } else {
        (void)resolver->Resolve(ctx, ns);
    }
    return scope.Escape(resolver->GetPromise());
}

} // namespace esm

extern "C" {

// The V8 platform + V8::Initialize are done from Rust via the `v8` crate (rusty_v8), because
// NewDefaultPlatform's std::unique_ptr<Platform> return can't be linked from this MSVC-STL shim
// (rusty_v8 uses V8's bundled libc++). V8 must be initialized before this is called.
napi_status js_create_runtime(napi_runtime__** runtime) {
    if (!runtime) return napi_invalid_arg;
    auto* rt = new napi_runtime__();
    rt->allocator = new tns::SimpleAllocator();
    v8::Isolate::CreateParams params;
    params.array_buffer_allocator = rt->allocator;
    rt->isolate = v8::Isolate::New(params);
    rt->isolate->SetFatalErrorHandler(&OnV8FatalError);
    rt->isolate->SetOOMErrorHandler(&OnV8OOMError);
    rt->isolate->SetHostImportModuleDynamicallyCallback(&esm::ImportDynamically);
    rt->isolate->SetHostInitializeImportMetaObjectCallback(&esm::InitImportMeta);
    *runtime = rt;
    return napi_ok;
}

napi_status js_create_napi_env(napi_env* env, napi_runtime__* rt) {
    if (!env || !rt) return napi_invalid_arg;
    v8::Isolate* isolate = rt->isolate;
    isolate->Enter();
    {
        // Stack handle scope just to create the context; the context is then persisted inside
        // napi_env__ (context_persistent) and kept current via Enter(), so it outlives this scope.
        v8::HandleScope handle_scope(isolate);
        v8::Local<v8::Context> context = v8::Context::New(isolate);
        context->Enter();
        *env = new napi_env__(context, NAPI_VERSION_EXPERIMENTAL);
    }
    // Open a long-lived napi handle scope (heap-backed HandleScopeWrapper: HandleScope itself is
    // stack-only) so napi calls from the Rust host can allocate handles. Kept open for process life.
    napi_handle_scope scope = nullptr;
    napi_open_handle_scope(*env, &scope);
    return napi_ok;
}

napi_status js_execute_script(napi_env env,
                              napi_value script,
                              const char* file,
                              napi_value* result) {
    return napi_run_script_source(env, script, file, result);
}

napi_status js_execute_pending_jobs(napi_env env) {
    env->isolate->PerformMicrotaskCheckpoint();
    return napi_ok;
}

// Run `source` as the entry ES module registered under `key`. Failures (compile, link, a throw
// during evaluation, a rejected top-level await) are reported through ns_esm_report_error.
napi_status js_run_module(napi_env env, const char* source, const char* key) {
    if (!env || !source || !key) return napi_invalid_arg;
    v8::Isolate* iso = env->isolate;
    v8::HandleScope handle_scope(iso);
    v8::Local<v8::Context> ctx = env->context();
    v8::Context::Scope context_scope(ctx);
    v8::TryCatch tc(iso);
    std::string entry(key);
    std::string text(source);
    v8::Local<v8::Module> module;
    v8::Local<v8::Value> result;
    if (!esm::LoadAndEvaluate(iso, ctx, tc, entry, nullptr, entry, &text, &module, &result)) {
        std::string report = tc.HasCaught()
                                 ? esm::ExceptionReport(iso, ctx, tc.Exception(), tc.Message())
                                 : "ESM: failed to run " + entry;
        ns_esm_report_error(report.c_str());
        return napi_generic_failure;
    }
    iso->PerformMicrotaskCheckpoint();
    esm::ReportEvaluation(iso, ctx, result);
    return napi_ok;
}

// Drop a module record so the next import compiles (and, over HTTP, fetches) it anew. Importers
// already linked keep the old record. Returns 1 when a record was removed.
int js_esm_evict(const char* key) {
    return key && esm::Evict(key) ? 1 : 0;
}

// Calls `visit(key, user)` for every registered module.
void js_esm_for_each_key(void (*visit)(const char* key, void* user), void* user) {
    for (const auto& entry : esm::g_registry) visit(entry.first.c_str(), user);
}

} // extern "C"
