// The dispatch hot paths are dominated by small short-lived allocations, where
// mimalloc beats the Windows heap.
#[global_allocator]
static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod class_helpers;
pub mod esm_http;
pub(crate) mod dotnet;
mod error;
mod ffi;
#[cfg(feature = "classic")]
mod generic_method_call;
#[cfg(feature = "classic")]
mod global_fns;
mod globals;
mod helpers;
#[cfg(feature = "classic")]
mod hmr_support;
#[cfg(feature = "classic")]
pub mod inspector;
#[cfg(feature = "classic")]
mod interop;
mod js_observable_vector;
#[cfg(feature = "classic")]
mod livesync;
#[cfg(feature = "classic")]
mod message_port;
mod method_call;
#[cfg(feature = "classic")]
mod name_space;
#[cfg(feature = "napi_engine")]
pub mod napi_engine;
#[cfg(feature = "classic")]
mod ns_proxy;
mod property_call;
#[cfg(feature = "classic")]
mod proxy_manifest_loader;
pub mod source_protect;
#[cfg(feature = "classic")]
pub mod timers;
#[cfg(feature = "classic")]
mod type_description;
pub mod ui_dispatcher;
mod value;
#[cfg(feature = "classic")]
pub(crate) mod win32;
#[cfg(feature = "classic")]
pub(crate) mod win32_fast;
#[cfg(feature = "classic")]
pub(crate) mod win32_known_fns;
mod websocket;
mod winhttp;
#[cfg(feature = "classic")]
mod worker_support;
#[cfg(feature = "classic")]
mod worker_threads;

#[cfg(feature = "classic")]
use crate::ns_proxy::CallbackThisObject;
#[cfg(feature = "classic")]
use crate::proxy_manifest_loader::SbgManifestLoader;
#[cfg(feature = "classic")]
use crate::value::{
    ffi_parse_bool_arg, ffi_parse_buffer_arg, ffi_parse_f32_arg, ffi_parse_f64_arg,
    ffi_parse_function_arg, ffi_parse_i16_arg, ffi_parse_i32_arg, ffi_parse_i64_arg,
    ffi_parse_i8_arg, ffi_parse_isize_arg, ffi_parse_pointer_arg, ffi_parse_string_arg,
    ffi_parse_struct_arg, ffi_parse_u16_arg, ffi_parse_u32_arg, ffi_parse_u64_arg,
    ffi_parse_u8_arg, ffi_parse_usize_arg, read_value_from_ptr, set_ret_val, MAX_SAFE_INTEGER,
    MIN_SAFE_INTEGER,
};
use crate::value::NativeType;
#[cfg(feature = "classic")]
use crate::value::NativeValue;
use ahash::AHashSet;
#[cfg(feature = "classic")]
use ahash::{AHashMap, AHasher};
#[cfg(feature = "classic")]
use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
#[cfg(feature = "classic")]
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::{Declaration, DeclarationKind};
use metadata::declarations::delegate_declaration::generic_delegate_declaration::GenericDelegateDeclaration;
use metadata::declarations::delegate_declaration::generic_delegate_instance_declaration::GenericDelegateInstanceDeclaration;
use metadata::declarations::delegate_declaration::DelegateDeclaration;
use metadata::declarations::delegate_declaration::DelegateDeclarationImpl;
#[cfg(feature = "classic")]
use metadata::declarations::enum_declaration::EnumDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::event_declaration::EventDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::namespace_declaration::NamespaceDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::property_declaration::PropertyDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::struct_declaration::StructDeclaration;
use metadata::generic_instance_id_builder::GenericInstanceIdBuilder;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;
#[cfg(feature = "classic")]
use metadata::value::Value;
use parking_lot::lock_api::{MappedRwLockReadGuard, RwLockReadGuard};
#[cfg(feature = "classic")]
use parking_lot::lock_api::{MappedRwLockWriteGuard, RwLockWriteGuard};
use parking_lot::{RawRwLock, RwLock};
#[cfg(feature = "classic")]
use parking_lot::Mutex;
#[cfg(feature = "classic")]
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs;
#[cfg(feature = "classic")]
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering as AtomicOrdering};
#[cfg(feature = "classic")]
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, OnceLock};
#[cfg(feature = "classic")]
use std::sync::Once;
#[cfg(feature = "classic")]
use v8::{FunctionTemplate, Local};
use windows::core::{IUnknown, Interface, GUID, HSTRING, PCWSTR};
#[cfg(feature = "classic")]
use windows::core::{Error, HRESULT};
#[cfg(feature = "classic")]
use windows::core::IInspectable;
#[cfg(feature = "classic")]
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::WinRT::RoGetActivationFactory;
#[cfg(feature = "classic")]
use windows::Win32::System::WinRT::{
    IActivationFactory, RoInitialize, RoUninitialize, RO_INIT_SINGLETHREADED,
};
#[cfg(feature = "classic")]
use windows::Win32::UI::Shell::IInitializeWithWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

#[cfg(feature = "classic")]
thread_local!(static ISOLATE: RefCell<Option<&'static mut v8::Isolate>> = RefCell::new(None));

// Raw pointer to the V8 isolate, set once during Runtime::new so that
// JS delegate Invoke trampolines can enter V8 without a scope on the stack.
#[cfg(feature = "classic")]
thread_local!(pub(crate) static DELEGATE_ISOLATE_PTR: Cell<*mut v8::Isolate> = Cell::new(std::ptr::null_mut()));
// Re-entrancy depth for JsDelegate::Invoke. When > 0 a delegate is firing while we're
// already inside a V8 scope (e.g. XAML re-entering ContainerContentChanging), so we must
// adopt the active scope instead of pushing a new root HandleScope from the raw isolate.
thread_local!(pub(crate) static DELEGATE_DEPTH: Cell<u32> = Cell::new(0));
// Set while a coalesced microtask drain is queued on the XAML DispatcherQueue,
// so the native→JS callbacks firing within one dispatcher pass schedule at most
// one drain work item between them. See `defer_microtask_drain`.
thread_local!(pub(crate) static MICROTASK_DRAIN_QUEUED: Cell<bool> = Cell::new(false));

// JS functions registered via NSWinRT.asDelegate so managed .NET delegates can
// call back into V8. Keyed by the integer id sent to C# as the callback id.
// Thread-local because V8 globals must be accessed on the isolate's thread.
#[cfg(feature = "classic")]
thread_local!(pub(crate) static DOTNET_JS_CALLBACKS: RefCell<HashMap<i32, v8::Global<v8::Function>>> = RefCell::new(HashMap::new()));
pub(crate) static DOTNET_NEXT_CB_ID: AtomicI32 = AtomicI32::new(1);
// JS callbacks that should be removed after a single invocation (oneshot).
thread_local!(pub(crate) static DOTNET_ONESHOT_JS_CALLBACKS: RefCell<HashSet<i32>> = RefCell::new(HashSet::new()));

// Optional hook called from the async-wait message loop so external tools
// (e.g. the devtools server) can pump their own messages without the runtime
// needing to depend on those crates directly.
thread_local!(pub static ASYNC_PUMP_HOOK: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None));

// Native ESM module registry: resolved absolute path → compiled V8 Module handle.
// Pre-populated by `compile_module_graph` before `instantiate_module` is called.
#[cfg(feature = "classic")]
thread_local!(static ESM_MODULE_REGISTRY: RefCell<HashMap<String, v8::Global<v8::Module>>> = RefCell::new(HashMap::new()));

// Maps a V8 Module identity hash (i32) to its resolved absolute path.
// Used by `resolve_module_callback` to locate the referrer's directory for relative imports.
thread_local!(static ESM_HASH_TO_PATH: RefCell<HashMap<i32, String>> = RefCell::new(HashMap::new()));

// Tracks constructors currently being built on this thread to avoid
// re-entrant template/property mutations that can corrupt V8 descriptor
// arrays when a constructor build recursively triggers building the same
// constructor (observed as a V8 internal DescriptorArray append failure).
thread_local!(static CREATING_CTORS: RefCell<AHashSet<String>> = RefCell::new(AHashSet::new()));

// Stores the most recent JS error (message + stack trace) captured during
// script execution or V8 callbacks. Retrieved by `get_last_js_error()`.
thread_local!(pub static LAST_JS_ERROR: RefCell<Option<String>> = RefCell::new(None));

/// Store a JS error string so it can be retrieved via the FFI.
pub fn store_last_js_error(error: String) {
    LAST_JS_ERROR.with(|e| {
        *e.borrow_mut() = Some(error);
    });
}

/// Flush any V8 microtasks (Promise continuations) that were queued by
/// `JsDelegate::Invoke` callbacks fired from XAML's own event loop between
/// frames.
///
/// XAML dispatches `Completed` notifications for async operations (e.g.
/// `BitmapImage.SetSourceAsync`) via `CoreDispatcher`/`CoreWindow` between
/// render frames. When those fire, `JsDelegate::Invoke` calls into V8 and
/// queues a microtask (Promise resolution). The isolate runs with explicit
/// microtasks policy (see `Runtime::new`), so nothing drains automatically:
/// this function and the per-callback checkpoints in the native→JS entry
/// points are the only drain sites. It is the target of the coalesced drain
/// scheduled by `defer_microtask_drain`, and is also called once per frame /
/// heartbeat via `runtime_pump_timers` so continuations never stall.
///
/// NOTE: Do NOT call `PeekMessageW + DispatchMessageW` here. This function is
/// called from `CompositionTarget.Rendering` via `runtime_pump_timers`, and
/// pumping Win32 messages inside a XAML rendering callback causes reentrancy
/// in XAML's internal rendering state machine.
#[cfg(feature = "classic")]
pub fn pump_dispatcher() {
    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return;
    }
    let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
    v8::scope!(scope, isolate);
    let ctx = match scope.get_slot::<v8::Global<v8::Context>>() {
        Some(g) => g.clone(),
        None => return,
    };
    let ctx = v8::Local::new(scope, &ctx);
    let scope = &mut v8::ContextScope::new(scope, ctx);
    v8::tc_scope!(tc, scope);
    tc.perform_microtask_checkpoint();
}

/// Move the post-callback microtask checkpoint out of the current native frame
/// when that frame may be a re-entrancy-sensitive XAML callout.
///
/// JS delegates frequently fire from inside XAML's render walk: a JS handler on
/// `CompositionTarget.Rendering`, or a re-entrant raise like
/// `ContainerContentChanging`. Draining Promise continuations there lets
/// arbitrary JS mutate the live XAML tree mid-walk, which trips XAML's
/// re-entrancy guard and fail-fasts the process with a stowed exception
/// (0xC000027B): the host-side half of this contract is documented at
/// App.xaml.cs `SchedulePump`. On the XAML UI thread the checkpoint is therefore
/// queued as an ordinary `DispatcherQueue` work item (coalesced: at most one in
/// flight), which the dispatcher runs only after the render walk completes.
///
/// Returns `true` when a drain is queued (or already pending) and the caller
/// must skip its inline checkpoint. Returns `false` when this thread has no
/// XAML dispatcher: console hosts, workers and tests keep the inline
/// checkpoint, where no render walk exists and prompt draining is preferable.
#[cfg(feature = "classic")]
pub(crate) fn defer_microtask_drain() -> bool {
    MICROTASK_DRAIN_QUEUED.with(|queued| {
        if queued.get() {
            return true;
        }
        let scheduled = ui_dispatcher::defer_on_ui_thread(|| {
            // Drain before clearing the flag: microtasks queued while the
            // checkpoint runs are processed by the same checkpoint loop, so
            // clearing afterwards cannot strand work. catch_unwind keeps a JS
            // panic from unwinding into the COM dispatcher frames above us.
            let result = std::panic::catch_unwind(pump_dispatcher);
            MICROTASK_DRAIN_QUEUED.with(|q| q.set(false));
            if result.is_err() {
                store_last_js_error("panic while draining deferred microtasks".to_string());
            }
        });
        if scheduled {
            queued.set(true);
        }
        scheduled
    })
}

/// Pump Win32 messages and, on the classic engine, flush V8 microtasks (napi hosts drain their
/// own engine's queue around this call).
///
/// Safe to call from a console app's own event loop (no XAML renderer active).
/// Do NOT call this from `CompositionTarget.Rendering`: use `pump_dispatcher()`
/// there instead, which skips `PeekMessageW` to avoid XAML reentrancy.
///
/// Returns `true` if at least one Win32 message was dispatched.
pub fn pump_messages() -> bool {
    let mut msg = MSG::default();
    let mut dispatched = false;
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
            dispatched = true;
        }
    }
    #[cfg(feature = "classic")]
    pump_dispatcher();
    dispatched
}

/// Retrieve (and clear) the last stored JS error.
pub fn get_last_js_error() -> Option<String> {
    LAST_JS_ERROR.with(|e| e.borrow_mut().take())
}

/// Test helper: call Windows.Data.Json.JsonValue::CreateStringValue via the
/// typed `windows` crate and return the raw (leaked) IInspectable pointer.
pub fn diag_direct_create_string_value(s: &str) -> *mut std::ffi::c_void {
    use windows::Data::Json::JsonValue;
    let h = HSTRING::from(s);
    match JsonValue::CreateStringValue(&h) {
        Ok(jv) => {
            let raw = jv.as_raw();
            std::mem::forget(jv);
            raw as *mut std::ffi::c_void
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Diagnostic helper: call Windows.Data.Json.JsonValue::CreateStringValue
/// via the runtime's libffi preparation path and return the created string.
pub fn diag_libffi_create_string_value_via_runtime(s: &str) -> Option<String> {
    use crate::value::{NativeType, NativeValue};
    use libffi::middle::{Cif, CodePtr, Type};
    use std::mem::ManuallyDrop;
    use windows::core::IUnknown;
    use windows::Data::Json::{IJsonValue, IJsonValueStatics};
    use windows::Win32::System::WinRT::{
        RoGetActivationFactory, RoInitialize, RO_INIT_MULTITHREADED,
    };

    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);

        let class_name: HSTRING = HSTRING::from("Windows.Data.Json.JsonValue");
        let statics = RoGetActivationFactory::<IJsonValueStatics>(&class_name).ok()?;
        let statics_ptr: *mut c_void = statics.as_raw() as *mut c_void;
        let vtable_ptr_ptr: *mut *mut c_void = std::mem::transmute(statics_ptr);
        let vtable_ptr = *vtable_ptr_ptr as *mut *mut c_void;

        // CreateStringValue is at vtable slot 10 for IJsonValueStatics.
        let create_str_off = 10isize;
        let func_ptr = *vtable_ptr.offset(create_str_off) as *const c_void;

        // Build argument buffer matching the runtime calling convention.
        let mut argument_buf: Vec<NativeValue> = Vec::new();
        let mut argument_parse_types: Vec<Option<NativeType>> = Vec::new();

        // `this` pointer
        argument_buf.push(NativeValue {
            pointer: statics.as_raw() as *mut c_void,
        });
        argument_parse_types.push(None);

        // HSTRING argument (stored as ManuallyDrop inside NativeValue)
        let h = HSTRING::from(s);
        argument_buf.push(NativeValue {
            string: ManuallyDrop::new(h.clone()),
        });
        argument_parse_types.push(Some(NativeType::String));

        // out-param for result
        let mut result: *mut c_void = std::ptr::null_mut();
        argument_buf.push(NativeValue {
            pointer: &mut result as *mut _ as *mut c_void,
        });
        argument_parse_types.push(None);

        let parameter_types = vec![NativeType::Pointer, NativeType::String, NativeType::Pointer];

        // Use runtime helpers to prepare stable HSTRING storage and build args.
        let prep = match crate::ffi::prepare_string_storage(
            &argument_buf,
            &parameter_types,
            &argument_parse_types,
        ) {
            Ok(p) => p,
            Err(_) => return None,
        };

        let call_args = crate::ffi::build_call_args(&prep, &argument_buf, &parameter_types);

        let cif = Cif::new(
            vec![Type::usize(), Type::usize(), Type::usize()],
            Type::i32(),
        );

        // Perform the libffi call.
        let _ret: i32 = cif.call(CodePtr::from_ptr(func_ptr), &call_args);

        if result.is_null() {
            return None;
        }

        // Convert result pointer to IJsonValue and read the string.
        let unknown = IUnknown::from_raw(result);
        let ijv: IJsonValue = unknown.cast::<IJsonValue>().ok()?;
        let h_res = ijv.GetString().ok()?;
        Some(h_res.to_string())
    }
}

#[cfg(feature = "classic")]
pub struct Runtime {
    isolate: v8::OwnedIsolate,
    global_context: v8::Global<v8::Context>,
    app_root: String,
    winrt_initialized: bool,
}

#[cfg(feature = "classic")]
static INIT: Once = Once::new();
#[cfg(feature = "classic")]
static PROXY_MANIFESTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static LOG_TO_CONSOLE: OnceLock<AtomicBool> = OnceLock::new();
/// Directory for the runtime trace log (`console.log`). Set by the host via
/// `runtime_set_local_folder` to the app's LocalState folder so the trace log sits next to the
/// crash/panic logs (and the CLI can tail a deterministic path). Falls back to the
/// process temp dir when unset (e.g. console/unpackaged hosts).
static LOG_DIR: OnceLock<String> = OnceLock::new();

// COM identity → JS wrapper object cache. Keyed on the canonical IUnknown pointer
// (obtained via QueryInterface(IID_IUnknown)), so the same underlying COM object
// always maps to the same JS proxy.
#[cfg(feature = "classic")]
thread_local!(pub(crate) static INSTANCE_CACHE: RefCell<HashMap<usize, v8::Weak<v8::Object>>> = RefCell::new(HashMap::new()));

/// When the cache exceeds this size, request an incremental GC so that weak
/// finalizers can drain dead entries.
#[cfg(feature = "classic")]
pub(crate) const INSTANCE_CACHE_GC_THRESHOLD: usize = 512;

// Next cache size at which to deliver a GC nudge. Doubles after each nudge:
// a fixed threshold oscillates in allocation loops (nudge → GC prunes below
// threshold → next insert re-nudges), serializing every creation behind
// incremental-marking work.
#[cfg(feature = "classic")]
thread_local!(pub(crate) static GC_NUDGE_NEXT_AT: std::cell::Cell<usize> = std::cell::Cell::new(INSTANCE_CACHE_GC_THRESHOLD));

// IActivationFactory cache: RoGetActivationFactory is expensive (COM broker round-trip) but factories
// are app-lifetime singletons: same class name always returns the same factory pointer.
// Keyed on the WinRT class full name (e.g. "Microsoft.UI.Xaml.Controls.TextBlock").
thread_local!(static ACTIVATION_FACTORY_CACHE: RefCell<HashMap<String, IUnknown>> = RefCell::new(HashMap::new()));

#[cfg(feature = "classic")]
pub(crate) struct EventRegistration {
    pub(crate) token: i64,
    pub(crate) handler: v8::Global<v8::Value>,
}

// Keyed on COM identity, not the JS proxy: if a proxy is GC'd and the same native
// object is re-wrapped, the old token must still be findable to avoid double-fire.
#[cfg(feature = "classic")]
thread_local!(pub(crate) static EVENT_REGISTRY: RefCell<HashMap<usize, HashMap<String, EventRegistration>>> = RefCell::new(HashMap::new()));

pub(crate) fn com_identity(unk: &IUnknown) -> Option<usize> {
    unk.cast::<IUnknown>().ok().map(|id| id.as_raw() as usize)
}

/// Called after inserting into the cache; re-arms once the cache genuinely
/// shrinks back under the base threshold.
#[cfg(feature = "classic")]
#[inline]
pub(crate) fn maybe_request_gc_nudge(cache_size: usize, isolate: &mut v8::Isolate) {
    GC_NUDGE_NEXT_AT.with(|next| {
        let at = next.get();
        if cache_size >= at {
            // Each nudge is a full collection. Under churn the cache refills to the threshold
            // every few milliseconds (and the shrink branch below re-arms it), so an unthrottled
            // nudge ran a full GC per few hundred wraps. Short-lived wrappers die in ordinary
            // scavenges anyway; the nudge only backstops COM memory V8 can't see, so a time
            // bound is enough: with a high-water override that keeps growth bounded.
            let now = std::time::Instant::now();
            let due = GC_NUDGE_LAST.with(|last| {
                last.get()
                    .map_or(true, |t| now.duration_since(t) >= GC_NUDGE_MIN_INTERVAL)
            });
            if due || cache_size >= at.saturating_mul(8) {
                isolate.memory_pressure_notification(v8::MemoryPressureLevel::Moderate);
                GC_NUDGE_LAST.with(|last| last.set(Some(now)));
                next.set(
                    cache_size
                        .saturating_mul(2)
                        .max(INSTANCE_CACHE_GC_THRESHOLD),
                );
            }
        } else if cache_size < INSTANCE_CACHE_GC_THRESHOLD {
            next.set(INSTANCE_CACHE_GC_THRESHOLD);
        }
    });
}

#[cfg(feature = "classic")]
const GC_NUDGE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

thread_local!(static GC_NUDGE_LAST: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) });

#[cfg(feature = "classic")]
pub(crate) fn proxy_manifests() -> &'static Mutex<Vec<String>> {
    PROXY_MANIFESTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Tracks hashes of already-loaded manifests to avoid O(N×size) string comparison.
#[cfg(feature = "classic")]
static MANIFEST_HASHES: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();

#[cfg(feature = "classic")]
pub(crate) fn manifest_hashes() -> &'static Mutex<HashSet<u64>> {
    MANIFEST_HASHES.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(feature = "classic")]
pub(crate) fn content_hash(s: &str) -> u64 {
    let mut h = AHasher::default();
    s.hash(&mut h);
    h.finish()
}

#[cfg(feature = "classic")]
fn default_sbg_manifest_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("SBG_MANIFEST_PATH") {
        return PathBuf::from(explicit);
    }
    if let Ok(out_dir) = std::env::var("SBG_OUTPUT_DIR") {
        return PathBuf::from(out_dir).join("sbg-manifest.json");
    }
    PathBuf::from("obj")
        .join("_ns_")
        .join("gen")
        .join("sbg-manifest.json")
}

#[cfg(feature = "classic")]
fn preload_sbg_manifest() {
    let manifest_path = default_sbg_manifest_path();
    if !manifest_path.exists() {
        return;
    }

    // Read once; feed the same string to both the loader and the dedup check.
    let Ok(content) = fs::read_to_string(&manifest_path) else {
        return;
    };

    let hash = content_hash(&content);
    {
        let mut hashes = manifest_hashes().lock();
        if !hashes.insert(hash) {
            return; // already loaded
        }
    }

    let mut loader = SbgManifestLoader::new();
    if loader.load_manifest_json(&content).is_ok() {
        proxy_manifests().lock().push(content);
    }
}

#[cfg(feature = "classic")]
fn split_type_name(type_name: &str) -> (Option<String>, String) {
    match type_name.rsplit_once('.') {
        Some((namespace, class_name)) => (Some(namespace.to_string()), class_name.to_string()),
        None => (None, type_name.to_string()),
    }
}

#[cfg(feature = "classic")]
fn extend_class_methods(
    class_declaration: &ClassDeclaration,
    methods: &mut Vec<MethodDeclaration>,
    seen: &mut HashSet<String>,
) {
    // Use contains() first so we only allocate a String when the method is new.
    // HashSet<String> supports Borrow<str>, so contains(&str) does not allocate.
    for method in class_declaration.methods() {
        let key = if !method.overload_name().is_empty() {
            method.overload_name()
        } else {
            method.name()
        };
        if !seen.contains(key) {
            seen.insert(key.to_string());
            methods.push(method.clone());
        }
    }

    if let Some(default_interface) = class_declaration.default_interface() {
        for method in default_interface.methods() {
            let key = if !method.overload_name().is_empty() {
                method.overload_name()
            } else {
                method.name()
            };
            if !seen.contains(key) {
                seen.insert(key.to_string());
                methods.push(method.clone());
            }
        }
    }

    for interface in class_declaration.implemented_interfaces() {
        for method in interface.methods() {
            let key = if !method.overload_name().is_empty() {
                method.overload_name()
            } else {
                method.name()
            };
            if !seen.contains(key) {
                seen.insert(key.to_string());
                methods.push(method.clone());
            }
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                extend_class_methods(base_class, methods, seen);
            }
        }
    }
}

#[cfg(feature = "classic")]
fn extend_class_properties(
    class_declaration: &ClassDeclaration,
    properties: &mut Vec<PropertyDeclaration>,
    seen: &mut HashSet<String>,
) {
    for property in class_declaration.properties() {
        let key = property.name();
        if !seen.contains(key) {
            seen.insert(key.to_string());
            properties.push(property.clone());
        }
    }

    if let Some(default_interface) = class_declaration.default_interface() {
        for property in default_interface.properties() {
            let key = property.name();
            if !seen.contains(key) {
                seen.insert(key.to_string());
                properties.push(property.clone());
            }
        }
    }

    for interface in class_declaration.implemented_interfaces() {
        for property in interface.properties() {
            let key = property.name();
            if !seen.contains(key) {
                seen.insert(key.to_string());
                properties.push(property.clone());
            }
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                extend_class_properties(base_class, properties, seen);
            }
        }
    }
}

#[cfg(feature = "classic")]
fn collect_class_methods(class_declaration: &ClassDeclaration) -> Vec<MethodDeclaration> {
    let mut methods = Vec::new();
    let mut seen = HashSet::new();
    extend_class_methods(class_declaration, &mut methods, &mut seen);
    methods
}

#[cfg(feature = "classic")]
fn collect_class_properties(class_declaration: &ClassDeclaration) -> Vec<PropertyDeclaration> {
    let mut properties = Vec::new();
    let mut seen = HashSet::new();
    extend_class_properties(class_declaration, &mut properties, &mut seen);
    properties
}

#[cfg(feature = "classic")]
struct ClassMembers {
    properties: AHashMap<String, PropertyDeclaration>,
    /// Keyed by overload name when present, plain name otherwise.
    methods: AHashMap<String, MethodDeclaration>,
}

// Per-thread because `PropertyDeclaration` / `MethodDeclaration` carry raw
// WinMD pointers that aren't `Send`. UWP runs single-threaded, so this is
// effectively a global cache.
#[cfg(feature = "classic")]
thread_local!(static CLASS_MEMBERS_CACHE: RefCell<AHashMap<String, ClassMembers>> = RefCell::new(AHashMap::new()));

#[cfg(feature = "classic")]
fn fill_class_members(
    class_declaration: &ClassDeclaration,
    properties: &mut AHashMap<String, PropertyDeclaration>,
    methods: &mut AHashMap<String, MethodDeclaration>,
) {
    // Use contains_key() before inserting to avoid allocating the String key on every
    // call when the entry already exists; String-keyed maps can borrow &str for
    // contains_key / get, so the lookup is allocation-free on the common hit path.
    let mut absorb_props = |list: &[PropertyDeclaration]| {
        for p in list {
            let key = p.name();
            if !properties.contains_key(key) {
                properties.insert(key.to_string(), p.clone());
            }
        }
    };
    let mut absorb_methods = |list: &[MethodDeclaration]| {
        for m in list {
            let key = if !m.overload_name().is_empty() {
                m.overload_name()
            } else {
                m.name()
            };
            if !methods.contains_key(key) {
                methods.insert(key.to_string(), m.clone());
            }
        }
    };
    absorb_props(class_declaration.properties());
    absorb_methods(class_declaration.methods());
    if let Some(di) = class_declaration.default_interface() {
        absorb_props(di.properties());
        absorb_methods(di.methods());
    }
    for iface in class_declaration.implemented_interfaces() {
        absorb_props(iface.properties());
        absorb_methods(iface.methods());
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                fill_class_members(base, properties, methods);
            }
        }
    }
}

#[cfg(feature = "classic")]
fn with_class_members<R>(
    class_declaration: &ClassDeclaration,
    f: impl FnOnce(&ClassMembers) -> R,
) -> R {
    CLASS_MEMBERS_CACHE.with(|cache| {
        let full_name = class_declaration.full_name();

        // Fast path: read with a shared borrow: no String allocation for the key.
        // String-keyed maps accept &str via the Borrow<str> blanket impl.
        {
            let borrow = cache.borrow();
            if let Some(entry) = borrow.get(full_name) {
                return f(entry);
            }
        }

        // Cache miss: build the member maps, then insert.
        // fill_class_members calls itself recursively but never re-enters
        // with_class_members, so the RefCell is free to borrow_mut here.
        let mut properties = AHashMap::new();
        let mut methods = AHashMap::new();
        fill_class_members(class_declaration, &mut properties, &mut methods);
        let mut borrow = cache.borrow_mut();
        let entry = borrow.entry(full_name.to_string()).or_insert(ClassMembers {
            properties,
            methods,
        });
        f(entry)
    })
}

#[cfg(feature = "classic")]
fn find_class_property(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<PropertyDeclaration> {
    with_class_members(class_declaration, |m| m.properties.get(name).cloned())
}

#[cfg(feature = "classic")]
fn find_class_method(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<MethodDeclaration> {
    with_class_members(class_declaration, |m| m.methods.get(name).cloned())
}

#[cfg(feature = "classic")]
fn class_method_matches(class_declaration: &ClassDeclaration, name: &str) -> bool {
    let method_match = |m: &MethodDeclaration| {
        let on = m.overload_name();
        (!on.is_empty() && on == name) || m.name() == name
    };

    if class_declaration.methods().iter().any(method_match) {
        return true;
    }

    if let Some(di) = class_declaration.default_interface() {
        if di.methods().iter().any(method_match) {
            return true;
        }
    }

    for iface in class_declaration.implemented_interfaces() {
        if iface.methods().iter().any(method_match) {
            return true;
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                return class_method_matches(base, name);
            }
        }
    }
    false
}

#[cfg(feature = "classic")]
fn class_property_matches(class_declaration: &ClassDeclaration, name: &str) -> bool {
    if class_declaration
        .properties()
        .iter()
        .any(|p| p.name() == name)
    {
        return true;
    }

    if let Some(di) = class_declaration.default_interface() {
        if di.properties().iter().any(|p| p.name() == name) {
            return true;
        }
    }

    for iface in class_declaration.implemented_interfaces() {
        if iface.properties().iter().any(|p| p.name() == name) {
            return true;
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                return class_property_matches(base, name);
            }
        }
    }
    false
}

#[cfg(feature = "classic")]
fn class_has_member_named(class_declaration: &ClassDeclaration, name: &str) -> bool {
    class_method_matches(class_declaration, name) || class_property_matches(class_declaration, name)
}

#[cfg(feature = "classic")]
fn find_event_methods(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<(MethodDeclaration, MethodDeclaration)> {
    let check = |events: &[EventDeclaration]| -> Option<(MethodDeclaration, MethodDeclaration)> {
        events
            .iter()
            .find(|e| e.name() == name)
            .map(|e| (e.add_method().clone(), e.remove_method().clone()))
    };
    if let Some(m) = check(class_declaration.events()) {
        return Some(m);
    }
    if let Some(di) = class_declaration.default_interface() {
        if let Some(m) = check(di.events()) {
            return Some(m);
        }
    }
    for iface in class_declaration.implemented_interfaces() {
        if let Some(m) = check(iface.events()) {
            return Some(m);
        }
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                return find_event_methods(base, name);
            }
        }
    }
    None
}

// The payloads are read by the classic engine's return conversion; the napi engines convert
// from the raw result and only match on the variant.
#[cfg_attr(not(feature = "classic"), allow(dead_code))]
#[derive(Clone)]
pub(crate) enum ReturnKind {
    Void,
    Primitive(crate::value::NativeType),
    /// GUID uses value-type ABI but a dedicated JS conversion.
    Guid,
    Struct(Arc<RwLock<dyn Declaration>>),
    /// Concrete class return: GetRuntimeClassName not needed.
    Object {
        decl: Arc<RwLock<dyn Declaration>>,
        type_name: Arc<str>,
    },
    /// Interface return: GetRuntimeClassName resolves the concrete class at runtime.
    InterfaceObject {
        decl: Arc<RwLock<dyn Declaration>>,
        type_name: Arc<str>,
    },
    /// Return type is `Object`/IInspectable: concrete type only known at runtime.
    DynamicObject,
}

pub(crate) fn classify_return(return_type: &str, is_void: bool) -> ReturnKind {
    if is_void {
        return ReturnKind::Void;
    }
    if return_type == "Guid" {
        return ReturnKind::Guid;
    }
    if return_type == "Object" {
        return ReturnKind::DynamicObject;
    }
    if return_type.contains('.') {
        let lookup = crate::helpers::strip_generic_suffix(return_type);
        match MetadataReader::find_by_name(lookup) {
            Some(decl) if matches!(decl.read().kind(), DeclarationKind::Struct) => {
                ReturnKind::Struct(decl)
            }
            Some(decl) if matches!(decl.read().kind(), DeclarationKind::Class) => {
                ReturnKind::Object {
                    decl,
                    type_name: Arc::from(return_type),
                }
            }
            Some(decl) => ReturnKind::InterfaceObject {
                decl,
                type_name: Arc::from(return_type),
            },
            None => crate::value::NativeType::try_from(return_type)
                .map(ReturnKind::Primitive)
                .unwrap_or(ReturnKind::Void),
        }
    } else {
        crate::value::NativeType::try_from(return_type)
            .map(ReturnKind::Primitive)
            .unwrap_or(ReturnKind::Void)
    }
}

#[derive(Clone)]
pub(crate) struct DeclarationFFI {
    pub(crate) inner: Arc<RwLock<dyn Declaration>>,
    pub(crate) instance: Option<IUnknown>,
    #[cfg(feature = "classic")]
    pub(crate) parent: Option<Arc<RwLock<dyn Declaration>>>,
    pub(crate) struct_instance: Option<(Vec<u8>, Vec<NativeType>)>,
    /// For inherited static properties/methods: the fully-qualified name of the
    /// WinRT class that declares them. Resolved lazily via class_activation_factory
    /// on first access so that constructors don't pay the cost of RoGetActivationFactory
    /// for every inherited static at object-creation time.
    #[cfg(feature = "classic")]
    pub(crate) static_factory_class: Option<String>,
}

unsafe impl Sync for DeclarationFFI {}

unsafe impl Send for DeclarationFFI {}

impl DeclarationFFI {
    #[cfg(feature = "classic")]
    pub fn new(declaration: Arc<RwLock<dyn Declaration>>) -> Self {
        Self {
            inner: declaration,
            instance: None,
            parent: None,
            struct_instance: None,
            static_factory_class: None,
        }
    }

    #[cfg(feature = "classic")]
    pub fn new_with_instance(
        declaration: Arc<RwLock<dyn Declaration>>,
        instance: Option<IUnknown>,
    ) -> Self {
        Self {
            inner: declaration,
            instance,
            parent: None,
            struct_instance: None,
            static_factory_class: None,
        }
    }

    #[cfg(feature = "classic")]
    pub fn as_any(&self) -> MappedRwLockReadGuard<'_, RawRwLock, dyn Any> {
        RwLockReadGuard::map(self.inner.read(), |dec| dec.as_any())
    }

    pub fn read(&self) -> MappedRwLockReadGuard<'_, RawRwLock, dyn Declaration> {
        RwLockReadGuard::map(self.inner.read(), |dec| dec)
    }

    #[cfg(feature = "classic")]
    pub fn write(&self) -> MappedRwLockWriteGuard<'_, RawRwLock, dyn Declaration> {
        RwLockWriteGuard::map(self.inner.write(), |dec| dec)
    }
}

impl Deref for DeclarationFFI {
    type Target = RwLock<dyn Declaration>;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

#[cfg(feature = "classic")]
use crate::generic_method_call::GenericMethodCall;
#[cfg(feature = "classic")]
use crate::method_call::MethodCall;
#[cfg(feature = "classic")]
use crate::property_call::PropertyCall;
#[cfg(feature = "classic")]
use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;

#[cfg(feature = "classic")]
struct HasInstanceData {
    /// IID used for the COM QueryInterface check.  `None` for classes and open
    /// generic interfaces that don't have a concrete parameterised IID.
    iid: Option<GUID>,
    /// Full WinRT type name used for an exact-name match before QI is attempted.
    full_name: String,
}

#[cfg(feature = "classic")]
unsafe impl Send for HasInstanceData {}
#[cfg(feature = "classic")]
unsafe impl Sync for HasInstanceData {}

#[cfg(feature = "classic")]
fn symbol_has_instance_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let arg = args.get(0);
    if !arg.is_object() {
        retval.set_bool(false);
        return;
    }
    let Some(obj) = arg.to_object(scope) else {
        retval.set_bool(false);
        return;
    };

    let data_ext = unsafe { args.data().cast::<v8::External>() };
    let data_ptr = data_ext.value() as *const HasInstanceData;
    let data = unsafe { &*data_ptr };

    if let Some(dec_field) = obj.get_internal_field(scope, 0) {
        if let Ok(dec_ext) = v8::Local::<v8::External>::try_from(dec_field) {
            let dec_ptr = dec_ext.value() as *mut DeclarationFFI;
            if !dec_ptr.is_null() {
                let dec = unsafe { &*dec_ptr };

                {
                    let lock = dec.read();
                    if lock.full_name() == data.full_name.as_str() {
                        retval.set_bool(true);
                        return;
                    }
                }

                let Some(iid) = data.iid else {
                    retval.set_bool(false);
                    return;
                };
                let Some(instance) = dec.instance.clone() else {
                    retval.set_bool(false);
                    return;
                };

                let vtable = instance.vtable();
                let mut qi_ptr: *mut c_void = std::ptr::null_mut();
                let hr = unsafe {
                    ((*vtable).QueryInterface)(
                        instance.as_raw(),
                        &iid,
                        std::mem::transmute(&mut qi_ptr),
                    )
                };
                let implements = hr.is_ok() && !qi_ptr.is_null();
                if !qi_ptr.is_null() {
                    drop(unsafe { IUnknown::from_raw(qi_ptr) });
                }
                retval.set_bool(implements);
                return;
            }
        }
    }

    if let Some(type_key) = v8::String::new(scope, "__type") {
        if let Some(tv) = obj.get(scope, type_key.into()) {
            if let Ok(ts) = v8::Local::<v8::String>::try_from(tv) {
                if ts.to_rust_string_lossy(scope) == data.full_name {
                    retval.set_bool(true);
                    return;
                }
            }
        }
    }

    let Some(iid) = data.iid else {
        retval.set_bool(false);
        return;
    };
    let raw_ptr = dotnet_proxy_native_ptr(scope, obj);
    if raw_ptr.is_null() {
        retval.set_bool(false);
        return;
    }

    let unk = std::mem::ManuallyDrop::new(unsafe { IUnknown::from_raw(raw_ptr) });
    let vtable = unk.vtable();
    let mut qi_ptr: *mut c_void = std::ptr::null_mut();
    let hr =
        unsafe { ((*vtable).QueryInterface)(unk.as_raw(), &iid, std::mem::transmute(&mut qi_ptr)) };
    let implements = hr.is_ok() && !qi_ptr.is_null();
    if !qi_ptr.is_null() {
        drop(unsafe { IUnknown::from_raw(qi_ptr) });
    }
    retval.set_bool(implements);
}

#[cfg(feature = "classic")]
fn dotnet_proxy_native_ptr(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> *mut c_void {
    if let Some(k) = v8::String::new(scope, "__native_ptr") {
        if let Some(pval) = obj.get(scope, k.into()) {
            if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(pval) {
                let p = bi.u64_value().0 as *mut c_void;
                if !p.is_null() {
                    return p;
                }
            }
        }
    }

    if let Some(hk) = v8::String::new(scope, "__handle") {
        if let Some(hval) = obj.get(scope, hk.into()) {
            let id: Option<i32> = if let Ok(v) = v8::Local::<v8::Int32>::try_from(hval) {
                Some(v.value())
            } else if let Ok(n) = v8::Local::<v8::Number>::try_from(hval) {
                n.integer_value(scope).map(|v| v as i32)
            } else {
                None
            };
            if let Some(id) = id {
                let req = format!(
                    "{{\"assembly\":null,\"typeName\":\"NativeScriptBridge.Bridge\",\"method\":\"GetNativePtrForHandle\",\"args\":[{}]}}",
                    id
                );
                if let Ok(resp) = crate::dotnet::call_dotnet(&req) {
                    let trimmed = resp.trim();
                    if !trimmed.is_empty() && trimmed != "null" {
                        if let Ok(n) = trimmed.parse::<i64>() {
                            if n != 0 {
                                return n as usize as *mut c_void;
                            }
                        }
                    }
                }
            }
        }
    }
    std::ptr::null_mut()
}

#[cfg(feature = "classic")]
fn attach_has_instance_to_template(
    scope: &mut v8::PinScope<'_, '_>,
    ctor_tmpl: v8::Local<v8::FunctionTemplate>,
    iid: Option<GUID>,
    full_name: &str,
) {
    let data = Box::into_raw(Box::new(HasInstanceData {
        iid,
        full_name: full_name.to_string(),
    }));
    let data_ext = v8::External::new(scope, data as _);
    let has_instance_tmpl = v8::FunctionTemplate::builder(symbol_has_instance_callback)
        .data(data_ext.into())
        .build(scope);
    let sym = v8::Symbol::get_has_instance(scope);
    ctor_tmpl.set_with_attr(
        sym.into(),
        has_instance_tmpl.into(),
        v8::PropertyAttribute::DONT_ENUM,
    );
}

#[cfg(feature = "classic")]
fn init_global(
    scope: &mut v8::ContextScope<v8::HandleScope<v8::Context>>,
    context: v8::Local<v8::Context>,
) {
    let global = context.global(scope);
    let value = v8::String::new(scope, "global").unwrap().into();
    global.define_own_property(
        scope,
        value,
        global.into(),
        v8::PropertyAttribute::READ_ONLY,
    );
}

pub fn debug_output(msg: &str) {
    // Only emit verbose debug logs when `NS_DEBUG` is present. Always allow
    // important severities through (ERROR/WARN/DEVTOOLS/NativeScript).
    let important = msg.starts_with("[ERROR]")
        || msg.starts_with("[WARN]")
        || msg.starts_with("[DEVTOOLS]")
        || msg.starts_with("[NativeScript]");
    // Runtime-configurable flag: default true.
    let enabled = LOG_TO_CONSOLE
        .get_or_init(|| AtomicBool::new(true))
        .load(AtomicOrdering::Relaxed);

    if !enabled && !important {
        return;
    }

    // Send UTF-16 string to debugger for reliable Unicode output
    let wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { OutputDebugStringW(PCWSTR::from_raw(wide.as_ptr())) };
    eprint!("{}", msg);
    use std::io::Write;
    LOG_FILE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            static LOG_PATH: OnceLock<String> = OnceLock::new();
            let path = LOG_PATH.get_or_init(|| {
                // Prefer the host-provided LocalState folder so the trace log is deterministic and
                // sits beside the crash/panic logs; fall back to the process temp dir,
                // then USERPROFILE, if it isn't set or isn't writable.
                let mut p = match LOG_DIR.get() {
                    Some(dir) => {
                        let mut pb = std::path::PathBuf::from(dir);
                        pb.push("console.log");
                        pb
                    }
                    None => {
                        let mut t = std::env::temp_dir();
                        t.push("console.log");
                        t
                    }
                };
                let writable = |path: &std::path::Path| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .is_ok()
                };
                if !writable(&p) {
                    let mut t = std::env::temp_dir();
                    t.push("console.log");
                    p = t;
                }
                let chosen = if writable(&p) {
                    p.to_string_lossy().into_owned()
                } else {
                    let base =
                        std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\fortu".into());
                    format!("{}\\console.log", base)
                };
                let banner = format!("[NativeScript] log file: {}\n", chosen);
                let wide_banner: Vec<u16> =
                    banner.encode_utf16().chain(std::iter::once(0)).collect();
                unsafe { OutputDebugStringW(PCWSTR::from_raw(wide_banner.as_ptr())) };
                chosen
            });
            *slot = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok();
        }
        if let Some(f) = slot.as_mut() {
            let _ = f.write_all(msg.as_bytes());
        }
    });

    // If this message came from the DevTools forwarder, map severity to
    // Windows Event Log so administrators can see important errors/warnings.
    if msg.starts_with("[DEVTOOLS]") {
        if let Some(rest) = msg.strip_prefix("[DEVTOOLS] ") {
            if rest.starts_with('[') {
                if let Some(end) = rest.find(']') {
                    let level = &rest[1..end];
                    use windows::Win32::System::EventLog::{
                        EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
                    };
                    let event_type = match level {
                        "ERROR" | "EXCEPTION" => EVENTLOG_ERROR_TYPE,
                        "WARN" | "WARNING" => EVENTLOG_WARNING_TYPE,
                        _ => EVENTLOG_INFORMATION_TYPE,
                    };
                    crate::globals::console::report_event(msg, event_type);
                }
            }
        }
    }
}

/// Set the directory for the runtime trace log (`console.log`). Called by the host with the app's
/// LocalState folder. Idempotent (first value wins): must be set before the first `debug_output`.
pub fn set_log_dir(dir: String) {
    let _ = LOG_DIR.set(dir);
}

#[cfg(test)]
pub(crate) fn log_dir_for_tests() -> Option<String> {
    LOG_DIR.get().cloned()
}

/// Disk path for a chunk's V8 bytecode cache, keyed by filename + a hash of the source. A livesync
/// edit changes the source → different hash → cache miss → recompile (never stale bytecode). Lives
/// under the app's writable local folder (LOG_DIR). Returns None before that folder is known.
#[cfg(feature = "classic")]
fn code_cache_path(filename: &str, script: &str) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = LOG_DIR.get()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    script.hash(&mut hasher);
    let base = std::path::Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("chunk");
    Some(
        std::path::Path::new(dir)
            .join("v8-codecache")
            .join(format!("{base}.{:016x}.jsc", hasher.finish())),
    )
}

/// Enable or disable logging to console at runtime. Default is `true`.
pub fn set_log_to_console(enabled: bool) {
    LOG_TO_CONSOLE
        .get_or_init(|| AtomicBool::new(true))
        .store(enabled, AtomicOrdering::Relaxed);
}

/// Query whether logging to console is enabled.
pub fn is_log_to_console() -> bool {
    LOG_TO_CONSOLE
        .get_or_init(|| AtomicBool::new(true))
        .load(AtomicOrdering::Relaxed)
}

/// C ABI: toggle logging to console from native hosts (e.g. C# P/Invoke).
/// Returns 1 on success (toggle applied), 0 if toggling is disabled (release builds).
#[no_mangle]
pub extern "C" fn ns_set_log_to_console(enabled: std::os::raw::c_int) -> std::os::raw::c_int {
    if cfg!(debug_assertions) {
        set_log_to_console(enabled != 0);
        1
    } else {
        0
    }
}

thread_local!(static LOG_FILE: RefCell<Option<fs::File>> = RefCell::new(None));

#[cfg(feature = "classic")]
pub(crate) fn throw_js_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(msg) = v8::String::new(scope, message) {
        let err = v8::Exception::error(scope, msg.into());
        scope.throw_exception(err);
    }
}

pub(crate) fn class_activation_factory(full_name: &str) -> windows::core::Result<IUnknown> {
    // Fast path: factories are app-lifetime singletons: cache avoids a COM broker round-trip per call.
    let cached = ACTIVATION_FACTORY_CACHE.with(|c| c.borrow().get(full_name).cloned());
    if let Some(factory) = cached {
        return Ok(factory);
    }
    let clazz_name = HSTRING::from(full_name);
    let factory = unsafe { RoGetActivationFactory::<IUnknown>(&clazz_name) }?;
    ACTIVATION_FACTORY_CACHE.with(|c| {
        c.borrow_mut()
            .insert(full_name.to_string(), factory.clone())
    });
    Ok(factory)
}

#[cfg(feature = "classic")]
pub(crate) fn resolve_class_factory_from_parent(
    dec: &DeclarationFFI,
) -> windows::core::Result<IUnknown> {
    if let Some(instance) = dec.instance.clone() {
        return Ok(instance);
    }

    // Lazy path: static_factory_class stores the declaring class name; call the factory only now.
    if let Some(ref class_name) = dec.static_factory_class {
        return class_activation_factory(class_name.as_str());
    }

    let Some(parent) = dec.parent.as_ref() else {
        return Err(Error::new(
            HRESULT(0x80004005u32 as i32),
            "Static WinRT member is missing its owning class declaration",
        ));
    };

    let parent = parent.read();
    let Some(clazz) = parent.as_any().downcast_ref::<ClassDeclaration>() else {
        return Err(Error::new(
            HRESULT(0x80004005u32 as i32),
            "Static WinRT member parent is not a class declaration",
        ));
    };

    class_activation_factory(clazz.full_name())
}

#[cfg(feature = "classic")]
fn value_to_string(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let value = value.to_string(scope)?;
    Some(value.to_rust_string_lossy(scope))
}

pub(crate) fn normalize_js_path(path: &str) -> PathBuf {
    if let Some(raw) = path.strip_prefix("file:///") {
        return PathBuf::from(raw.replace('/', "\\"));
    }
    if let Some(raw) = path.strip_prefix("file://") {
        return PathBuf::from(raw.replace('/', "\\"));
    }
    PathBuf::from(path)
}

pub(crate) fn try_resolve_with_known_extensions(candidate: PathBuf) -> PathBuf {
    // Each `.exists()` probe also checks the sealed app.nsbundle's decrypted table (a no-op,
    // cheap OnceLock read when no bundle is loaded): a packed app has no real `app/` directory
    // on disk, so `is_dir()`/`.exists()` alone would never resolve anything.
    if candidate.exists() || crate::source_protect::contains(&candidate.to_string_lossy()) {
        return candidate;
    }

    if candidate.extension().is_none() {
        for ext in ["js", "mjs", "cjs"] {
            let with_ext = candidate.with_extension(ext);
            if with_ext.exists() || crate::source_protect::contains(&with_ext.to_string_lossy()) {
                return with_ext;
            }
        }
    }

    for index_file in ["index.js", "index.mjs", "index.cjs"] {
        let with_index = candidate.join(index_file);
        if with_index.exists() || crate::source_protect::contains(&with_index.to_string_lossy()) {
            return with_index;
        }
    }

    candidate
}

fn js_path_exists(p: &Path) -> bool {
    p.exists() || crate::source_protect::contains(&p.to_string_lossy())
}

/// Find the entry module a host asked to run. Hosts pass either an absolute path or just a
/// file name ("bundle.mjs"); a bare name is looked up in the app directories the templates use
/// (`<app_root>/app`, `<app_root>/../app`, …) rather than the process working directory.
fn locate_entry_module(filename: &str, app_root: &str) -> PathBuf {
    let p = normalize_js_path(filename);
    if p.is_absolute() {
        return try_resolve_with_known_extensions(p);
    }
    let root = PathBuf::from(app_root);
    let parent = root.parent().map(Path::to_path_buf);
    let bases = [
        Some(root.join("app")),
        Some(root.join("App")),
        parent.as_ref().map(|d| d.join("app")),
        parent.as_ref().map(|d| d.join("App")),
        Some(root.clone()),
        std::env::current_dir().ok(),
    ];
    for base in bases.into_iter().flatten() {
        let candidate = try_resolve_with_known_extensions(base.join(&p));
        if js_path_exists(&candidate) {
            return candidate;
        }
    }
    try_resolve_with_known_extensions(p)
}

thread_local! {
    // Directory of the entry module: what `~/…` and root-absolute (`/…`) specifiers resolve
    // against, matching the iOS/Android runtimes' app-root mapping.
    static ESM_APP_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn esm_app_dir() -> Option<PathBuf> {
    ESM_APP_DIR.with(|d| d.borrow().clone())
}

thread_local! {
    // Canonical HTTP registry key → the URL it was last requested as (the fetch keeps the query the
    // canonical key may drop).
    static ESM_HTTP_URLS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    // Registry key → why loading it failed, so the resolve callback can report the real cause.
    static ESM_LOAD_ERRORS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

const NS_MODULE_BUILTIN: &str = "ns:module";

/// Resolve a module specifier to its registry key: `ns:module` for the builtin, a canonical URL
/// for modules served over HTTP(S), otherwise an absolute file path.
///
/// Order follows the iOS/Android runtimes: builtins, then the `configureLoader` import map, then
/// absolute URLs, then relative/root-absolute specifiers from an HTTP referrer (resolved as URLs,
/// the way a browser does), then files.
fn resolve_esm_path(specifier: &str, referrer_path: Option<&str>) -> String {
    if specifier == NS_MODULE_BUILTIN {
        return specifier.to_string();
    }
    let mut spec = crate::esm_http::repair_collapsed_scheme(specifier);
    if let Some(mapped) = crate::esm_http::lookup_import_map(&spec, referrer_path.unwrap_or("")) {
        spec = mapped;
        if spec == NS_MODULE_BUILTIN {
            return spec;
        }
    }
    let referrer_is_http = referrer_path.is_some_and(crate::esm_http::is_http);
    let url = if crate::esm_http::is_http(&spec) {
        Some(spec.clone())
    } else if referrer_is_http && (spec.starts_with('.') || spec.starts_with('/')) {
        url::Url::parse(referrer_path.unwrap_or(""))
            .ok()
            .and_then(|base| base.join(&spec).ok())
            .map(|u| u.to_string())
    } else {
        None
    };
    if let Some(url) = url {
        let key = crate::esm_http::canonicalize_http_url_key(&url);
        ESM_HTTP_URLS.with(|m| m.borrow_mut().insert(key.clone(), url));
        return key;
    }
    resolve_file_specifier(&spec, referrer_path.filter(|r| !crate::esm_http::is_http(r)))
}

/// File resolution: relative (`./`, `../`), `file://`, app-rooted (`~/`, `/`) and absolute
/// specifiers. Query strings and fragments are dropped (bundlers use them as cache busters). Bare
/// specifiers resolve against the app directory. Bundled output only emits relative imports,
/// so a bare one is either an app-root path or unresolvable either way.
fn resolve_file_specifier(specifier: &str, referrer_path: Option<&str>) -> String {
    let specifier = if specifier.starts_with("file:") || !specifier.contains("://") {
        specifier.split(['?', '#']).next().unwrap_or(specifier)
    } else {
        specifier
    };
    let app_rooted = specifier
        .strip_prefix("~/")
        .or_else(|| specifier.strip_prefix('/').filter(|s| !s.starts_with('/')));
    let is_bare = !specifier.starts_with('.')
        && !specifier.contains(':')
        && !specifier.starts_with('/')
        && !specifier.starts_with('\\');
    let candidate = if let Some(rest) = app_rooted.filter(|_| esm_app_dir().is_some()) {
        esm_app_dir().unwrap_or_default().join(rest)
    } else if is_bare && esm_app_dir().is_some() {
        esm_app_dir().unwrap_or_default().join(specifier)
    } else if specifier.starts_with("./") || specifier.starts_with("../") {
        let parent = referrer_path
            .map(normalize_js_path)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        // A packed (virtual, no-plaintext-on-disk) referrer never satisfies `.is_file()`, so also
        // check the sealed bundle's table: otherwise a relative import from a bundle-only module
        // would wrongly treat the referrer itself as the base directory instead of its parent.
        let base = if parent.is_file() || crate::source_protect::contains(&parent.to_string_lossy())
        {
            parent.parent().map(Path::to_path_buf).unwrap_or(parent)
        } else {
            parent
        };
        base.join(specifier)
    } else {
        normalize_js_path(specifier)
    };
    let candidate = try_resolve_with_known_extensions(candidate);
    candidate
        .canonicalize()
        .unwrap_or(candidate)
        .to_string_lossy()
        .into_owned()
}

/// Stateless V8 resolve-module callback used during `instantiate_module`.
/// All modules must have been pre-compiled by `compile_module_graph` and stored
/// in `ESM_MODULE_REGISTRY` / `ESM_HASH_TO_PATH` before this is called.
#[cfg(feature = "classic")]
fn resolve_module_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);

    let spec = specifier.to_rust_string_lossy(scope);
    let referrer_hash = referrer.get_identity_hash().get();
    let referrer_path = ESM_HASH_TO_PATH.with(|m| m.borrow().get(&referrer_hash).cloned());
    let resolved = resolve_esm_path(&spec, referrer_path.as_deref());

    let found = ESM_MODULE_REGISTRY.with(|registry| {
        let registry = registry.borrow();
        registry
            .get(&resolved)
            .map(|global| v8::Local::new(scope, global))
    });
    if found.is_none() {
        // V8 requires a pending exception when resolution fails; returning empty without one
        // is a fatal CHECK in instantiate_module.
        let msg = module_not_found_message(&spec, referrer_path.as_deref(), &resolved);
        if let Some(s) = v8::String::new(scope, &msg) {
            let err = v8::Exception::error(scope, s);
            scope.throw_exception(err);
        }
    }
    found
}

fn module_not_found_message(spec: &str, referrer: Option<&str>, resolved: &str) -> String {
    let from = referrer.map(|r| format!(" imported from {r}")).unwrap_or_default();
    match ESM_LOAD_ERRORS.with(|m| m.borrow().get(resolved).cloned()) {
        Some(reason) => format!("{reason}{from}"),
        None => format!("Cannot find module '{spec}'{from} (resolved to {resolved})"),
    }
}

/// `import.meta` for modules loaded through `compile_module_graph`: `url` (a `file:///` URL, as
/// the iOS/Android runtimes and bundlers' `new URL(x, import.meta.url)` expect), `filename` and
/// `dirname`.
#[cfg(feature = "classic")]
unsafe extern "C" fn esm_import_meta(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    v8::callback_scope!(unsafe scope, context);
    let hash = module.get_identity_hash().get();
    let Some(path) = ESM_HASH_TO_PATH.with(|m| m.borrow().get(&hash).cloned()) else {
        return;
    };
    if crate::esm_http::is_http(&path) || path == NS_MODULE_BUILTIN {
        // A served module's identity is its URL; it has no file name or directory.
        if let (Some(k), Some(v)) = (v8::String::new(scope, "url"), v8::String::new(scope, &path)) {
            let _ = meta.create_data_property(scope, k.into(), v.into());
        }
        return;
    }
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path).to_string();
    let url = format!("file:///{}", path.replace('\\', "/"));
    let dirname = Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (k, v) in [("url", url.as_str()), ("filename", path.as_str()), ("dirname", dirname.as_str())] {
        if let (Some(k), Some(v)) = (v8::String::new(scope, k), v8::String::new(scope, v)) {
            let _ = meta.create_data_property(scope, k.into(), v.into());
        }
    }
}

/// Message + stack for a thrown/rejected JS value (the `stack` property when it's an Error).
#[cfg(feature = "classic")]
fn js_exception_report(scope: &mut v8::PinScope<'_, '_>, exc: v8::Local<v8::Value>) -> String {
    if exc.is_object() {
        if let (Some(obj), Some(key)) = (exc.to_object(scope), v8::String::new(scope, "stack")) {
            if let Some(stack) = obj.get(scope, key.into()).filter(|s| s.is_string()) {
                return stack.to_rust_string_lossy(scope);
            }
        }
    }
    exc.to_rust_string_lossy(scope)
}

#[cfg(feature = "classic")]
fn report_module_error(scope: &mut v8::PinScope<'_, '_>, exc: v8::Local<v8::Value>) {
    let report = js_exception_report(scope, exc);
    debug_output(&format!("[NativeScript] Uncaught error evaluating module: {report}\n"));
    crate::store_last_js_error(report);
}

/// With top-level await, `Module::evaluate` returns a promise and an exception thrown while the
/// graph evaluates *rejects* it instead of throwing. Nothing reaches a TryCatch. Surface it: a
/// settled rejection now, a pending (TLA) graph whenever it rejects.
#[cfg(feature = "classic")]
fn report_module_evaluation(scope: &mut v8::PinScope<'_, '_>, result: v8::Local<v8::Value>) {
    let Ok(promise) = v8::Local::<v8::Promise>::try_from(result) else {
        return;
    };
    match promise.state() {
        v8::PromiseState::Rejected => {
            let exc = promise.result(scope);
            report_module_error(scope, exc);
        }
        v8::PromiseState::Pending => {
            let on_reject = v8::Function::new(
                scope,
                |scope: &mut v8::PinScope<'_, '_>,
                 args: v8::FunctionCallbackArguments,
                 _rv: v8::ReturnValue| {
                    report_module_error(scope, args.get(0));
                },
            );
            if let Some(on_reject) = on_reject {
                let _ = promise.catch(scope, on_reject);
            }
        }
        v8::PromiseState::Fulfilled => {}
    }
}

#[cfg(feature = "classic")]
fn js_error_value<'s>(scope: &mut v8::PinScope<'s, '_>, msg: &str) -> v8::Local<'s, v8::Value> {
    match v8::String::new(scope, msg) {
        Some(s) => v8::Exception::error(scope, s),
        None => v8::undefined(scope).into(),
    }
}

/// Source of the `ns:module` builtin: the dev-loader control surface (`configureLoader`,
/// `invalidateModules`, `getLoadedModuleUrls`, `createRequire`) the HELPER_SOURCE installs as
/// `__nsModuleBuiltin`, re-exported for `import … from "ns:module"`.
const NS_MODULE_SOURCE: &str = "const m = globalThis.__nsModuleBuiltin;\n\
export default m;\n\
export const configureLoader = m.configureLoader, invalidateModules = m.invalidateModules, \
getLoadedModuleUrls = m.getLoadedModuleUrls, createRequire = m.createRequire;\n";

/// Registry key for a URL handed in by JS (`invalidateModules`): canonical for HTTP, a path for
/// `file://`, verbatim otherwise.
fn registry_key_for(url: &str) -> String {
    if crate::esm_http::is_http(url) || url.starts_with("file://http") || url.starts_with("http:/") || url.starts_with("https:/") {
        return crate::esm_http::canonicalize_http_url_key(url);
    }
    if url.starts_with("file://") {
        let p = normalize_js_path(url);
        return p.canonicalize().unwrap_or(p).to_string_lossy().into_owned();
    }
    url.to_string()
}

/// Drop a module from the registry so the next import compiles (and, over HTTP, fetches) it anew.
/// Importers already linked keep the old record. The HMR client re-imports the graph above it.
#[cfg(feature = "classic")]
fn evict_module_key(key: &str) -> bool {
    let removed = ESM_MODULE_REGISTRY.with(|r| r.borrow_mut().remove(key)).is_some();
    ESM_HASH_TO_PATH.with(|m| m.borrow_mut().retain(|_, v| v != key));
    ESM_LOAD_ERRORS.with(|m| m.borrow_mut().remove(key));
    removed
}

#[cfg(feature = "classic")]
fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    if let Some(s) = v8::String::new(scope, msg) {
        let err = v8::Exception::type_error(scope, s);
        scope.throw_exception(err);
    }
}

/// `ns:module` `configureLoader`: called by the JS wrapper with the config as a JSON string.
#[cfg(feature = "classic")]
pub(crate) fn handle_ns_module_configure_loader(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let json = args.get(0).to_rust_string_lossy(scope);
    if let Err(e) = crate::esm_http::configure_loader(&json) {
        throw_type_error(scope, &e);
    }
}

/// `ns:module` `invalidateModules(urls)`: registry eviction plus a one-shot cache-bust nonce on
/// each evicted URL's next fetch. Returns the number of registry entries removed.
#[cfg(feature = "classic")]
pub(crate) fn handle_ns_module_invalidate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Ok(array) = v8::Local::<v8::Array>::try_from(args.get(0)) else {
        throw_type_error(scope, "invalidateModules expects an array of URL strings");
        return;
    };
    let mut keys: Vec<String> = Vec::new();
    for i in 0..array.length() {
        let Some(value) = array.get_index(scope, i) else {
            return;
        };
        if !value.is_string() {
            throw_type_error(scope, &format!("invalidateModules: urls[{i}] must be a string"));
            return;
        }
        let key = registry_key_for(&value.to_rust_string_lossy(scope));
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    let removed = keys.iter().filter(|k| evict_module_key(k)).count();
    let http_keys: Vec<String> = keys.into_iter().filter(|k| crate::esm_http::is_http(k)).collect();
    crate::esm_http::mark_keys_for_cache_bust(&http_keys);
    rv.set(v8::Integer::new(scope, removed as i32).into());
}

/// `ns:module` `getLoadedModuleUrls()`: the URL-keyed (served) modules currently registered.
#[cfg(feature = "classic")]
pub(crate) fn handle_ns_module_loaded_urls(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let mut urls: Vec<String> = ESM_MODULE_REGISTRY.with(|r| {
        r.borrow().keys().filter(|k| k.contains("://")).cloned().collect()
    });
    urls.sort();
    let array = v8::Array::new(scope, urls.len() as i32);
    for (i, url) in urls.iter().enumerate() {
        if let Some(s) = v8::String::new(scope, url) {
            array.set_index(scope, i as u32, s.into());
        }
    }
    rv.set(array.into());
}

fn http_fetch_url(key: &str) -> String {
    ESM_HTTP_URLS.with(|m| m.borrow().get(key).cloned()).unwrap_or_else(|| key.to_string())
}

fn http_module_source(fetched: crate::esm_http::FetchedModule) -> String {
    match fetched.kind {
        crate::esm_http::ModuleKind::JavaScript => fetched.body,
        crate::esm_http::ModuleKind::Json => crate::esm_http::json_module_source(&fetched.body),
    }
}

fn record_load_error(key: &str, reason: String) {
    debug_output(&format!("[NativeScript] ESM: {reason}\n"));
    ESM_LOAD_ERRORS.with(|m| m.borrow_mut().insert(key.to_string(), reason));
}

/// Source text for a registry key: the builtin, an HTTP module (fetched now), or a file.
fn read_module_source(key: &str) -> Option<String> {
    if key == NS_MODULE_BUILTIN {
        return Some(NS_MODULE_SOURCE.to_string());
    }
    if crate::esm_http::is_http(key) {
        return match crate::esm_http::fetch_module(&http_fetch_url(key), key) {
            Ok(fetched) => Some(http_module_source(fetched)),
            Err(reason) => {
                record_load_error(key, reason);
                None
            }
        };
    }
    crate::source_protect::read_text(key).or_else(|| fs::read_to_string(key).ok())
}

/// The module loader's engine-neutral half, for engines whose module records live in their own
/// shim (the napi V8 package compiles, links and evaluates in C++): the same resolution, source
/// loading, error text and `ns:module` bookkeeping the classic loader above uses, so both
/// resolve a graph identically.
pub mod esm_loader {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::Path;

    thread_local! {
        // HTTP modules fetched ahead by `prefetch`, consumed by `read_source`.
        static PREFETCHED: RefCell<HashMap<String, Result<crate::esm_http::FetchedModule, String>>> =
            RefCell::new(HashMap::new());
    }

    /// Registry key of the entry module the host passes in (`bundle.mjs`, absolute or relative to
    /// the app root). Also roots `~/` / `/` specifiers and the remote-module gate at its directory.
    pub fn entry_key(filename: &str, app_root: &str) -> String {
        let p = super::locate_entry_module(filename, app_root);
        let key = p.canonicalize().unwrap_or(p).to_string_lossy().into_owned();
        if let Some(dir) = Path::new(&key).parent() {
            super::ESM_APP_DIR.with(|d| *d.borrow_mut() = Some(dir.to_path_buf()));
            crate::esm_http::init_security_from_app_dir(dir);
        }
        key
    }

    /// Whether the host's entry script is an ES module: Vite's `bundle.mjs`, or source opening
    /// with an import/export (the classic runtime's `run_script` rule).
    pub fn is_module_entry(filename: &str, code: &str) -> bool {
        let trimmed = code.trim_start();
        filename.ends_with(".mjs") || trimmed.starts_with("import ") || trimmed.starts_with("export ")
    }

    pub fn resolve(spec: &str, referrer: Option<&str>) -> String {
        super::resolve_esm_path(spec, referrer)
    }

    /// Source text for a registry key (builtin, HTTP fetch or file); `None` records why.
    pub fn read_source(key: &str) -> Option<String> {
        match PREFETCHED.with(|m| m.borrow_mut().remove(key)) {
            Some(Ok(fetched)) => Some(super::http_module_source(fetched)),
            Some(Err(reason)) => {
                super::record_load_error(key, reason);
                None
            }
            None => super::read_module_source(key),
        }
    }

    /// Fetch a module's HTTP dependencies concurrently before they're read one by one, as the
    /// classic graph walk does (a dev-server boot walks hundreds of modules).
    pub fn prefetch(keys: &[String]) {
        let pending: Vec<(String, String)> = keys
            .iter()
            .filter(|k| crate::esm_http::is_http(k))
            .filter(|k| !PREFETCHED.with(|m| m.borrow().contains_key(k.as_str())))
            .map(|k| (super::http_fetch_url(k), k.clone()))
            .collect();
        if pending.len() < 2 {
            return;
        }
        let results = crate::esm_http::fetch_modules(&pending);
        PREFETCHED.with(|m| {
            let mut m = m.borrow_mut();
            for ((_, key), result) in pending.into_iter().zip(results) {
                m.insert(key, result);
            }
        });
    }

    pub fn not_found_message(spec: &str, referrer: Option<&str>, key: &str) -> String {
        super::module_not_found_message(spec, referrer, key)
    }

    /// Volatile (`configureLoader`) HTTP keys are re-fetched on every dynamic import.
    pub fn is_volatile(key: &str) -> bool {
        crate::esm_http::is_http(key) && crate::esm_http::is_volatile(key)
    }

    pub fn is_http(key: &str) -> bool {
        crate::esm_http::is_http(key)
    }

    /// Forget a key's recorded load failure (the engine drops its module record alongside).
    pub fn forget(key: &str) {
        super::ESM_LOAD_ERRORS.with(|m| m.borrow_mut().remove(key));
        PREFETCHED.with(|m| m.borrow_mut().remove(key));
    }

    pub fn registry_key_for(url: &str) -> String {
        super::registry_key_for(url)
    }

    pub fn configure_loader(json: &str) -> Result<(), String> {
        crate::esm_http::configure_loader(json)
    }

    pub fn mark_keys_for_cache_bust(keys: &[String]) {
        crate::esm_http::mark_keys_for_cache_bust(keys)
    }

    /// Specifier (and registry key) of the `ns:module` builtin, which re-exports
    /// `globalThis.__nsModuleBuiltin`.
    pub const NS_MODULE: &str = super::NS_MODULE_BUILTIN;
}

/// Load, link and evaluate the module graph rooted at `resolved` (`source` when the caller
/// already has the root's text). `Ok` carries the root module and `evaluate`'s result (a promise
/// under TLA); `Err` the value to throw/reject with. The real exception (SyntaxError, link
/// error, …), not a stringified summary. Globals, since the TryCatch scope is internal.
#[cfg(feature = "classic")]
type EsmEvaluated = (v8::Global<v8::Module>, v8::Global<v8::Value>);

#[cfg(feature = "classic")]
fn esm_load_and_evaluate(
    scope: &mut v8::PinScope<'_, '_>,
    spec: &str,
    referrer: Option<&str>,
    resolved: &str,
    source: Option<&str>,
) -> Result<EsmEvaluated, v8::Global<v8::Value>> {
    v8::tc_scope!(tc, scope);
    let fail = |tc: &mut v8::PinScope<'_, '_>, exc: Option<v8::Local<v8::Value>>, what: &str| {
        let exc = exc.unwrap_or_else(|| js_error_value(tc, &format!("ESM: failed to {what} {resolved}")));
        v8::Global::new(tc, exc)
    };
    let existing = ESM_MODULE_REGISTRY.with(|r| r.borrow().get(resolved).cloned());
    let module = match existing {
        Some(g) => v8::Local::new(tc, &g),
        None => {
            let owned = match source {
                Some(s) => s.to_string(),
                None => match read_module_source(resolved) {
                    Some(s) => s,
                    None => {
                        let msg = module_not_found_message(spec, referrer, resolved);
                        let exc = js_error_value(tc, &msg);
                        return Err(v8::Global::new(tc, exc));
                    }
                },
            };
            compile_module_graph(tc, &owned, resolved);
            if tc.has_caught() {
                let exc = tc.exception();
                return Err(fail(tc, exc, "compile"));
            }
            match ESM_MODULE_REGISTRY.with(|r| r.borrow().get(resolved).cloned()) {
                Some(g) => v8::Local::new(tc, &g),
                None => return Err(fail(tc, None, "compile")),
            }
        }
    };
    if module.instantiate_module(tc, resolve_module_callback).is_none() {
        let exc = tc.exception();
        return Err(fail(tc, exc, "link"));
    }
    match module.evaluate(tc) {
        Some(result) => Ok((v8::Global::new(tc, module), v8::Global::new(tc, result))),
        None => {
            let exc = tc.exception();
            Err(fail(tc, exc, "evaluate"))
        }
    }
}

thread_local! {
    // Modules compiled without a usable code cache, awaiting `flush_module_code_caches`.
    #[cfg(feature = "classic")]
    static PENDING_MODULE_CODE_CACHES: RefCell<Vec<(v8::Global<v8::Module>, PathBuf)>> =
        const { RefCell::new(Vec::new()) };
}

/// Write code caches for modules compiled this run. Called after the entry graph evaluates, so
/// the cache also covers the functions startup compiled lazily, not just top-level code.
#[cfg(feature = "classic")]
fn flush_module_code_caches(scope: &mut v8::PinScope<'_, '_>) {
    let pending = PENDING_MODULE_CODE_CACHES.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for (module, path) in pending {
        let module = v8::Local::new(scope, &module);
        if module.get_status() == v8::ModuleStatus::Errored {
            continue;
        }
        if let Some(cached) = module.get_unbound_module_script(scope).create_code_cache() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&path, &**cached);
        }
    }
}

/// Walk and pre-compile the entire transitive module graph starting from `path`.
/// Compiled modules are stored in `ESM_MODULE_REGISTRY` and `ESM_HASH_TO_PATH`.
/// Must be called before `instantiate_module`.
#[cfg(feature = "classic")]
fn compile_module_graph(scope: &mut v8::PinScope<'_, '_>, source: &str, path: &str) -> bool {
    if ESM_MODULE_REGISTRY.with(|r| r.borrow().contains_key(path)) {
        return true;
    }

    let Some(source_str) = v8::String::new(scope, source) else {
        return false;
    };
    let Some(name_str) = v8::String::new(scope, path) else {
        return false;
    };
    let name_val: v8::Local<v8::Value> = name_str.into();
    let origin = v8::ScriptOrigin::new(
        scope, name_val, 0, 0, false, -1, None, false, false, true, None,
    );
    // Bytecode-cache large modules (Vite's bundle.mjs / vendor.mjs), as run_script does for
    // classic chunks. Keyed by content hash, so an edited module just misses and recompiles.
    let cache_path = if source.len() >= 65536 && !crate::esm_http::is_http(path) {
        code_cache_path(path, source)
    } else {
        None
    };
    let cached_bytes = cache_path.as_ref().and_then(|p| fs::read(p).ok());
    let (compiled, needs_cache) = match cached_bytes.as_ref() {
        Some(bytes) => {
            let cached = v8::script_compiler::CachedData::new(bytes);
            let mut compiler_source =
                v8::script_compiler::Source::new_with_cached_data(source_str, Some(&origin), cached);
            let module = v8::script_compiler::compile_module2(
                scope,
                &mut compiler_source,
                v8::script_compiler::CompileOptions::ConsumeCodeCache,
                v8::script_compiler::NoCacheReason::NoReason,
            );
            let rejected = compiler_source
                .get_cached_data()
                .map_or(true, |d| d.rejected());
            (module, rejected)
        }
        None => {
            let mut compiler_source = v8::script_compiler::Source::new(source_str, Some(&origin));
            (
                v8::script_compiler::compile_module(scope, &mut compiler_source),
                cache_path.is_some(),
            )
        }
    };
    // A SyntaxError leaves an exception pending: stop the whole walk rather than compile more
    // modules on top of it.
    let Some(module) = compiled else {
        return false;
    };
    if needs_cache {
        if let Some(p) = cache_path {
            let global = v8::Global::new(scope, module);
            PENDING_MODULE_CODE_CACHES.with(|q| q.borrow_mut().push((global, p)));
        }
    }

    let identity_hash = module.get_identity_hash().get();

    // Collect child specifiers as Rust strings before any mutable borrow of scope.
    let requests = module.get_module_requests();
    let child_specifiers: Vec<String> = (0..requests.length())
        .filter_map(|i| {
            let data = requests.get(scope, i)?;
            let request: v8::Local<v8::ModuleRequest> = data.try_into().ok()?;
            Some(request.get_specifier().to_rust_string_lossy(scope))
        })
        .collect();

    // Store the compiled module (breaks import cycles on re-entry).
    let global = v8::Global::new(scope, module);
    ESM_MODULE_REGISTRY.with(|r| r.borrow_mut().insert(path.to_string(), global));
    ESM_HASH_TO_PATH.with(|m| m.borrow_mut().insert(identity_hash, path.to_string()));

    // Resolve every dependency first so the ones served over HTTP can be fetched concurrently
    // (a dev-server boot walks hundreds of modules), then compile them in import order.
    let mut children: Vec<String> = Vec::with_capacity(child_specifiers.len());
    for spec in &child_specifiers {
        let key = resolve_esm_path(spec, Some(path));
        let known = ESM_MODULE_REGISTRY.with(|r| r.borrow().contains_key(&key));
        if !known && !children.contains(&key) {
            children.push(key);
        }
    }
    let http_children: Vec<(String, String)> = children
        .iter()
        .filter(|k| crate::esm_http::is_http(k))
        .map(|k| (http_fetch_url(k), k.clone()))
        .collect();
    let mut fetched: HashMap<String, Result<crate::esm_http::FetchedModule, String>> = http_children
        .iter()
        .map(|(_, k)| k.clone())
        .zip(crate::esm_http::fetch_modules(&http_children))
        .collect();

    for key in children {
        // A module compiled meanwhile (reached through a sibling's subtree) is done.
        if ESM_MODULE_REGISTRY.with(|r| r.borrow().contains_key(&key)) {
            continue;
        }
        // An unloadable child stays out of the registry; linking then throws a proper
        // error from resolve_module_callback.
        let content = match fetched.remove(&key) {
            Some(Ok(module)) => Some(http_module_source(module)),
            Some(Err(reason)) => {
                record_load_error(&key, reason);
                None
            }
            None => read_module_source(&key),
        };
        if let Some(content) = content {
            if !compile_module_graph(scope, &content, &key) {
                return false;
            }
        }
    }
    true
}

#[cfg(feature = "classic")]
fn create_ns_object<'a>(
    name: &str,
    declaration: Arc<RwLock<dyn Declaration>>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    let Some(name_str) = v8::String::new(scope, name) else {
        return v8::undefined(scope).into();
    };
    let tmpl = FunctionTemplate::new(scope, handle_ns_func);
    tmpl.set_class_name(name_str);
    let object_tmpl = tmpl.instance_template(scope);
    object_tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .query(handle_named_property_query)
            .getter(handle_named_property_getter)
            .setter(handle_named_property_setter),
    );
    object_tmpl.set_internal_field_count(2);

    let Some(object) = object_tmpl.new_instance(scope) else {
        return v8::undefined(scope).into();
    };
    let declaration = Box::new(DeclarationFFI::new(declaration));
    let ext = v8::External::new(scope, Box::into_raw(declaration) as _);
    object.set_internal_field(0, ext.into());

    let object_store = v8::Map::new(scope);
    object.set_internal_field(1, object_store.into());

    let ret = object;

    ret.into()
}

/// Properties exposed on the returned JS object:
///   .data1: UInt32
///   .data2: UInt16
///   .data3: UInt16
///   .data4: Array<number> (8 bytes)
///   .toString() / .valueOf(): "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}"
#[cfg(feature = "classic")]
unsafe fn guid_ptr_to_js_object<'a>(
    ptr: *mut std::ffi::c_void,
    scope: &mut v8::PinScope<'a, '_>,
) -> v8::Local<'a, v8::Object> {
    use windows::core::GUID;
    let g = &*(ptr as *const GUID);

    let guid_str = format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    );

    let obj = v8::Object::new(scope);

    // Scalar fields
    let key_data1 = v8::String::new(scope, "data1").unwrap();
    let val_data1 = v8::Integer::new_from_unsigned(scope, g.data1);
    obj.set(scope, key_data1.into(), val_data1.into());

    let key_data2 = v8::String::new(scope, "data2").unwrap();
    let val_data2 = v8::Integer::new_from_unsigned(scope, g.data2 as u32);
    obj.set(scope, key_data2.into(), val_data2.into());

    let key_data3 = v8::String::new(scope, "data3").unwrap();
    let val_data3 = v8::Integer::new_from_unsigned(scope, g.data3 as u32);
    obj.set(scope, key_data3.into(), val_data3.into());

    // data4 as a JS Array of 8 numbers
    let arr = v8::Array::new(scope, 8);
    for (i, &byte) in g.data4.iter().enumerate() {
        let byte_val = v8::Integer::new_from_unsigned(scope, byte as u32);
        arr.set_index(scope, i as u32, byte_val.into());
    }
    let key_data4 = v8::String::new(scope, "data4").unwrap();
    obj.set(scope, key_data4.into(), arr.into());

    // toString() / valueOf() both return the GUID string
    let guid_v8 = v8::String::new(scope, &guid_str).unwrap();
    let to_string_fn = v8::FunctionTemplate::builder(
        |_scope: &mut v8::PinScope<'_, '_>,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let s = unsafe { args.data().cast::<v8::String>() };
            retval.set(s.into());
        },
    )
    .data(guid_v8.into())
    .build(scope)
    .get_function(scope)
    .unwrap();

    let key_to_string = v8::String::new(scope, "toString").unwrap();
    obj.set(scope, key_to_string.into(), to_string_fn.into());

    let key_value_of = v8::String::new(scope, "valueOf").unwrap();
    obj.set(scope, key_value_of.into(), to_string_fn.into());

    obj
}

/// Captured data for calling a method on a `GenericInterfaceInstance` via the getter interceptor.
#[cfg(feature = "classic")]
struct IfaceMethodCallData {
    method: MethodDeclaration,
    instance: IUnknown,
    iid: GUID,
    type_args: Vec<String>,
}

/// Extract the comma-separated type arguments from a closed generic type name.
/// E.g. `IFoo`2<Windows.X.Bar, Windows.X.Baz>` → `["Windows.X.Bar", "Windows.X.Baz"]`
pub(crate) fn extract_generic_type_args(full_name: &str) -> Vec<String> {
    let Some(start) = full_name.find('<') else {
        return Vec::new();
    };
    let end = full_name.rfind('>').unwrap_or(full_name.len());
    let inner = &full_name[start + 1..end];
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for ch in inner.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = current.trim().to_owned();
                if !trimmed.is_empty() {
                    args.push(trimmed);
                }
                current = String::new();
            }
            _ => {
                current.push(ch);
            }
        }
    }
    let trimmed = current.trim().to_owned();
    if !trimmed.is_empty() {
        args.push(trimmed);
    }
    args
}

#[cfg(feature = "classic")]
fn create_ns_ctor_instance_object<'a>(
    name: &str,
    factory: Option<IUnknown>,
    parent: Option<Arc<RwLock<dyn Declaration>>>,
    declaration: Arc<RwLock<dyn Declaration>>,
    instance: Option<IUnknown>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    // COM identity key: QI(IID_IUnknown) gives the canonical pointer regardless of which
    // interface we hold.
    let identity_key: Option<usize> = instance
        .as_ref()
        .and_then(|unk| unk.cast::<IUnknown>().ok().map(|id| id.as_raw() as usize));
    if let Some(key) = identity_key {
        let hit = INSTANCE_CACHE.with(|cache| {
            cache
                .borrow()
                .get(&key)
                .and_then(|weak| weak.to_local(scope))
        });
        if let Some(local) = hit {
            return local.into();
        }
    }

    // Member callbacks resolve the COM instance from internal field 0, so a
    // template built for one instance serves all. The "L|" prefix keeps this
    // builder's templates separate from ns_proxy's: the interceptor sets differ.
    let template_key: String = match &parent {
        Some(p) => format!("L|{}|{}", name, p.read().full_name()),
        None => format!("L|{}", name),
    };
    let cached_tmpl: Option<v8::Global<v8::FunctionTemplate>> = scope
        .get_slot::<crate::ns_proxy::InstanceTemplateCache>()
        .and_then(|c| c.0.borrow().get(template_key.as_str()).cloned());
    if let Some(tmpl_global) = cached_tmpl {
        let tmpl = v8::Local::new(scope, &tmpl_global);
        return crate::ns_proxy::finish_instance_object(
            tmpl,
            declaration,
            instance,
            identity_key,
            scope,
        );
    }

    let class_name = v8::String::new(scope, name).unwrap();

    let tmpl = FunctionTemplate::new(scope, handle_ns_func);
    let object_tmpl = tmpl.instance_template(scope);

    // Two internal fields: [0] = DeclarationFFI external, [1] = per-instance side store (Map)
    object_tmpl.set_internal_field_count(2);

    let declaration_ffi = Box::into_raw(Box::new(DeclarationFFI::new_with_instance(
        declaration.clone(),
        instance.clone(),
    )));
    let ext = v8::External::new(scope, declaration_ffi as _);

    object_tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .getter(
                |scope: &mut v8::PinScope<'_, '_>,
                 key: Local<v8::Name>,
                 args: v8::PropertyCallbackArguments,
                 mut rv: v8::ReturnValue<v8::Value>|
                 -> v8::Intercepted {
                    if !key.is_string() {
                        return v8::Intercepted::kNo;
                    }

                    let name = key.to_rust_string_lossy(scope);
                    if name == "__probe__" {
                        let value = v8::String::new(scope, "instance-handler-active").unwrap();
                        rv.set(value.into());
                        return v8::Intercepted::kYes;
                    }
                    // Prefer the DeclarationFFI stored on the instance (holder internal field[0]).
                    // If the holder doesn't have the internal field (e.g. the property
                    // lives on the prototype), prefer the `this` object's internal
                    // DeclarationFFI when available. Fall back to the callback data
                    // otherwise.
                    let holder = args.holder();
                    let dec_field_opt = holder.get_internal_field(scope, 0);
                    let dec = if let Some(dec_field) = dec_field_opt {
                        let dec_ext = unsafe { dec_field.cast::<v8::External>() };
                        let dec_ptr = dec_ext.value() as *mut DeclarationFFI;
                        unsafe { &*dec_ptr }
                    } else {
                        // If the holder doesn't have the DeclarationFFI, fall back
                        // to the callback data. (Avoid using `args.this()` here to
                        // remain compatible with the v8 bindings available.)
                        let dec_ext = unsafe { args.data().cast::<v8::External>() };
                        let dec_ptr = dec_ext.value() as *mut DeclarationFFI;
                        unsafe { &*dec_ptr }
                    };

                    let lock = dec.read();

                    // Handle GenericInterfaceInstance properties and methods dynamically
                    if let Some(iface) = lock
                        .as_any()
                        .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                    {
                        let iid = iface.id();
                        let type_args = extract_generic_type_args(iface.full_name());

                        // Side-store check for JS-assigned values
                        let this = holder;
                        if let Some(store_field) = this.get_internal_field(scope, 1) {
                            let store = unsafe { store_field.cast::<v8::Map>() };
                            if let Some(cache) = store.get(scope, key.into()) {
                                if !cache.is_null_or_undefined() {
                                    rv.set(cache);
                                    return v8::Intercepted::kYes;
                                }
                            }
                        }

                        // Property getter (e.g. Completed, Progress)
                        if let Some(property) = iface
                            .properties()
                            .iter()
                            .find(|p| p.name() == name.as_str())
                        {
                            let property_clone = property.clone();
                            drop(lock);
                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                return v8::Intercepted::kNo;
                            };
                            let Some(mut property_call) = PropertyCall::new_for_interface(
                                &property_clone,
                                false,
                                ns_instance,
                                false,
                                iid,
                                type_args,
                            ) else {
                                return v8::Intercepted::kNo;
                            };
                            let (ret, result, _outs) = property_call.call_with_values(scope, &[]);
                            if ret.is_err() {
                                let detail = format!(
                                    "Property get '{}' failed: {} (0x{:08X})",
                                    name,
                                    ret.message(),
                                    ret.0 as u32
                                );
                                let message = v8::String::new(scope, &detail).unwrap();
                                let error = v8::Exception::error(scope, message);
                                scope.throw_exception(error);
                                return v8::Intercepted::kYes;
                            }
                            if property_call.is_void() {
                                rv.set_undefined();
                                return v8::Intercepted::kYes;
                            }
                            let return_sig = property_call.return_type().to_string();
                            if return_sig.contains('.') {
                                if let Some(declaration) =
                                    MetadataReader::find_by_name(return_sig.as_str())
                                {
                                    let ret_val: Local<v8::Value> = if matches!(
                                        declaration.read().kind(),
                                        DeclarationKind::Struct
                                    ) {
                                        create_struct_object_from_raw(declaration, result, scope)
                                            .into()
                                    } else if result.is_null() {
                                        v8::null(scope).into()
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        create_ns_ctor_instance_object(
                                            &return_sig,
                                            None,
                                            None,
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into()
                                    };
                                    rv.set(ret_val);
                                    return v8::Intercepted::kYes;
                                }
                            }
                            if let Ok(native_type) = NativeType::try_from(return_sig.as_str()) {
                                unsafe {
                                    set_ret_val(result, scope, rv, native_type);
                                }
                                return v8::Intercepted::kYes;
                            }
                            return v8::Intercepted::kNo;
                        }

                        // Method access: return a JS function that calls via QI + vtable
                        if let Some(method_decl) =
                            iface.methods().iter().find(|m| m.name() == name.as_str())
                        {
                            let method_clone = method_decl.clone();
                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                drop(lock);
                                return v8::Intercepted::kNo;
                            };
                            drop(lock);

                            let call_data = Box::into_raw(Box::new(IfaceMethodCallData {
                                method: method_clone,
                                instance: ns_instance,
                                iid,
                                type_args,
                            }));
                            let ext = v8::External::new(scope, call_data as _);

                            let func = v8::Function::builder(
                                |scope: &mut v8::PinScope<'_, '_>,
                                 args: v8::FunctionCallbackArguments,
                                 mut retval: v8::ReturnValue| {
                                    let data = unsafe {
                                        &*(args.data().cast::<v8::External>().value()
                                            as *const IfaceMethodCallData)
                                    };
                                    let Some(mut method_call) =
                                        PropertyCall::new_method_for_interface(
                                            &data.method,
                                            data.instance.clone(),
                                            data.iid,
                                            data.type_args.clone(),
                                        )
                                    else {
                                        return;
                                    };

                                    let mut arg_vals: Vec<Local<v8::Value>> =
                                        Vec::with_capacity(args.length() as usize);
                                    for i in 0..args.length() {
                                        arg_vals.push(args.get(i));
                                    }

                                    let (ret, result, _outs) =
                                        method_call.call_with_values(scope, &arg_vals);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg);
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if method_call.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    let return_sig = method_call.return_type().to_string();
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig.as_str())
                                        {
                                            let ret_val: Local<v8::Value> = if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into()
                                            } else if result.is_null() {
                                                v8::null(scope).into()
                                            } else {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                create_ns_ctor_instance_object(
                                                    &return_sig,
                                                    None,
                                                    None,
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into()
                                            };
                                            retval.set(ret_val);
                                            return;
                                        }
                                    }
                                    if let Ok(native_type) =
                                        NativeType::try_from(return_sig.as_str())
                                    {
                                        unsafe {
                                            set_ret_val(result, scope, retval, native_type);
                                        }
                                    }
                                },
                            )
                            .data(ext.into())
                            .build(scope)
                            .unwrap();

                            let func: Local<v8::Value> = func.into();
                            if let Some(store_field) = holder.get_internal_field(scope, 1) {
                                let store = unsafe { store_field.cast::<v8::Map>() };
                                store.set(scope, key.into(), func);
                            }
                            rv.set(func);
                            return v8::Intercepted::kYes;
                        }

                        return v8::Intercepted::kNo;
                    }

                    // Handle plain (non-generic) interface instances returned from properties/methods.
                    // e.g. IHttpContent returned by HttpResponseMessage.Content.
                    if let Some(iface) = lock.as_any().downcast_ref::<InterfaceDeclaration>() {
                        let iid = iface.id();
                        let type_args: Vec<String> = vec![];

                        let this = holder;
                        if let Some(store_field) = this.get_internal_field(scope, 1) {
                            let store = unsafe { store_field.cast::<v8::Map>() };
                            if let Some(cache) = store.get(scope, key.into()) {
                                if !cache.is_null_or_undefined() {
                                    rv.set(cache);
                                    return v8::Intercepted::kYes;
                                }
                            }
                        }

                        if let Some(property) = iface
                            .properties()
                            .iter()
                            .find(|p| p.name() == name.as_str())
                        {
                            let property_clone = property.clone();
                            drop(lock);
                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                return v8::Intercepted::kNo;
                            };
                            let Some(mut property_call) = PropertyCall::new_for_interface(
                                &property_clone,
                                false,
                                ns_instance,
                                false,
                                iid,
                                type_args,
                            ) else {
                                return v8::Intercepted::kNo;
                            };
                            let (ret, result, _outs) = property_call.call_with_values(scope, &[]);
                            if ret.is_err() {
                                let detail = format!(
                                    "Property get '{}' failed: {} (0x{:08X})",
                                    name,
                                    ret.message(),
                                    ret.0 as u32
                                );
                                let message = v8::String::new(scope, &detail).unwrap();
                                let error = v8::Exception::error(scope, message);
                                scope.throw_exception(error);
                                return v8::Intercepted::kYes;
                            }
                            if property_call.is_void() {
                                rv.set_undefined();
                                return v8::Intercepted::kYes;
                            }
                            let return_sig = property_call.return_type().to_string();
                            if return_sig.contains('.') {
                                if let Some(declaration) =
                                    MetadataReader::find_by_name(return_sig.as_str())
                                {
                                    let ret_val: Local<v8::Value> = if matches!(
                                        declaration.read().kind(),
                                        DeclarationKind::Struct
                                    ) {
                                        create_struct_object_from_raw(declaration, result, scope)
                                            .into()
                                    } else if result.is_null() {
                                        v8::null(scope).into()
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        create_ns_ctor_instance_object(
                                            &return_sig,
                                            None,
                                            None,
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into()
                                    };
                                    rv.set(ret_val);
                                    return v8::Intercepted::kYes;
                                }
                            }
                            if let Ok(native_type) = NativeType::try_from(return_sig.as_str()) {
                                unsafe {
                                    set_ret_val(result, scope, rv, native_type);
                                }
                                return v8::Intercepted::kYes;
                            }
                            return v8::Intercepted::kNo;
                        }

                        if let Some(method_decl) =
                            iface.methods().iter().find(|m| m.name() == name.as_str())
                        {
                            let method_clone = method_decl.clone();
                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                drop(lock);
                                return v8::Intercepted::kNo;
                            };
                            drop(lock);

                            let call_data = Box::into_raw(Box::new(IfaceMethodCallData {
                                method: method_clone,
                                instance: ns_instance,
                                iid,
                                type_args,
                            }));
                            let ext = v8::External::new(scope, call_data as _);

                            let func = v8::Function::builder(
                                |scope: &mut v8::PinScope<'_, '_>,
                                 args: v8::FunctionCallbackArguments,
                                 mut retval: v8::ReturnValue| {
                                    let data = unsafe {
                                        &*(args.data().cast::<v8::External>().value()
                                            as *const IfaceMethodCallData)
                                    };
                                    let Some(mut method_call) =
                                        PropertyCall::new_method_for_interface(
                                            &data.method,
                                            data.instance.clone(),
                                            data.iid,
                                            data.type_args.clone(),
                                        )
                                    else {
                                        return;
                                    };

                                    let mut arg_vals: Vec<Local<v8::Value>> =
                                        Vec::with_capacity(args.length() as usize);
                                    for i in 0..args.length() {
                                        arg_vals.push(args.get(i));
                                    }

                                    let (ret, result, _outs) =
                                        method_call.call_with_values(scope, &arg_vals);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg);
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if method_call.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    let return_sig = method_call.return_type().to_string();
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig.as_str())
                                        {
                                            let ret_val: Local<v8::Value> = if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into()
                                            } else if result.is_null() {
                                                v8::null(scope).into()
                                            } else {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                create_ns_ctor_instance_object(
                                                    &return_sig,
                                                    None,
                                                    None,
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into()
                                            };
                                            retval.set(ret_val);
                                            return;
                                        }
                                    }
                                    if let Ok(native_type) =
                                        NativeType::try_from(return_sig.as_str())
                                    {
                                        unsafe {
                                            set_ret_val(result, scope, retval, native_type);
                                        }
                                    }
                                },
                            )
                            .data(ext.into())
                            .build(scope)
                            .unwrap();

                            let func: Local<v8::Value> = func.into();
                            if let Some(store_field) = holder.get_internal_field(scope, 1) {
                                let store = unsafe { store_field.cast::<v8::Map>() };
                                store.set(scope, key.into(), func);
                            }
                            rv.set(func);
                            return v8::Intercepted::kYes;
                        }

                        return v8::Intercepted::kNo;
                    }

                    let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
                        if crate::class_helpers::find_interface_event_methods(&*lock, &name)
                            .is_some()
                        {
                            let handler = crate::ns_proxy::read_winrt_event(
                                scope,
                                dec.instance.as_ref(),
                                &name,
                            );
                            rv.set(handler);
                            return v8::Intercepted::kYes;
                        }
                        return v8::Intercepted::kNo;
                    };

                    // If a JS-assigned override exists in the per-instance store, return it.
                    // Use the holder (where the property was found) to access the side-store map.
                    let this = holder;
                    let store_field_opt = this.get_internal_field(scope, 1);
                    if let Some(store_field) = store_field_opt {
                        let store = unsafe { store_field.cast::<v8::Map>() };
                        if let Some(cache) = store.get(scope, key.into()) {
                            if !cache.is_null_or_undefined() {
                                rv.set(cache);
                                return v8::Intercepted::kYes;
                            }
                        }
                    }

                    if let Some(property) = find_class_property(clazz, &name) {
                        let Some(ns_instance) =
                            crate::ns_proxy::this_instance(scope, args.this_object())
                                .or_else(|| dec.instance.clone())
                        else {
                            return v8::Intercepted::kNo;
                        };
                        let Some(mut property_call) =
                            PropertyCall::new(&property, false, ns_instance, false)
                        else {
                            return v8::Intercepted::kNo;
                        };
                        let (ret, result, _outs) = property_call.call_with_values(scope, &[]);

                        if ret.is_err() {
                            let detail = format!(
                                "Property get '{}' failed: {} (0x{:08X})",
                                name,
                                ret.message(),
                                ret.0 as u32
                            );
                            let message = v8::String::new(scope, &detail).unwrap();
                            let error = v8::Exception::error(scope, message);
                            scope.throw_exception(error);
                            return v8::Intercepted::kYes;
                        }

                        if property_call.is_void() {
                            rv.set_undefined();
                            return v8::Intercepted::kYes;
                        }

                        let return_sig = property_call.return_type().to_string();
                        if return_sig.contains('.') {
                            if let Some(declaration) =
                                MetadataReader::find_by_name(return_sig.as_str())
                            {
                                let ret: Local<v8::Value> =
                                    if matches!(declaration.read().kind(), DeclarationKind::Struct)
                                    {
                                        create_struct_object_from_raw(declaration, result, scope)
                                            .into()
                                    } else if result.is_null() {
                                        v8::null(scope).into()
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        create_ns_ctor_instance_object(
                                            return_sig.as_str(),
                                            None,
                                            None,
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into()
                                    };
                                rv.set(ret);
                                return v8::Intercepted::kYes;
                            }
                        }

                        if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                            unsafe {
                                set_ret_val(result, scope, rv, return_type);
                            }
                            return v8::Intercepted::kYes;
                        }

                        return v8::Intercepted::kNo;
                    }

                    if let Some(method) = find_class_method(clazz, &name) {

                        crate::class_helpers::register_overload_siblings(clazz, &name, &method);
                        let declaration = Arc::new(RwLock::new(method.clone()));
                        let declaration = Box::into_raw(Box::new(
                            DeclarationFFI::new_with_instance(declaration, dec.instance.clone()),
                        ));
                        let ext = v8::External::new(scope, declaration as _);

                        let builder = v8::Function::builder(
                            |scope: &mut v8::PinScope<'_, '_>,
                             args: v8::FunctionCallbackArguments,
                             mut retval: v8::ReturnValue| {
                                let dec = unsafe { args.data().cast::<v8::External>() };
                                let dec = dec.value() as *mut DeclarationFFI;
                                let dec = unsafe { &*dec };
                                let lock = dec.read();
                                let Some(method) =
                                    lock.as_any().downcast_ref::<MethodDeclaration>()
                                else {
                                    return;
                                };
                                let Some(ns_instance) =
                                    crate::ns_proxy::this_instance(scope, args.this_object())
                                        .or_else(|| dec.instance.clone())
                                else {
                                    return;
                                };
                                let mut method =
                                    {
                                    let alt = crate::class_helpers::overload_for_argc(method, args.length() as usize);
                                    let method = alt.as_ref().unwrap_or(method);
                                    MethodCall::new(method, method.is_sealed(), ns_instance, false)
                                };
                                let (ret, result, outs) = method.call(scope, &args);

                                if ret.is_err() {
                                    let detail = crate::error::format_hresult_message(ret);
                                    let message = v8::String::new(scope, &detail).unwrap();
                                    let error = v8::Exception::error(scope, message);
                                    scope.throw_exception(error);
                                    return;
                                }

                                // If there are out-parameters, return an array containing the
                                // primary return (if present) followed by the out-values.
                                if !outs.is_empty() {
                                    let mut arr_len = outs.len();
                                    if !method.is_void() {
                                        arr_len += 1;
                                    }
                                    let arr = v8::Array::new(scope, arr_len as i32);
                                    let mut idx = 0u32;

                                    if !method.is_void() {
                                        let return_sig = method.return_type().to_string();
                                        let mut return_value_opt: Option<Local<v8::Value>> = None;
                                        if return_sig.contains('.') {
                                            if let Some(declaration) =
                                                MetadataReader::find_by_name(return_sig.as_str())
                                            {
                                                if matches!(
                                                    declaration.read().kind(),
                                                    DeclarationKind::Struct
                                                ) {
                                                    let obj = crate::create_struct_object_from_raw(
                                                        declaration,
                                                        result,
                                                        scope,
                                                    )
                                                    .into();
                                                    return_value_opt = Some(obj);
                                                } else if !result.is_null() {
                                                    let instance =
                                                        unsafe { IUnknown::from_raw(result) };
                                                    let retv: Local<v8::Value> =
                                                        create_ns_ctor_instance_object(
                                                            return_sig.as_str(),
                                                            None,
                                                            dec.parent.clone(),
                                                            declaration,
                                                            Some(instance),
                                                            scope,
                                                        )
                                                        .into();
                                                    return_value_opt = Some(retv);
                                                } else {
                                                    return_value_opt = Some(v8::null(scope).into());
                                                }
                                            }
                                        }
                                        if return_value_opt.is_none() {
                                            if let Ok(return_type) =
                                                NativeType::try_from(return_sig.as_str())
                                            {
                                                let v = unsafe {
                                                    read_value_from_ptr(
                                                        result as *const c_void,
                                                        scope,
                                                        return_type,
                                                    )
                                                };
                                                return_value_opt = Some(v);
                                            }
                                        }
                                        if let Some(rv) = return_value_opt {
                                            arr.set_index(scope, idx, rv);
                                            idx += 1;
                                        }
                                    }

                                    for outv in outs.into_iter() {
                                        arr.set_index(scope, idx, outv);
                                        idx += 1;
                                    }
                                    retval.set(arr.into());
                                    return;
                                }

                                if method.is_void() {
                                    retval.set_undefined();
                                    return;
                                }

                                let return_sig = method.return_type().to_string();
                                if return_sig.contains('.') {
                                    if let Some(declaration) =
                                        MetadataReader::find_by_name(return_sig.as_str())
                                    {
                                        let ret: Local<v8::Value> = if matches!(
                                            declaration.read().kind(),
                                            DeclarationKind::Struct
                                        ) {
                                            create_struct_object_from_raw(
                                                declaration,
                                                result,
                                                scope,
                                            )
                                            .into()
                                        } else if result.is_null() {
                                            v8::null(scope).into()
                                        } else {
                                            let instance = unsafe { IUnknown::from_raw(result) };
                                            create_ns_ctor_instance_object(
                                                return_sig.as_str(),
                                                None,
                                                dec.parent.clone(),
                                                declaration,
                                                Some(instance),
                                                scope,
                                            )
                                            .into()
                                        };
                                        retval.set(ret);
                                        return;
                                    }
                                }

                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                    unsafe {
                                        set_ret_val(result, scope, retval, return_type);
                                    }
                                }
                            },
                        )
                        .data(ext.into())
                        .build(scope)
                        .unwrap();

                        let func: Local<v8::Value> = builder.into();
                        if let Some(store_field) = holder.get_internal_field(scope, 1) {
                            let store = unsafe { store_field.cast::<v8::Map>() };
                            store.set(scope, key.into(), func);
                        }
                        rv.set(func);
                        return v8::Intercepted::kYes;
                    }

                    if find_event_methods(clazz, &name).is_some() {
                        let handler =
                            crate::ns_proxy::read_winrt_event(scope, dec.instance.as_ref(), &name);
                        rv.set(handler);
                        return v8::Intercepted::kYes;
                    }

                    v8::Intercepted::kNo
                },
            )
            .setter(
                |scope: &mut v8::PinScope<'_, '_>,
                 key: Local<v8::Name>,
                 val: Local<v8::Value>,
                 args: v8::PropertyCallbackArguments,
                 mut _rv: v8::ReturnValue<()>|
                 -> v8::Intercepted {
                    if !key.is_string() {
                        return v8::Intercepted::kNo;
                    }

                    let name = key.to_rust_string_lossy(scope);
                    // Prefer the per-instance DeclarationFFI stored on the holder
                    // (internal field 0). The callback data is baked into the per-class
                    // cached template and holds the instance the template was first
                    // built with: using it would attach events to the wrong object.
                    let dec_ptr = crate::ns_proxy::this_declaration_ffi(scope, args.holder())
                        .unwrap_or_else(|| {
                            unsafe { args.data().cast::<v8::External>() }.value()
                                as *mut DeclarationFFI
                        });
                    let dec = unsafe { &mut *dec_ptr };
                    let lock = dec.read();

                    if let Some(iface) = lock
                        .as_any()
                        .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                    {
                        let iid = iface.id();
                        let type_args = extract_generic_type_args(iface.full_name());
                        if let Some(property) = iface
                            .properties()
                            .iter()
                            .find(|p| p.name() == name.as_str())
                        {
                            if property.setter().is_some() {
                                let property_clone = property.clone();
                                drop(lock);
                                let Some(ns_instance) =
                                    crate::ns_proxy::this_instance(scope, args.this_object())
                                        .or_else(|| dec.instance.clone())
                                else {
                                    return v8::Intercepted::kNo;
                                };
                                let Some(mut property_call) = PropertyCall::new_for_interface(
                                    &property_clone,
                                    true,
                                    ns_instance,
                                    false,
                                    iid,
                                    type_args,
                                ) else {
                                    return v8::Intercepted::kNo;
                                };
                                let (ret, _, _outs) = property_call.call_with_values(scope, &[val]);
                                if ret.is_err() {
                                    let detail = format!(
                                        "Property set '{}' failed: {} (0x{:08X})",
                                        name,
                                        ret.message(),
                                        ret.0 as u32
                                    );
                                    let message = v8::String::new(scope, &detail).unwrap();
                                    let error = v8::Exception::error(scope, message);
                                    scope.throw_exception(error);
                                }
                                return v8::Intercepted::kYes;
                            }
                        }
                        return v8::Intercepted::kNo;
                    }

                    if let Some(iface) = lock.as_any().downcast_ref::<InterfaceDeclaration>() {
                        let iid = iface.id();
                        let type_args: Vec<String> = vec![];
                        if let Some(property) = iface
                            .properties()
                            .iter()
                            .find(|p| p.name() == name.as_str())
                        {
                            if property.setter().is_some() {
                                let property_clone = property.clone();
                                drop(lock);
                                let Some(ns_instance) =
                                    crate::ns_proxy::this_instance(scope, args.this_object())
                                        .or_else(|| dec.instance.clone())
                                else {
                                    return v8::Intercepted::kNo;
                                };
                                let Some(mut property_call) = PropertyCall::new_for_interface(
                                    &property_clone,
                                    true,
                                    ns_instance,
                                    false,
                                    iid,
                                    type_args,
                                ) else {
                                    return v8::Intercepted::kNo;
                                };
                                let (ret, _, _outs) = property_call.call_with_values(scope, &[val]);
                                if ret.is_err() {
                                    let detail = format!(
                                        "Property set '{}' failed: {} (0x{:08X})",
                                        name,
                                        ret.message(),
                                        ret.0 as u32
                                    );
                                    let message = v8::String::new(scope, &detail).unwrap();
                                    let error = v8::Exception::error(scope, message);
                                    scope.throw_exception(error);
                                }
                                return v8::Intercepted::kYes;
                            }
                        }
                        return v8::Intercepted::kNo;
                    }

                    let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
                        if let Some((add_method, remove_method)) =
                            crate::class_helpers::find_interface_event_methods(&*lock, &name)
                        {
                            let instance = dec.instance.clone();
                            drop(lock);
                            return crate::ns_proxy::wire_winrt_event(
                                scope,
                                &name,
                                instance,
                                &add_method,
                                &remove_method,
                                val,
                            );
                        }
                        return v8::Intercepted::kNo;
                    };

                    if let Some(property) = find_class_property(clazz, &name) {
                        if property.setter().is_none() {
                            return v8::Intercepted::kNo;
                        }

                        let Some(ns_instance) =
                            crate::ns_proxy::this_instance(scope, args.this_object())
                                .or_else(|| dec.instance.clone())
                        else {
                            return v8::Intercepted::kNo;
                        };
                        let Some(mut property_call) =
                            PropertyCall::new(&property, true, ns_instance, false)
                        else {
                            return v8::Intercepted::kNo;
                        };
                        let (ret, _, _outs) = property_call.call_with_values(scope, &[val]);
                        if ret.is_err() {
                            let detail = format!(
                                "Property set '{}' failed: {} (0x{:08X})",
                                name,
                                ret.message(),
                                ret.0 as u32
                            );
                            let message = v8::String::new(scope, &detail).unwrap();
                            let error = v8::Exception::error(scope, message);
                            scope.throw_exception(error);
                        }
                        return v8::Intercepted::kYes;
                    }

                    // Try WinRT events: `instance.EventName = new DelegateType({ Invoke: fn })`.
                    if let Some((add_method, remove_method)) = find_event_methods(clazz, &name) {
                        let instance = dec.instance.clone();
                        drop(lock);
                        return crate::ns_proxy::wire_winrt_event(
                            scope,
                            &name,
                            instance,
                            &add_method,
                            &remove_method,
                            val,
                        );
                    }

                    v8::Intercepted::kNo
                },
            )
            .data(ext.into()),
    );

    tmpl.set_class_name(class_name);

    let proto = tmpl.prototype_template(scope);

    {
        let lock = declaration.read();

        let kind = lock.kind();

        match kind {
            DeclarationKind::Class => {
                let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
                    return v8::undefined(scope).into();
                };
                let class_methods = collect_class_methods(clazz);
                let class_properties = collect_class_properties(clazz);
                let mut seen_member_names: AHashSet<String> = AHashSet::new();

                let to_string_func = FunctionTemplate::builder(
                    |_scope: &mut v8::PinScope<'_, '_>,
                     args: v8::FunctionCallbackArguments,
                     mut retval: v8::ReturnValue| {
                        retval.set(args.data());
                    },
                )
                .data(class_name.into())
                .build(scope);

                let to_string = v8::String::new(scope, "toString").unwrap();

                // Instance-template properties are copied onto every object at
                // new_instance; prototype members are shared.
                proto.set(to_string.into(), to_string_func.into());

                for method in class_methods.iter() {
                    let method_name = if method.overload_name().is_empty() {
                        method.name().to_string()
                    } else {
                        method.overload_name().to_string()
                    };

                    let is_static = method.is_static();

                    let key = format!(
                        "{}:{}",
                        method_name,
                        if is_static { "static" } else { "instance" }
                    );
                    if !seen_member_names.insert(key) {
                        continue;
                    }

                    let name = v8::String::new(scope, method_name.as_str());

                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(method.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );

                    let declaration = Box::into_raw(Box::new(declaration));

                    let ext = v8::External::new(scope, declaration as _);

                    extern "C" fn callback(callback: *const v8::FunctionCallbackInfo) {
                        let info = unsafe { &*callback };

                        v8::callback_scope!(unsafe scope, info);
                        let args = unsafe {
                            v8::FunctionCallbackArguments::from_function_callback_info(info)
                        };
                        let mut retval = v8::ReturnValue::from_function_callback_info(info);

                        let dec = unsafe { args.data().cast::<v8::External>() };

                        let dec = dec.value() as *mut DeclarationFFI;

                        let dec = unsafe { &*dec };

                        let lock = dec.read();

                        let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>() else {
                            return;
                        };

                        let _nam = method.name();
                        let Some(ns_instance) =
                            crate::ns_proxy::this_instance(scope, args.this_object())
                                .or_else(|| dec.instance.clone())
                        else {
                            return;
                        };
                        let mut method =
                            {
                                    let alt = crate::class_helpers::overload_for_argc(method, args.length() as usize);
                                    let method = alt.as_ref().unwrap_or(method);
                                    MethodCall::new(method, method.is_sealed(), ns_instance, false)
                                };

                        let (ret, result, outs) = method.call(scope, &args);

                        if ret.is_err() {
                            let detail = crate::error::format_hresult_message(ret);
                            let msg = v8::String::new(scope, &detail).unwrap();
                            let err = v8::Exception::error(scope, msg.into());
                            scope.throw_exception(err);
                            return;
                        }

                        if !outs.is_empty() {
                            let mut arr_len = outs.len();
                            if !method.is_void() {
                                arr_len += 1;
                            }
                            let arr = v8::Array::new(scope, arr_len as i32);
                            let mut idx = 0u32;

                            if !method.is_void() {
                                let return_sig = method.return_type().to_string();
                                let mut return_value_opt: Option<Local<v8::Value>> = None;
                                if return_sig.contains('.') {
                                    if let Some(declaration) =
                                        MetadataReader::find_by_name(return_sig.as_str())
                                    {
                                        if matches!(
                                            declaration.read().kind(),
                                            DeclarationKind::Struct
                                        ) {
                                            let obj = crate::create_struct_object_from_raw(
                                                declaration,
                                                result,
                                                scope,
                                            )
                                            .into();
                                            return_value_opt = Some(obj);
                                        } else if !result.is_null() {
                                            let instance = unsafe { IUnknown::from_raw(result) };
                                            let retv: Local<v8::Value> =
                                                create_ns_ctor_instance_object(
                                                    return_sig.as_str(),
                                                    None,
                                                    dec.parent.clone(),
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into();
                                            return_value_opt = Some(retv);
                                        } else {
                                            return_value_opt = Some(v8::null(scope).into());
                                        }
                                    }
                                }
                                if return_value_opt.is_none() {
                                    if let Ok(return_type) =
                                        NativeType::try_from(return_sig.as_str())
                                    {
                                        let v = unsafe {
                                            read_value_from_ptr(
                                                result as *const c_void,
                                                scope,
                                                return_type,
                                            )
                                        };
                                        return_value_opt = Some(v);
                                    }
                                }
                                if let Some(rv) = return_value_opt {
                                    arr.set_index(scope, idx, rv);
                                    idx += 1;
                                }
                            }

                            for outv in outs.into_iter() {
                                arr.set_index(scope, idx, outv);
                                idx += 1;
                            }
                            retval.set(arr.into());
                            return;
                        }

                        if method.is_void() {
                            retval.set_undefined();
                            return;
                        }

                        let return_sig = method.return_type().to_string();
                        if return_sig == "Guid" {
                            let obj = unsafe { guid_ptr_to_js_object(result, scope) };
                            retval.set(obj.into());
                        } else if return_sig == "Object" && !result.is_null() {
                            // Methods declared to return `Object`/IInspectable (e.g. XamlReader.Load)
                            // hand back an opaque pointer whose concrete type is only known at runtime.
                            // Resolve it via GetRuntimeClassName and wrap as a full typed proxy so
                            // property/event interceptors work: otherwise the caller gets a
                            // non-extensible object that can't subscribe to events.
                            let instance = unsafe { IUnknown::from_raw(result) };
                            let resolved = instance
                                .cast::<IInspectable>()
                                .ok()
                                .and_then(|insp| insp.GetRuntimeClassName().ok())
                                .map(|cn| cn.to_string())
                                .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
                                .filter(|(_, d)| {
                                    !matches!(d.read().kind(), DeclarationKind::Struct)
                                });
                            match resolved {
                                Some((cname, decl)) => {
                                    let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                                        cname.as_str(),
                                        None,
                                        dec.parent.clone(),
                                        decl,
                                        Some(instance),
                                        scope,
                                    )
                                    .into();
                                    retval.set(ret.into());
                                }
                                None => {
                                    // Keep the ref alive; fall back to the generic pointer wrapper.
                                    let _ = std::mem::ManuallyDrop::new(instance);
                                    unsafe {
                                        set_ret_val(result, scope, retval, NativeType::Pointer);
                                    }
                                }
                            }
                        } else {
                            match NativeType::try_from(return_sig.as_str()) {
                                Ok(return_type) => {
                                    if return_sig.contains('.') {
                                        if result.is_null() {
                                            retval.set(v8::null(scope).into());
                                            return;
                                        }
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        let declaration =
                                            MetadataReader::find_by_name(return_sig.as_str())
                                                .unwrap_or_else(|| dec.inner.clone());
                                        let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                                            return_sig.as_str(),
                                            None,
                                            dec.parent.clone(),
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into();
                                        retval.set(ret.into());
                                        return;
                                    }
                                    unsafe {
                                        set_ret_val(result, scope, retval, return_type);
                                    }
                                }
                                Err(_) => {}
                            }
                        }

                        // todo
                    }

                    let func = FunctionTemplate::builder_raw(callback)
                        .data(ext.into())
                        .build(scope);

                    if is_static {
                        tmpl.set_with_attr(
                            name.unwrap().into(),
                            func.into(),
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        // Prototype, not instance template: see toString above.
                        proto.set_with_attr(
                            name.unwrap().into(),
                            func.into(),
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    }
                }

                for property in class_properties.iter() {
                    let property_name = property.name().to_string();
                    let is_static = property.is_static();

                    let key = format!(
                        "{}:{}",
                        property_name,
                        if is_static { "static" } else { "instance" }
                    );
                    if !seen_member_names.insert(key) {
                        continue;
                    }

                    let name = v8::String::new(scope, property_name.as_str());

                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(property.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );

                    let getter_declaration = declaration.clone();

                    let getter_declaration = Box::into_raw(Box::new(getter_declaration));

                    let getter_declaration_ext = v8::External::new(scope, getter_declaration as _);

                    let getter = FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };

                            let dec = dec.value() as *mut DeclarationFFI;

                            let dec = unsafe { &*dec };

                            let lock = dec.read();

                            let Some(method) = lock.as_any().downcast_ref::<PropertyDeclaration>()
                            else {
                                return;
                            };

                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let Some(mut method) =
                                PropertyCall::new(method, false, ns_instance, false)
                            else {
                                return;
                            };

                            let (ret, result, outs) = method.call(scope, &args);

                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            }

                            if !outs.is_empty() {
                                let mut arr_len = outs.len();
                                if !method.is_void() {
                                    arr_len += 1;
                                }
                                let arr = v8::Array::new(scope, arr_len as i32);
                                let mut idx = 0u32;

                                if !method.is_void() {
                                    let return_sig = method.return_type().to_string();
                                    let mut return_value_opt: Option<Local<v8::Value>> = None;
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig.as_str())
                                        {
                                            if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                let obj = crate::create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into();
                                                return_value_opt = Some(obj);
                                            } else if !result.is_null() {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                let retv: Local<v8::Value> =
                                                    create_ns_ctor_instance_object(
                                                        return_sig.as_str(),
                                                        None,
                                                        None,
                                                        declaration,
                                                        Some(instance),
                                                        scope,
                                                    )
                                                    .into();
                                                return_value_opt = Some(retv);
                                            } else {
                                                return_value_opt = Some(v8::null(scope).into());
                                            }
                                        }
                                    }
                                    if return_value_opt.is_none() {
                                        if let Ok(return_type) =
                                            NativeType::try_from(return_sig.as_str())
                                        {
                                            let v = unsafe {
                                                read_value_from_ptr(
                                                    result as *const c_void,
                                                    scope,
                                                    return_type,
                                                )
                                            };
                                            return_value_opt = Some(v);
                                        }
                                    }
                                    if let Some(rv) = return_value_opt {
                                        arr.set_index(scope, idx, rv);
                                        idx += 1;
                                    }
                                }

                                for outv in outs.into_iter() {
                                    arr.set_index(scope, idx, outv);
                                    idx += 1;
                                }
                                retval.set(arr.into());
                                return;
                            }

                            if method.is_void() {
                                retval.set_undefined();
                                return;
                            }

                            let return_sig = method.return_type().to_string();
                            if return_sig.contains('.') {
                                if let Some(declaration) =
                                    MetadataReader::find_by_name(return_sig.as_str())
                                {
                                    let ret: Local<v8::Value> = if matches!(
                                        declaration.read().kind(),
                                        DeclarationKind::Struct
                                    ) {
                                        create_struct_object_from_raw(declaration, result, scope)
                                            .into()
                                    } else if result.is_null() {
                                        v8::null(scope).into()
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        create_ns_ctor_instance_object(
                                            return_sig.as_str(),
                                            None,
                                            None,
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into()
                                    };
                                    retval.set(ret.into());
                                    return;
                                }
                            }

                            match NativeType::try_from(return_sig.as_str()) {
                                Ok(return_type) => unsafe {
                                    set_ret_val(result, scope, retval, return_type);
                                },
                                Err(_) => {}
                            }
                        },
                    )
                    .data(getter_declaration_ext.into())
                    .build(scope);

                    let mut setter: Option<Local<FunctionTemplate>> = None;

                    if property.setter().is_some() {
                        let setter_declaration = declaration;

                        let setter_declaration = Box::into_raw(Box::new(setter_declaration));

                        let setter_declaration_ext =
                            v8::External::new(scope, setter_declaration as _);

                        setter = Some(
                            FunctionTemplate::builder(
                                |scope: &mut v8::PinScope<'_, '_>,
                                 args: v8::FunctionCallbackArguments,
                                 _retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let Some(prop) =
                                        lock.as_any().downcast_ref::<PropertyDeclaration>()
                                    else {
                                        return;
                                    };
                                    let Some(ns_instance) =
                                        crate::ns_proxy::this_instance(scope, args.this_object())
                                            .or_else(|| dec.instance.clone())
                                    else {
                                        return;
                                    };
                                    let Some(mut method) =
                                        PropertyCall::new(prop, true, ns_instance, false)
                                    else {
                                        return;
                                    };
                                    let (ret, _, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg);
                                        scope.throw_exception(err);
                                    }
                                },
                            )
                            .data(setter_declaration_ext.into())
                            .build(scope),
                        );
                    }

                    if property.is_static() {
                        // Static properties live on the constructor, not the prototype.
                        let name = name.unwrap();
                        tmpl.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        let name = name.unwrap();
                        // Prototype, not instance template: see toString above.
                        proto.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::NONE,
                        );
                    }
                }
            }
            DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance => {
                // SAFETY: outer match arm filtered to exactly these three kinds.
                let clazz: &dyn BaseClassDeclarationImpl = match kind {
                    DeclarationKind::Interface => {
                        match lock.as_any().downcast_ref::<InterfaceDeclaration>() {
                            Some(d) => d,
                            None => return v8::undefined(scope).into(),
                        }
                    }
                    DeclarationKind::GenericInterface => {
                        match lock.as_any().downcast_ref::<GenericInterfaceDeclaration>() {
                            Some(d) => d,
                            None => return v8::undefined(scope).into(),
                        }
                    }
                    DeclarationKind::GenericInterfaceInstance => {
                        match lock
                            .as_any()
                            .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                        {
                            Some(d) => d,
                            None => return v8::undefined(scope).into(),
                        }
                    }
                    _ => unsafe { std::hint::unreachable_unchecked() },
                };

                let to_string_func = FunctionTemplate::builder(
                    |_scope: &mut v8::PinScope<'_, '_>,
                     args: v8::FunctionCallbackArguments,
                     mut retval: v8::ReturnValue| {
                        retval.set(args.data());
                    },
                )
                .data(class_name.into())
                .build(scope);

                let to_string = v8::String::new(scope, "toString").unwrap();
                proto.set(to_string.into(), to_string_func.into());

                if let Some(clazz) = parent {
                    let clazz = clazz.read();
                    let kind = clazz.kind();

                    match kind {
                        DeclarationKind::Class => {
                            if let Some(clazz) = clazz.as_any().downcast_ref::<ClassDeclaration>() {
                                for method in clazz.methods().iter() {
                                    let name = v8::String::new(scope, method.name());
                                    let is_static = method.is_static();

                                    let declaration = DeclarationFFI::new_with_instance(
                                        Arc::new(RwLock::new(method.clone())),
                                        if is_static {
                                            factory.clone()
                                        } else {
                                            instance.clone()
                                        },
                                    );

                                    let declaration = Box::into_raw(Box::new(declaration));

                                    let ext = v8::External::new(scope, declaration as _);

                                    let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                          args: v8::FunctionCallbackArguments,
                                                                          mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };

                                    let dec = dec.value() as *mut DeclarationFFI;

                                    let dec = unsafe { &*dec };

                                    let lock = dec.read();

                                    let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>() else { return; };

                                    let Some(ns_instance) = crate::ns_proxy::this_instance(scope, args.this_object()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(
                                        method, method.is_sealed(), ns_instance, false,
                                    );

                                    let (ret, result, outs) = method.call(scope, &args);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if !outs.is_empty() {
                                        let mut arr_len = outs.len();
                                        if !method.is_void() { arr_len += 1; }
                                        let arr = v8::Array::new(scope, arr_len as i32);
                                        let mut idx = 0u32;

                                        if !method.is_void() {
                                            let return_sig = method.return_type().to_string();
                                            let mut return_value_opt: Option<Local<v8::Value>> = None;
                                            if return_sig.contains('.') {
                                                if let Some(declaration) = MetadataReader::find_by_name(return_sig.as_str()) {
                                                    if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                                        let obj = crate::create_struct_object_from_raw(declaration, result, scope).into();
                                                        return_value_opt = Some(obj);
                                                    } else if !result.is_null() {
                                                        let instance = unsafe { IUnknown::from_raw(result) };
                                                        let retv: Local<v8::Value> = create_ns_ctor_instance_object(return_sig.as_str(), None, None, declaration, Some(instance), scope).into();
                                                        return_value_opt = Some(retv);
                                                    } else {
                                                        return_value_opt = Some(v8::null(scope).into());
                                                    }
                                                }
                                            }
                                            if return_value_opt.is_none() {
                                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                                    let v = unsafe { read_value_from_ptr(result as *const c_void, scope, return_type) };
                                                    return_value_opt = Some(v);
                                                }
                                            }
                                            if let Some(rv) = return_value_opt {
                                                arr.set_index(scope, idx, rv);
                                                idx += 1;
                                            }
                                        }

                                        for outv in outs.into_iter() {
                                            arr.set_index(scope, idx, outv);
                                            idx += 1;
                                        }
                                        retval.set(arr.into());
                                        return;
                                    }

                                    if method.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    let return_sig = method.return_type().to_string();
                                    if return_sig == "Object" && !result.is_null() {
                                        // Methods declared to return `Object`/IInspectable (e.g.
                                        // XamlReader.Load) hand back an opaque pointer whose concrete
                                        // type is only known at runtime. Resolve via GetRuntimeClassName
                                        // and wrap as a full typed proxy so property/event interceptors
                                        // work: otherwise the caller gets a non-extensible object that
                                        // can't subscribe to events.
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        let resolved = instance
                                            .cast::<IInspectable>()
                                            .ok()
                                            .and_then(|insp| insp.GetRuntimeClassName().ok())
                                            .map(|cn| cn.to_string())
                                            .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
                                            .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct));
                                        match resolved {
                                            Some((cname, decl)) => {
                                                let ret: Local<v8::Value> = create_ns_ctor_instance_object(cname.as_str(), None, None, decl, Some(instance), scope).into();
                                                retval.set(ret.into());
                                            }
                                            None => {
                                                let _ = std::mem::ManuallyDrop::new(instance);
                                                unsafe { set_ret_val(result, scope, retval, NativeType::Pointer); }
                                            }
                                        }
                                        return;
                                    }

                                    match NativeType::try_from(method.return_type()) {
                                        Ok(return_type) => {
                                            unsafe { set_ret_val(result, scope, retval, return_type); }
                                        }
                                        Err(_) => {}
                                    }

                                    // todo
                                })
                                    .data(ext.into())
                                    .build(scope);

                                    if is_static {
                                        tmpl.set(name.unwrap().into(), func.into());
                                    } else {
                                        proto.set(name.unwrap().into(), func.into());
                                    }
                                }

                                for property in clazz.properties().iter() {
                                    let name = v8::String::new(scope, property.name());
                                    let is_static = property.is_static();

                                    let declaration = DeclarationFFI::new_with_instance(
                                        Arc::new(RwLock::new(property.clone())),
                                        if is_static {
                                            factory.clone()
                                        } else {
                                            instance.clone()
                                        },
                                    );

                                    let getter_declaration = declaration.clone();

                                    let getter_declaration =
                                        Box::into_raw(Box::new(getter_declaration));

                                    let getter_declaration_ext =
                                        v8::External::new(scope, getter_declaration as _);

                                    let getter = FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                        args: v8::FunctionCallbackArguments,
                                                                        mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };

                                    let dec = dec.value() as *mut DeclarationFFI;

                                    let dec = unsafe { &*dec };

                                    let lock = dec.read();

                                    let _kind = lock.kind();

                                    let Some(property) = lock.as_any().downcast_ref::<PropertyDeclaration>() else { return; };

                                    let Some(ns_instance) = crate::ns_proxy::this_instance(scope, args.this_object()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(
                                        property.getter(), false, ns_instance, false,
                                    );


                                    let (ret, result, outs) = method.call(scope, &args);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if !outs.is_empty() {
                                        let mut arr_len = outs.len();
                                        if !method.is_void() { arr_len += 1; }
                                        let arr = v8::Array::new(scope, arr_len as i32);
                                        let mut idx = 0u32;

                                        if !method.is_void() {
                                            let return_sig = method.return_type().to_string();
                                            let mut return_value_opt: Option<Local<v8::Value>> = None;
                                            if return_sig.contains('.') {
                                                if let Some(declaration) = MetadataReader::find_by_name(return_sig.as_str()) {
                                                    if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                                        let obj = crate::create_struct_object_from_raw(declaration, result, scope).into();
                                                        return_value_opt = Some(obj);
                                                    } else if !result.is_null() {
                                                        let instance = unsafe { IUnknown::from_raw(result) };
                                                        let retv: Local<v8::Value> = create_ns_ctor_instance_object(return_sig.as_str(), None, None, declaration, Some(instance), scope).into();
                                                        return_value_opt = Some(retv);
                                                    } else {
                                                        return_value_opt = Some(v8::null(scope).into());
                                                    }
                                                }
                                            }
                                            if return_value_opt.is_none() {
                                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                                    let v = unsafe { read_value_from_ptr(result as *const c_void, scope, return_type) };
                                                    return_value_opt = Some(v);
                                                }
                                            }
                                            if let Some(rv) = return_value_opt {
                                                arr.set_index(scope, idx, rv);
                                                idx += 1;
                                            }
                                        }

                                        for outv in outs.into_iter() {
                                            arr.set_index(scope, idx, outv);
                                            idx += 1;
                                        }
                                        retval.set(arr.into());
                                        return;
                                    }

                                    if method.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    match NativeType::try_from(method.return_type()) {
                                        Ok(return_type) => {
                                            unsafe { set_ret_val(result, scope, retval, return_type); }
                                        }
                                        Err(_) => {}
                                    }
                                })
                                    .data(getter_declaration_ext.into())
                                    .build(scope);

                                    let mut setter: Option<Local<FunctionTemplate>> = None;

                                    if property.setter().is_some() {
                                        let setter_declaration = declaration;

                                        let setter_declaration =
                                            Box::into_raw(Box::new(setter_declaration));

                                        let setter_declaration_ext =
                                            v8::External::new(scope, setter_declaration as _);

                                        setter = Some(FunctionTemplate::builder(|_scope: &mut v8::PinScope<'_, '_>,
                                                                             _args: v8::FunctionCallbackArguments,
                                                                             _retval: v8::ReturnValue| {})
                                        .data(setter_declaration_ext.into())
                                        .build(scope));
                                    }

                                    if property.is_static() {
                                        let name = name.unwrap();
                                        tmpl.set_accessor_property(
                                            name.into(),
                                            Some(getter),
                                            setter,
                                            v8::PropertyAttribute::DONT_DELETE,
                                        );
                                    } else {
                                        let name = name.unwrap();
                                        proto.set_accessor_property(
                                            name.into(),
                                            Some(getter),
                                            setter,
                                            v8::PropertyAttribute::READ_ONLY
                                                | v8::PropertyAttribute::DONT_DELETE,
                                        );
                                    }
                                }
                            } // end if let Some(clazz) = downcast ClassDeclaration
                        }
                        DeclarationKind::Interface
                        | DeclarationKind::GenericInterface
                        | DeclarationKind::GenericInterfaceInstance => {
                            let clazz_opt: Option<&dyn BaseClassDeclarationImpl> = match kind {
                                DeclarationKind::Interface => clazz
                                    .as_any()
                                    .downcast_ref::<InterfaceDeclaration>()
                                    .map(|d| d as _),
                                DeclarationKind::GenericInterface => clazz
                                    .as_any()
                                    .downcast_ref::<GenericInterfaceDeclaration>()
                                    .map(|d| d as _),
                                DeclarationKind::GenericInterfaceInstance => clazz
                                    .as_any()
                                    .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                                    .map(|d| d as _),
                                _ => None,
                            };
                            if let Some(clazz) = clazz_opt {
                                for method in clazz.methods().iter() {
                                    let name = v8::String::new(scope, method.name());
                                    let is_static = method.is_static();

                                    let declaration = DeclarationFFI::new_with_instance(
                                        Arc::new(RwLock::new(method.clone())),
                                        if is_static {
                                            factory.clone()
                                        } else {
                                            instance.clone()
                                        },
                                    );

                                    let declaration = Box::into_raw(Box::new(declaration));

                                    let ext = v8::External::new(scope, declaration as _);

                                    let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                          args: v8::FunctionCallbackArguments,
                                                                          mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };

                                    let dec = dec.value() as *mut DeclarationFFI;

                                    let dec = unsafe { &*dec };

                                    let lock = dec.read();

                                    let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>() else { return; };

                                    let Some(ns_instance) = crate::ns_proxy::this_instance(scope, args.this_object()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(
                                        method, method.is_sealed(), ns_instance, false,
                                    );

                                    let (ret, result, outs) = method.call(scope, &args);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if !outs.is_empty() {
                                        let mut arr_len = outs.len();
                                        if !method.is_void() { arr_len += 1; }
                                        let arr = v8::Array::new(scope, arr_len as i32);
                                        let mut idx = 0u32;

                                        if !method.is_void() {
                                            let return_sig = method.return_type().to_string();
                                            let mut return_value_opt: Option<Local<v8::Value>> = None;
                                            if return_sig.contains('.') {
                                                if let Some(declaration) = MetadataReader::find_by_name(return_sig.as_str()) {
                                                    if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                                        let obj = crate::create_struct_object_from_raw(declaration, result, scope).into();
                                                        return_value_opt = Some(obj);
                                                    } else if !result.is_null() {
                                                        let instance = unsafe { IUnknown::from_raw(result) };
                                                        let retv: Local<v8::Value> = create_ns_ctor_instance_object(return_sig.as_str(), None, None, declaration, Some(instance), scope).into();
                                                        return_value_opt = Some(retv);
                                                    } else {
                                                        return_value_opt = Some(v8::null(scope).into());
                                                    }
                                                }
                                            }
                                            if return_value_opt.is_none() {
                                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                                    let v = unsafe { read_value_from_ptr(result as *const c_void, scope, return_type) };
                                                    return_value_opt = Some(v);
                                                }
                                            }
                                            if let Some(rv) = return_value_opt {
                                                arr.set_index(scope, idx, rv);
                                                idx += 1;
                                            }
                                        }

                                        for outv in outs.into_iter() {
                                            arr.set_index(scope, idx, outv);
                                            idx += 1;
                                        }
                                        retval.set(arr.into());
                                        return;
                                    }

                                    if method.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    let return_sig = method.return_type().to_string();
                                    if return_sig == "Object" && !result.is_null() {
                                        // Methods declared to return `Object`/IInspectable (e.g.
                                        // XamlReader.Load) hand back an opaque pointer whose concrete
                                        // type is only known at runtime. Resolve via GetRuntimeClassName
                                        // and wrap as a full typed proxy so property/event interceptors
                                        // work: otherwise the caller gets a non-extensible object that
                                        // can't subscribe to events.
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        let resolved = instance
                                            .cast::<IInspectable>()
                                            .ok()
                                            .and_then(|insp| insp.GetRuntimeClassName().ok())
                                            .map(|cn| cn.to_string())
                                            .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
                                            .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct));
                                        match resolved {
                                            Some((cname, decl)) => {
                                                let ret: Local<v8::Value> = create_ns_ctor_instance_object(cname.as_str(), None, None, decl, Some(instance), scope).into();
                                                retval.set(ret.into());
                                            }
                                            None => {
                                                let _ = std::mem::ManuallyDrop::new(instance);
                                                unsafe { set_ret_val(result, scope, retval, NativeType::Pointer); }
                                            }
                                        }
                                        return;
                                    }

                                    match NativeType::try_from(method.return_type()) {
                                        Ok(return_type) => {
                                            unsafe { set_ret_val(result, scope, retval, return_type); }
                                        }
                                        Err(_) => {}
                                    }

                                    // todo
                                })
                                    .data(ext.into())
                                    .build(scope);

                                    if is_static {
                                        tmpl.set(name.unwrap().into(), func.into());
                                    } else {
                                        proto.set(name.unwrap().into(), func.into());
                                    }
                                }

                                for property in clazz.properties().iter() {
                                    let name = v8::String::new(scope, property.name());
                                    let is_static = property.is_static();

                                    let declaration = DeclarationFFI::new_with_instance(
                                        Arc::new(RwLock::new(property.clone())),
                                        if is_static {
                                            factory.clone()
                                        } else {
                                            instance.clone()
                                        },
                                    );

                                    let getter_declaration = declaration.clone();

                                    let getter_declaration =
                                        Box::into_raw(Box::new(getter_declaration));

                                    let getter_declaration_ext =
                                        v8::External::new(scope, getter_declaration as _);

                                    let getter = FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                        args: v8::FunctionCallbackArguments,
                                                                        mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };

                                    let dec = dec.value() as *mut DeclarationFFI;

                                    let dec = unsafe { &*dec };

                                    let lock = dec.read();

                                    let _kind = lock.kind();

                                    let Some(method) = lock.as_any().downcast_ref::<PropertyDeclaration>() else { return; };

                                    let Some(ns_instance) = crate::ns_proxy::this_instance(scope, args.this_object()).or_else(|| dec.instance.clone()) else { return; };
                                    let Some(mut method) = PropertyCall::new(
                                        method, false, ns_instance, false,
                                    ) else { return; };


                                    let (ret, result, outs) = method.call(scope, &args);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if !outs.is_empty() {
                                        let mut arr_len = outs.len();
                                        if !method.is_void() { arr_len += 1; }
                                        let arr = v8::Array::new(scope, arr_len as i32);
                                        let mut idx = 0u32;

                                        if !method.is_void() {
                                            let return_sig = method.return_type().to_string();
                                            let mut return_value_opt: Option<Local<v8::Value>> = None;
                                            if return_sig.contains('.') {
                                                if let Some(declaration) = MetadataReader::find_by_name(return_sig.as_str()) {
                                                    if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                                        let obj = crate::create_struct_object_from_raw(declaration, result, scope).into();
                                                        return_value_opt = Some(obj);
                                                    } else if !result.is_null() {
                                                        let instance = unsafe { IUnknown::from_raw(result) };
                                                        let retv: Local<v8::Value> = create_ns_ctor_instance_object(return_sig.as_str(), None, None, declaration, Some(instance), scope).into();
                                                        return_value_opt = Some(retv);
                                                    } else {
                                                        return_value_opt = Some(v8::null(scope).into());
                                                    }
                                                }
                                            }
                                            if return_value_opt.is_none() {
                                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                                    let v = unsafe { read_value_from_ptr(result as *const c_void, scope, return_type) };
                                                    return_value_opt = Some(v);
                                                }
                                            }
                                            if let Some(rv) = return_value_opt {
                                                arr.set_index(scope, idx, rv);
                                                idx += 1;
                                            }
                                        }

                                        for outv in outs.into_iter() {
                                            arr.set_index(scope, idx, outv);
                                            idx += 1;
                                        }
                                        retval.set(arr.into());
                                        return;
                                    }

                                    if method.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    match NativeType::try_from(method.return_type()) {
                                        Ok(return_type) => {
                                            unsafe { set_ret_val(result, scope, retval, return_type); }
                                        }
                                        Err(_) => {}
                                    }
                                })
                                    .data(getter_declaration_ext.into())
                                    .build(scope);

                                    let mut setter: Option<Local<FunctionTemplate>> = None;

                                    if property.setter().is_some() {
                                        let setter_declaration = declaration;

                                        let setter_declaration =
                                            Box::into_raw(Box::new(setter_declaration));

                                        let setter_declaration_ext =
                                            v8::External::new(scope, setter_declaration as _);

                                        setter = Some(FunctionTemplate::builder(|_scope: &mut v8::PinScope<'_, '_>,
                                                                             _args: v8::FunctionCallbackArguments,
                                                                             _retval: v8::ReturnValue| {})
                                        .data(setter_declaration_ext.into())
                                        .build(scope));
                                    }

                                    if property.is_static() {
                                        let name = name.unwrap();
                                        tmpl.set_accessor_property(
                                            name.into(),
                                            Some(getter),
                                            setter,
                                            v8::PropertyAttribute::DONT_DELETE,
                                        );
                                    } else {
                                        let name = name.unwrap();
                                        proto.set_accessor_property(
                                            name.into(),
                                            Some(getter),
                                            setter,
                                            v8::PropertyAttribute::READ_ONLY
                                                | v8::PropertyAttribute::DONT_DELETE,
                                        );
                                    }
                                }
                            } // end if let Some(clazz) = clazz_opt
                        }
                        DeclarationKind::Delegate
                        | DeclarationKind::GenericDelegate
                        | DeclarationKind::GenericDelegateInstance => {
                            let method = match kind {
                                DeclarationKind::Delegate => clazz
                                    .as_any()
                                    .downcast_ref::<DelegateDeclaration>()
                                    .map(|delegate| delegate.invoke_method().clone()),
                                DeclarationKind::GenericDelegate => clazz
                                    .as_any()
                                    .downcast_ref::<GenericDelegateDeclaration>()
                                    .map(|delegate| delegate.invoke_method().clone()),
                                DeclarationKind::GenericDelegateInstance => clazz
                                    .as_any()
                                    .downcast_ref::<GenericDelegateInstanceDeclaration>()
                                    .map(|delegate| delegate.invoke_method().clone()),
                                _ => None,
                            };

                            if let Some(method) = method {
                                let name = v8::String::new(scope, method.name());
                                let declaration = DeclarationFFI::new_with_instance(
                                    Arc::new(RwLock::new(method)),
                                    instance.clone(),
                                );
                                let declaration = Box::into_raw(Box::new(declaration));
                                let ext = v8::External::new(scope, declaration as _);

                                let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                          args: v8::FunctionCallbackArguments,
                                                                          mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>() else { return; };

                                    let Some(ns_instance) = crate::ns_proxy::this_instance(scope, args.this_object()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(
                                        method,
                                        method.is_sealed(),
                                        ns_instance,
                                        false,
                                    );

                                    let (ret, result, outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if !outs.is_empty() {
                                        let mut arr_len = outs.len();
                                        if !method.is_void() { arr_len += 1; }
                                        let arr = v8::Array::new(scope, arr_len as i32);
                                        let mut idx = 0u32;

                                        if !method.is_void() {
                                            let return_sig = method.return_type().to_string();
                                            let mut return_value_opt: Option<Local<v8::Value>> = None;
                                            if return_sig.contains('.') {
                                                if let Some(declaration) = MetadataReader::find_by_name(return_sig.as_str()) {
                                                    if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                                        let obj = crate::create_struct_object_from_raw(declaration, result, scope).into();
                                                        return_value_opt = Some(obj);
                                                    } else if !result.is_null() {
                                                        let instance = unsafe { IUnknown::from_raw(result) };
                                                        let retv: Local<v8::Value> = create_ns_ctor_instance_object(return_sig.as_str(), None, None, declaration, Some(instance), scope).into();
                                                        return_value_opt = Some(retv);
                                                    } else {
                                                        return_value_opt = Some(v8::null(scope).into());
                                                    }
                                                }
                                            }
                                            if return_value_opt.is_none() {
                                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                                    let v = unsafe { read_value_from_ptr(result as *const c_void, scope, return_type) };
                                                    return_value_opt = Some(v);
                                                }
                                            }
                                            if let Some(rv) = return_value_opt {
                                                arr.set_index(scope, idx, rv);
                                                idx += 1;
                                            }
                                        }

                                        for outv in outs.into_iter() {
                                            arr.set_index(scope, idx, outv);
                                            idx += 1;
                                        }
                                        retval.set(arr.into());
                                        return;
                                    }

                                    if method.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    let return_sig = method.return_type().to_string();
                                    if return_sig.contains('.') {
                                        if let Some(declaration) = MetadataReader::find_by_name(return_sig.as_str()) {
                                            let ret: Local<v8::Value> = if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                                create_struct_object_from_raw(declaration, result, scope).into()
                                            } else if result.is_null() {
                                                v8::null(scope).into()
                                            } else {
                                                let instance = unsafe { IUnknown::from_raw(result) };
                                                create_ns_ctor_instance_object(return_sig.as_str(), None, None, declaration, Some(instance), scope).into()
                                            };
                                            retval.set(ret);
                                            return;
                                        }
                                    } else {
                                        crate::ns_proxy::set_ret_val_resolving_object(result, return_sig.as_str(), scope, retval);
                                    }
                                })
                                    .data(ext.into())
                                    .build(scope);

                                if let Some(name) = name {
                                    proto.set(name.into(), func.into());
                                }
                            }
                        }
                        _ => {}
                    }
                }

                for method in clazz.methods().iter() {
                    let name = v8::String::new(scope, method.name());
                    let is_static = method.is_static();

                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(method.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );

                    let declaration = Box::into_raw(Box::new(declaration));

                    let ext = v8::External::new(scope, declaration as _);

                    let func = v8::FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };

                            let dec = dec.value() as *mut DeclarationFFI;

                            let dec = unsafe { &*dec };

                            let lock = dec.read();

                            let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>()
                            else {
                                return;
                            };

                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let mut method =
                                {
                                    let alt = crate::class_helpers::overload_for_argc(method, args.length() as usize);
                                    let method = alt.as_ref().unwrap_or(method);
                                    MethodCall::new(method, method.is_sealed(), ns_instance, false)
                                };

                            let (ret, result, outs) = method.call(scope, &args);

                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            }

                            if !outs.is_empty() {
                                let mut arr_len = outs.len();
                                if !method.is_void() {
                                    arr_len += 1;
                                }
                                let arr = v8::Array::new(scope, arr_len as i32);
                                let mut idx = 0u32;

                                if !method.is_void() {
                                    let return_sig = method.return_type().to_string();
                                    let mut return_value_opt: Option<Local<v8::Value>> = None;
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig.as_str())
                                        {
                                            if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                let obj = crate::create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into();
                                                return_value_opt = Some(obj);
                                            } else if !result.is_null() {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                let retv: Local<v8::Value> =
                                                    create_ns_ctor_instance_object(
                                                        &return_sig,
                                                        None,
                                                        None,
                                                        declaration,
                                                        Some(instance),
                                                        scope,
                                                    )
                                                    .into();
                                                return_value_opt = Some(retv);
                                            } else {
                                                return_value_opt = Some(v8::null(scope).into());
                                            }
                                        }
                                    }
                                    if return_value_opt.is_none() {
                                        if let Ok(return_type) =
                                            NativeType::try_from(return_sig.as_str())
                                        {
                                            let v = unsafe {
                                                read_value_from_ptr(
                                                    result as *const c_void,
                                                    scope,
                                                    return_type,
                                                )
                                            };
                                            return_value_opt = Some(v);
                                        }
                                    }
                                    if let Some(rv) = return_value_opt {
                                        arr.set_index(scope, idx, rv);
                                        idx += 1;
                                    }
                                }

                                for outv in outs.into_iter() {
                                    arr.set_index(scope, idx, outv);
                                    idx += 1;
                                }
                                retval.set(arr.into());
                                return;
                            }

                            if method.is_void() {
                                retval.set_undefined();
                                return;
                            }

                            let return_sig = method.return_type().to_string();
                            if return_sig.contains('.') {
                                if result.is_null() {
                                    retval.set(v8::null(scope).into());
                                } else {
                                    let declaration =
                                        MetadataReader::find_by_name(return_sig.as_str())
                                            .unwrap_or_else(|| dec.inner.clone());
                                    let instance = unsafe { IUnknown::from_raw(result) };
                                    let ret_val: Local<v8::Value> = create_ns_ctor_instance_object(
                                        &return_sig,
                                        None,
                                        None,
                                        declaration,
                                        Some(instance),
                                        scope,
                                    )
                                    .into();
                                    retval.set(ret_val);
                                }
                            } else {
                                crate::ns_proxy::set_ret_val_resolving_object(
                                    result,
                                    return_sig.as_str(),
                                    scope,
                                    retval,
                                );
                            }
                        },
                    )
                    .data(ext.into())
                    .build(scope);

                    if is_static {
                        tmpl.set(name.unwrap().into(), func.into());
                    } else {
                        proto.set(name.unwrap().into(), func.into());
                    }
                }

                for property in clazz.properties().iter() {
                    let name = v8::String::new(scope, property.name());
                    let is_static = property.is_static();

                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(property.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );

                    let getter_declaration = declaration.clone();

                    let getter_declaration = Box::into_raw(Box::new(getter_declaration));

                    let getter_declaration_ext = v8::External::new(scope, getter_declaration as _);

                    let getter = FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };

                            let dec = dec.value() as *mut DeclarationFFI;

                            let dec = unsafe { &*dec };

                            let lock = dec.read();

                            let _kind = lock.kind();

                            let Some(method) = lock.as_any().downcast_ref::<PropertyDeclaration>()
                            else {
                                return;
                            };

                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let Some(mut method) =
                                PropertyCall::new(method, false, ns_instance, false)
                            else {
                                return;
                            };

                            let (ret, result, outs) = method.call(scope, &args);

                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            }

                            if !outs.is_empty() {
                                let mut arr_len = outs.len();
                                if !method.is_void() {
                                    arr_len += 1;
                                }
                                let arr = v8::Array::new(scope, arr_len as i32);
                                let mut idx = 0u32;

                                if !method.is_void() {
                                    let return_sig = method.return_type().to_string();
                                    let mut return_value_opt: Option<Local<v8::Value>> = None;
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig.as_str())
                                        {
                                            if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                let obj = crate::create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into();
                                                return_value_opt = Some(obj);
                                            } else if !result.is_null() {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                let retv: Local<v8::Value> =
                                                    create_ns_ctor_instance_object(
                                                        return_sig.as_str(),
                                                        None,
                                                        None,
                                                        declaration,
                                                        Some(instance),
                                                        scope,
                                                    )
                                                    .into();
                                                return_value_opt = Some(retv);
                                            } else {
                                                return_value_opt = Some(v8::null(scope).into());
                                            }
                                        }
                                    }
                                    if return_value_opt.is_none() {
                                        if let Ok(return_type) =
                                            NativeType::try_from(return_sig.as_str())
                                        {
                                            let v = unsafe {
                                                read_value_from_ptr(
                                                    result as *const c_void,
                                                    scope,
                                                    return_type,
                                                )
                                            };
                                            return_value_opt = Some(v);
                                        }
                                    }
                                    if let Some(rv) = return_value_opt {
                                        arr.set_index(scope, idx, rv);
                                        idx += 1;
                                    }
                                }

                                for outv in outs.into_iter() {
                                    arr.set_index(scope, idx, outv);
                                    idx += 1;
                                }
                                retval.set(arr.into());
                                return;
                            }

                            if method.is_void() {
                                retval.set_undefined();
                                return;
                            }

                            match NativeType::try_from(method.return_type()) {
                                Ok(return_type) => unsafe {
                                    set_ret_val(result, scope, retval, return_type);
                                },
                                Err(_) => {}
                            }
                        },
                    )
                    .data(getter_declaration_ext.into())
                    .build(scope);

                    let mut setter: Option<Local<FunctionTemplate>> = None;

                    if property.setter().is_some() {
                        let setter_declaration = declaration;

                        let setter_declaration = Box::into_raw(Box::new(setter_declaration));

                        let setter_declaration_ext =
                            v8::External::new(scope, setter_declaration as _);

                        setter = Some(
                            FunctionTemplate::builder(
                                |scope: &mut v8::PinScope<'_, '_>,
                                 args: v8::FunctionCallbackArguments,
                                 _retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let Some(prop) =
                                        lock.as_any().downcast_ref::<PropertyDeclaration>()
                                    else {
                                        return;
                                    };
                                    let Some(setter) = prop.setter() else {
                                        return;
                                    };
                                    let Some(ns_instance) =
                                        crate::ns_proxy::this_instance(scope, args.this_object())
                                            .or_else(|| dec.instance.clone())
                                    else {
                                        return;
                                    };
                                    let mut method =
                                        MethodCall::new(setter, false, ns_instance, false);
                                    let (ret, _, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg);
                                        scope.throw_exception(err);
                                    }
                                },
                            )
                            .data(setter_declaration_ext.into())
                            .build(scope),
                        );
                    }

                    if property.is_static() {
                        let name = name.unwrap();
                        tmpl.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        let name = name.unwrap();
                        proto.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE,
                        );
                    }
                }
            }
            DeclarationKind::GenericInterface => {
                let Some(clazz) = lock.as_any().downcast_ref::<GenericInterfaceDeclaration>()
                else {
                    return v8::undefined(scope).into();
                };

                let return_types = helpers::get_generic_return_types(name);
                let type_args_str: String = return_types.names().join(",");

                for method in clazz.methods() {
                    let signature = method.return_type();

                    let Some(metadata) = method.metadata() else {
                        continue;
                    };
                    let return_type_str = Signature::to_string(metadata, &signature);

                    let return_type_index = match usize::from_str_radix(
                        &*return_type_str.as_str().replace("Var!", ""),
                        10,
                    ) {
                        Ok(idx) => idx,
                        Err(_) => continue,
                    };

                    let Some(&return_type) = return_types.names().get(return_type_index) else {
                        continue;
                    };

                    let name = v8::String::new(scope, method.name());

                    let is_static = method.is_static();

                    let parent = declaration.clone();
                    let mut declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(method.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );
                    declaration.parent = Some(parent);

                    let declaration = Box::into_raw(Box::new(declaration));

                    let Some(return_type) = v8::String::new(scope, return_type) else {
                        continue;
                    };
                    let Some(type_args_v8) = v8::String::new(scope, &type_args_str) else {
                        continue;
                    };

                    let ext = v8::External::new(scope, declaration as _);

                    let data = v8::Array::new_with_elements(
                        scope,
                        &[ext.into(), return_type.into(), type_args_v8.into()],
                    );

                    let func = FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let Ok(data) = v8::Local::<v8::Array>::try_from(args.data()) else {
                                return;
                            };

                            let Some(return_type_val) = data.get_index(scope, 1) else {
                                return;
                            };
                            let return_type = return_type_val.to_rust_string_lossy(scope);

                            let type_args_str = data
                                .get_index(scope, 2)
                                .map(|v| v.to_rust_string_lossy(scope))
                                .unwrap_or_default();
                            let type_args: Vec<String> = if type_args_str.is_empty() {
                                Vec::new()
                            } else {
                                type_args_str.split(',').map(|s| s.to_owned()).collect()
                            };

                            let Some(dec_val) = data.get_index(scope, 0) else {
                                return;
                            };
                            let dec = unsafe { dec_val.cast::<v8::External>() };

                            let dec = dec.value() as *mut DeclarationFFI;

                            let dec = unsafe { &*dec };

                            let lock = dec.read();

                            let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>()
                            else {
                                return;
                            };

                            let Some(parent_arc) = dec.parent.as_ref() else {
                                return;
                            };
                            let parent = parent_arc.read();
                            let Some(parent) = parent
                                .as_any()
                                .downcast_ref::<GenericInterfaceDeclaration>()
                            else {
                                return;
                            };

                            let Some(ns_instance) =
                                crate::ns_proxy::this_instance(scope, args.this_object())
                                    .or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let mut method = GenericMethodCall::new(
                                parent,
                                method,
                                method.is_sealed(),
                                ns_instance,
                                false,
                                return_type,
                                type_args,
                            );

                            let (ret, result, outs) = method.call(scope, &args);

                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            }

                            if !outs.is_empty() {
                                let mut arr_len = outs.len();
                                if !method.is_void() {
                                    arr_len += 1;
                                }
                                let arr = v8::Array::new(scope, arr_len as i32);
                                let mut idx = 0u32;

                                if !method.is_void() {
                                    let return_sig = method.return_type();
                                    let mut return_value_opt: Option<Local<v8::Value>> = None;
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig)
                                        {
                                            if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                let obj = crate::create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into();
                                                return_value_opt = Some(obj);
                                            } else if !result.is_null() {
                                                let instance = unsafe {
                                                    IUnknown::from_raw(
                                                        *(result as *mut *mut c_void),
                                                    )
                                                };
                                                let retv: Local<v8::Value> =
                                                    create_ns_ctor_instance_object(
                                                        return_sig,
                                                        None,
                                                        dec.parent.clone(),
                                                        declaration,
                                                        Some(instance),
                                                        scope,
                                                    )
                                                    .into();
                                                return_value_opt = Some(retv);
                                            } else {
                                                return_value_opt = Some(v8::null(scope).into());
                                            }
                                        } else {
                                            let instance = unsafe {
                                                IUnknown::from_raw(*(result as *mut *mut c_void))
                                            };
                                            let declaration =
                                                MetadataReader::find_by_name(return_sig)
                                                    .unwrap_or_else(|| dec.inner.clone());
                                            let retv: Local<v8::Value> =
                                                create_ns_ctor_instance_object(
                                                    return_sig,
                                                    None,
                                                    dec.parent.clone(),
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into();
                                            return_value_opt = Some(retv);
                                        }
                                    }
                                    if return_value_opt.is_none() {
                                        if let Ok(return_type) = NativeType::try_from(return_sig) {
                                            let v = unsafe {
                                                read_value_from_ptr(
                                                    result as *const c_void,
                                                    scope,
                                                    return_type,
                                                )
                                            };
                                            return_value_opt = Some(v);
                                        }
                                    }
                                    if let Some(rv) = return_value_opt {
                                        arr.set_index(scope, idx, rv);
                                        idx += 1;
                                    }
                                }

                                for outv in outs.into_iter() {
                                    arr.set_index(scope, idx, outv);
                                    idx += 1;
                                }
                                retval.set(arr.into());
                                return;
                            }

                            if method.is_void() {
                                retval.set_undefined();
                                return;
                            }

                            let return_sig = method.return_type();
                            match NativeType::try_from(return_sig) {
                                Ok(return_type) => {
                                    if return_sig.contains('.') {
                                        if let Some(declaration) =
                                            MetadataReader::find_by_name(return_sig)
                                        {
                                            let ret: Local<v8::Value> = if matches!(
                                                declaration.read().kind(),
                                                DeclarationKind::Struct
                                            ) {
                                                crate::create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into()
                                            } else if result.is_null() {
                                                v8::null(scope).into()
                                            } else {
                                                let instance = unsafe {
                                                    IUnknown::from_raw(
                                                        *(result as *mut *mut c_void),
                                                    )
                                                };
                                                create_ns_ctor_instance_object(
                                                    return_sig,
                                                    None,
                                                    dec.parent.clone(),
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into()
                                            };
                                            retval.set(ret.into());
                                            return;
                                        } else {
                                            let instance = unsafe {
                                                IUnknown::from_raw(*(result as *mut *mut c_void))
                                            };
                                            let declaration =
                                                MetadataReader::find_by_name(return_sig)
                                                    .unwrap_or_else(|| dec.inner.clone());
                                            let ret: Local<v8::Value> =
                                                create_ns_ctor_instance_object(
                                                    return_sig,
                                                    None,
                                                    dec.parent.clone(),
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into();
                                            retval.set(ret.into());
                                            return;
                                        }
                                    }
                                    unsafe {
                                        set_ret_val(result, scope, retval, return_type);
                                    }
                                }
                                Err(_) => {}
                            }

                            // todo
                        },
                    )
                    .data(data.into())
                    .build(scope);

                    if let Some(n) = name {
                        if is_static {
                            tmpl.set_with_attr(
                                n.into(),
                                func.into(),
                                v8::PropertyAttribute::DONT_DELETE,
                            );
                        } else {
                            proto.set_with_attr(
                                n.into(),
                                func.into(),
                                v8::PropertyAttribute::DONT_DELETE,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    {
        let g = v8::Global::new(scope, tmpl);
        if let Some(cache) = scope.get_slot::<crate::ns_proxy::InstanceTemplateCache>() {
            cache.0.borrow_mut().insert(template_key, g);
        }
    }

    crate::ns_proxy::finish_instance_object(tmpl, declaration, instance, identity_key, scope)
}

/// Converts a raw WinRT out-parameter result pointer to a `Local<v8::Value>`.
/// Returns `None` for void returns, null COM pointers, or unrecognised types.
/// The `parent_decl` is forwarded to `create_ns_ctor_instance_object` for COM returns.
#[cfg(feature = "classic")]
unsafe fn raw_result_to_local<'s>(
    result: *mut c_void,
    signature: &str,
    parent_decl: Option<Arc<RwLock<dyn Declaration>>>,
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<Local<'s, v8::Value>> {
    let raw = result as usize;
    match signature {
        "Void" => None,
        "Guid" => Some(unsafe { guid_ptr_to_js_object(result, scope) }.into()),
        _ if !signature.contains('.') => {
            let native_type = NativeType::try_from(signature).ok()?;
            let v: Local<v8::Value> = match native_type {
                NativeType::Void => return None,
                NativeType::Bool => v8::Boolean::new(scope, (raw as u8) != 0).into(),
                NativeType::U8 => v8::Number::new(scope, raw as u8 as f64).into(),
                NativeType::I8 => v8::Number::new(scope, (raw as u8 as i8) as f64).into(),
                NativeType::U16 => v8::Number::new(scope, raw as u16 as f64).into(),
                NativeType::I16 => v8::Number::new(scope, (raw as u16 as i16) as f64).into(),
                NativeType::U32 => v8::Number::new(scope, raw as u32 as f64).into(),
                NativeType::I32 => v8::Number::new(scope, (raw as u32 as i32) as f64).into(),
                NativeType::U64 => {
                    let v = raw as u64;
                    if v > MAX_SAFE_INTEGER as u64 {
                        v8::BigInt::new_from_u64(scope, v).into()
                    } else {
                        v8::Number::new(scope, v as f64).into()
                    }
                }
                NativeType::I64 => {
                    let v = raw as u64 as i64;
                    if v > MAX_SAFE_INTEGER as i64 || v < MIN_SAFE_INTEGER as i64 {
                        v8::BigInt::new_from_i64(scope, v).into()
                    } else {
                        v8::Number::new(scope, v as f64).into()
                    }
                }
                NativeType::USize => {
                    if raw > MAX_SAFE_INTEGER as usize {
                        v8::BigInt::new_from_u64(scope, raw as u64).into()
                    } else {
                        v8::Number::new(scope, raw as f64).into()
                    }
                }
                NativeType::ISize => {
                    let v = raw as isize;
                    if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&v) {
                        v8::BigInt::new_from_i64(scope, v as i64).into()
                    } else {
                        v8::Number::new(scope, v as f64).into()
                    }
                }
                NativeType::F32 => v8::Number::new(scope, f32::from_bits(raw as u32) as f64).into(),
                NativeType::F64 => v8::Number::new(scope, f64::from_bits(raw as u64)).into(),
                NativeType::String => {
                    let hstring: HSTRING = std::mem::transmute(result);
                    let s = hstring.to_string();
                    v8::String::new(scope, s.as_str())?.into()
                }
                // "Object" in WinRT metadata maps to NativeType::Pointer and means
                // IInspectable*: try to resolve the concrete runtime class name so
                // we can hand back a typed wrapper instead of an opaque External.
                NativeType::Pointer => {
                    if result.is_null() {
                        return None;
                    }
                    let unknown = IUnknown::from_raw(result);
                    if let Ok(inspectable) = unknown.cast::<IInspectable>() {
                        if let Ok(class_name) = inspectable.GetRuntimeClassName() {
                            let name_str = class_name.to_string();
                            if let Some(decl) = MetadataReader::find_by_name(&name_str) {
                                let instance = unknown.clone();
                                return Some(
                                    create_ns_ctor_instance_object(
                                        &name_str,
                                        None,
                                        parent_decl,
                                        decl,
                                        Some(instance),
                                        scope,
                                    )
                                    .into(),
                                );
                            }
                        }
                    }
                    // Could not resolve type: expose as opaque External so the
                    // property is at least present rather than silently missing.
                    v8::External::new(scope, result).into()
                }
                _ => return None,
            };
            Some(v)
        }
        _ => {
            if result.is_null() {
                return None;
            }
            let com_instance = IUnknown::from_raw(result);
            let decl = MetadataReader::find_by_name(signature)?;
            Some(
                create_ns_ctor_instance_object(
                    signature,
                    None,
                    parent_decl,
                    decl,
                    Some(com_instance),
                    scope,
                )
                .into(),
            )
        }
    }
}

#[cfg(feature = "classic")]
fn create_ns_ctor_object<'a>(
    name: &str,
    parent: Option<Arc<RwLock<dyn Declaration>>>,
    declaration: Arc<RwLock<dyn Declaration>>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    // Re-entrancy guard: if we're already building this constructor on this
    // thread, return a lightweight stub function to avoid mutating V8
    // templates/descriptors twice (which can trigger internal V8 assertions).
    let name_str = name;
    let already_building = CREATING_CTORS.with(|set| {
        let mut set = set.borrow_mut();
        if set.contains(name_str) {
            true
        } else {
            set.insert(name_str.to_string());
            false
        }
    });

    if already_building {
        let stub = v8::FunctionTemplate::builder(
            |_scope: &mut v8::PinScope<'_, '_>,
             _args: v8::FunctionCallbackArguments,
             mut _retval: v8::ReturnValue| {},
        )
        .build(scope);
        let Some(func) = stub.get_function(scope) else {
            return v8::undefined(scope).into();
        };
        let key = v8::String::new(scope, "__typeName__").unwrap();
        let val = v8::String::new(scope, name_str).unwrap();
        func.set(scope, key.into(), val.into());
        return func.into();
    }

    let name = v8::String::new(scope, name).unwrap();

    let mut declaration_ffi = DeclarationFFI::new(Arc::clone(&declaration));

    declaration_ffi.parent = parent;

    let declaration_ptr = Box::into_raw(Box::new(declaration_ffi));

    let data_ext = v8::External::new(scope, declaration_ptr as _);

    let tmpl = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                              args: v8::FunctionCallbackArguments,
                                              mut retval: v8::ReturnValue| {
        let length = args.length();

        let dec = unsafe { args.data().cast::<v8::External>() };

        let dec = dec.value() as *mut DeclarationFFI;

        let dec = unsafe { &*dec };

        let lock = dec.read();

        let kind = lock.kind();

        let ext = args.data();

        match kind {
            DeclarationKind::Class => {
                // Collect all needed data while holding the lock, then release it so
                // create_ns_ctor_instance_object can re-acquire without deadlocking.
                let full_name;
                let is_sealed;
                let initializers: Vec<MethodDeclaration>;
                let parent;
                {
                    let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else { return; };
                    full_name = clazz.full_name().to_string();
                    is_sealed = clazz.is_sealed();
                    initializers = clazz.initializers().iter().cloned().collect();
                    parent = dec.parent.clone();
                }
                drop(lock);

                // Attempt activation using several candidate type names derived
                // from metadata (full name, stripped-generic, simple name).
                // This allows trying alternate activators when the default
                // `RoGetActivationFactory` lookup for `full_name` doesn't work
                // (observed with some XAML types such as FontFamily).
                let mut clazz_factory_opt: Option<IUnknown> = None;
                let mut last_err: Option<windows::core::Error> = None;
                let mut candidates: Vec<String> = Vec::new();
                candidates.push(full_name.clone());
                let stripped = crate::helpers::strip_generic_suffix(full_name.as_str()).to_string();
                if stripped != candidates[0] {
                    candidates.push(stripped);
                }
                let (_ns, simple_name) = split_type_name(full_name.as_str());
                if !simple_name.is_empty() && !candidates.contains(&simple_name) {
                    candidates.push(simple_name.clone());
                }

                for candidate in candidates.iter() {
                    match class_activation_factory(candidate.as_str()) {
                        Ok(factory) => { clazz_factory_opt = Some(factory); break; }
                        Err(e) => { last_err = Some(e); }
                    }
                }

                let clazz_factory = match clazz_factory_opt {
                    Some(f) => f,
                    None => {
                        if let Some(e) = last_err {
                            throw_js_error(
                                scope,
                                format!("Failed to activate WinRT class {}: {}", full_name, e.message()).as_str(),
                            );
                        } else {
                            throw_js_error(
                                scope,
                                format!("Failed to activate WinRT class {}", full_name).as_str(),
                            );
                        }
                        return;
                    }
                };

                if length == 0 {
                    match clazz_factory.cast::<IActivationFactory>() {
                        Ok(activation_factory) => {
                            match unsafe { activation_factory.ActivateInstance() } {
                                Ok(instance) => {
                                    let result = match instance.cast::<IUnknown>() {
                                        Ok(value) => value,
                                        Err(error) => {
                                            throw_js_error(
                                                scope,
                                                format!("Failed to cast activated instance for {}: {}", full_name, error.message()).as_str(),
                                            );
                                            return;
                                        }
                                    };

                                    if let Ok(init) = result.cast::<IInitializeWithWindow>() {
                                        let hwnd = unsafe { GetConsoleWindow() };
                                        if !hwnd.is_invalid() {
                                            let _ = unsafe { init.Initialize(hwnd) };
                                        }
                                    }

                                    if let Some(declaration) = MetadataReader::find_by_name(&full_name) {
                                        let instance_obj = create_ns_ctor_instance_object(
                                            &full_name, Some(clazz_factory.clone()), parent.clone(), declaration, Some(result), scope,
                                        );
                                        retval.set(instance_obj);
                                    }
                                    return;
                                }
                                Err(error) => {
                                    throw_js_error(
                                        scope,
                                        format!("ActivateInstance failed for WinRT class {}: {}", full_name, error.message()).as_str(),
                                    );
                                    return;
                                }
                            }
                        }
                        Err(_) => {
                            // Fall through to metadata-driven constructor dispatch when
                            // activation factory cannot be cast.
                        }
                    }
                }

                for ctor in &initializers {
                    let number_of_parameters = ctor.number_of_parameters();
                    if number_of_parameters != length as usize {
                        continue;
                    }
                    let mut method = MethodCall::new(ctor, is_sealed, clazz_factory.clone(), true);
                    let (ret, result, outs) = method.call(scope, &args);

                    if ret.is_ok() {
                        if result.is_null() {
                            retval.set(v8::null(scope).into());
                            return;
                        }
                        let result = unsafe { IUnknown::from_raw(result) };
                        let vtable = result.vtable();
                        let mut qi_ptr: *mut c_void = std::ptr::null_mut();
                        let res = unsafe {
                            ((*vtable).QueryInterface)(
                                result.as_raw(),
                                &IUnknown::IID,
                                std::mem::transmute(&mut qi_ptr),
                            )
                        };
                        if res.is_err() || qi_ptr.is_null() {
                            let message = res.message().to_string();
                            let message = v8::String::new(scope, message.as_str()).unwrap();
                            let error = v8::Exception::error(scope, message.into());
                            scope.throw_exception(error);
                            return;
                        }
                        let result = unsafe { IUnknown::from_raw(qi_ptr) };

                        if let Ok(init) = result.cast::<IInitializeWithWindow>() {
                            let hwnd = unsafe { GetConsoleWindow() };
                            if !hwnd.is_invalid() {
                                let _ = unsafe { init.Initialize(hwnd) };
                            }
                        }

                        if let Some(declaration) = MetadataReader::find_by_name(&full_name) {
                            let instance_obj = create_ns_ctor_instance_object(
                                &full_name, Some(clazz_factory.clone()), parent.clone(), declaration, Some(result), scope,
                            );
                            if !outs.is_empty() {
                                let arr = v8::Array::new(scope, (1 + outs.len()) as i32);
                                arr.set_index(scope, 0, instance_obj);
                                let mut idx = 1u32;
                                for outv in outs.into_iter() {
                                    arr.set_index(scope, idx, outv);
                                    idx += 1;
                                }
                                retval.set(arr.into());
                            } else {
                                retval.set(instance_obj);
                            }
                        }
                        return;
                    } else {
                        let detail = crate::error::format_hresult_message(ret);
                        let message = v8::String::new(scope, &detail).unwrap();
                        let error = v8::Exception::error(scope, message.into());
                        scope.throw_exception(error);
                        return;
                    }
                }
                return;
            }
            DeclarationKind::Struct => {}
            DeclarationKind::Delegate
            | DeclarationKind::GenericDelegate
            | DeclarationKind::GenericDelegateInstance => {
                // Accept a plain JS function or an Android-style { Invoke(){} } object.
                if length >= 1 {
                    let arg0 = args.get(0);
                    let maybe_func: Option<v8::Local<v8::Function>> = if arg0.is_function() {
                        v8::Local::<v8::Function>::try_from(arg0).ok()
                    } else if arg0.is_object() {
                        if let Some(obj) = arg0.to_object(scope) {
                            let mut found = None;
                            for name in &["Invoke", "invoke"] {
                                if let Some(key) = v8::String::new(scope, name) {
                                    if let Some(val) = obj.get(scope, key.into()) {
                                        if let Ok(f) = v8::Local::<v8::Function>::try_from(val) {
                                            found = Some(f);
                                            break;
                                        }
                                    }
                                }
                            }
                            found
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(func) = maybe_func {
                        if let Some((guid, param_types)) =
                            js_delegate_params_from_declaration(&*lock, kind)
                        {
                            let global_func = v8::Global::new(scope, func);
                            let data = Box::new(JsDelegateData {
                                js_func: global_func,
                                param_types,
                            });
                            let delegate = Box::new(JsDelegate {
                                vtable: &JS_DELEGATE_VTBL as *const _,
                                ref_count: AtomicU32::new(1),
                                guid,
                                data: Box::into_raw(data),
                            });
                            let raw = Box::into_raw(delegate) as *mut c_void;

                            let result_obj = v8::Object::new(scope);
                            if let Some(key) = v8::String::new(scope, "handle") {
                                result_obj.set(
                                    scope,
                                    key.into(),
                                    v8::External::new(scope, raw).into(),
                                );
                            }
                            retval.set(result_obj.into());
                            return;
                        } else {
                        }
                    } else {
                    }
                }
            }
            _ => {}
        }

        let object_tmpl = v8::ObjectTemplate::new(scope);
        object_tmpl.set_named_property_handler(
            v8::NamedPropertyHandlerConfiguration::new()
                .query(handle_named_property_query)
                .getter(handle_named_property_getter)
                .setter(handle_named_property_setter)
        );
        object_tmpl.set_indexed_property_handler(
            v8::IndexedPropertyHandlerConfiguration::new()
                .setter(handle_indexed_property_setter)
                .getter(handle_indexed_property_getter)
        );
        object_tmpl.set_internal_field_count(2);
        let Some(object) = object_tmpl.new_instance(scope) else { return; };

        object.set_internal_field(0, ext.into());

        let object_store = v8::Map::new(scope);

        if matches!(
            kind,
            DeclarationKind::Interface
                | DeclarationKind::GenericInterface
                | DeclarationKind::GenericInterfaceInstance
                | DeclarationKind::Delegate
                | DeclarationKind::GenericDelegate
                | DeclarationKind::GenericDelegateInstance
                | DeclarationKind::Event
        ) && length >= 1
        {
            let implementation = args.get(0);
            if implementation.is_object() || implementation.is_function() {
                if let Some(impl_key) = v8::String::new(scope, "__implementation__") {
                    object_store.set(scope, impl_key.into(), implementation);
                }
            }
        }

        object.set_internal_field(1, object_store.into());

        retval.set(object.into());
    })
        .data(data_ext.into()).build(scope);
    // Ensure instances created by this constructor use the same
    // named/indexed property handlers and internal fields as the
    // instance-wrapper path so prototype/instance members are visible.
    let instance_tmpl = tmpl.instance_template(scope);
    instance_tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .getter(
                |scope: &mut v8::PinScope<'_, '_>,
                 key: Local<v8::Name>,
                 args: v8::PropertyCallbackArguments,
                 mut rv: v8::ReturnValue<v8::Value>|
                 -> v8::Intercepted {
                    if !key.is_string() {
                        return v8::Intercepted::kNo;
                    }

                    let name = key.to_rust_string_lossy(scope);
                    if name == "__probe__" {
                        let value = v8::String::new(scope, "instance-handler-active").unwrap();
                        rv.set(value.into());
                        return v8::Intercepted::kYes;
                    }

                    // Prefer the DeclarationFFI stored on the instance (holder internal field[0]).
                    // Fall back to the callback data if the holder lacks the internal field.
                    let holder = args.holder();
                    let dec_field_opt = holder.get_internal_field(scope, 0);
                    let dec = if let Some(dec_field) = dec_field_opt {
                        let dec_ext = unsafe { dec_field.cast::<v8::External>() };
                        let dec_ptr = dec_ext.value() as *mut DeclarationFFI;
                        unsafe { &*dec_ptr }
                    } else {
                        let dec_ext = unsafe { args.data().cast::<v8::External>() };
                        let dec_ptr = dec_ext.value() as *mut DeclarationFFI;
                        unsafe { &*dec_ptr }
                    };

                    let lock = dec.read();

                    let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
                        return v8::Intercepted::kNo;
                    };

                    // If a JS-assigned override exists in the per-instance store, return it.
                    // Use the holder (where the property was found) to access the side-store map.
                    let this = holder;
                    if let Some(store_field) = this.get_internal_field(scope, 1) {
                        let store = unsafe { store_field.cast::<v8::Map>() };
                        if let Some(cache) = store.get(scope, key.into()) {
                            if !cache.is_null_or_undefined() {
                                rv.set(cache);
                                return v8::Intercepted::kYes;
                            }
                        }
                    }

                    if let Some(property) = find_class_property(clazz, &name) {
                        let Some(ns_instance) =
                            crate::ns_proxy::this_instance(scope, args.this_object())
                                .or_else(|| dec.instance.clone())
                        else {
                            return v8::Intercepted::kNo;
                        };
                        let Some(mut property_call) =
                            PropertyCall::new(&property, false, ns_instance, false)
                        else {
                            return v8::Intercepted::kNo;
                        };
                        let (ret, result, _outs) = property_call.call_with_values(scope, &[]);

                        if ret.is_err() {
                            let detail = format!(
                                "Property get '{}' failed: {} (0x{:08X})",
                                name,
                                ret.message(),
                                ret.0 as u32
                            );
                            let message = v8::String::new(scope, &detail).unwrap();
                            let error = v8::Exception::error(scope, message);
                            scope.throw_exception(error);
                            return v8::Intercepted::kYes;
                        }

                        if property_call.is_void() {
                            rv.set_undefined();
                            return v8::Intercepted::kYes;
                        }

                        let return_sig = property_call.return_type().to_string();
                        if return_sig.contains('.') {
                            if let Some(declaration) =
                                MetadataReader::find_by_name(return_sig.as_str())
                            {
                                let ret: Local<v8::Value> =
                                    if matches!(declaration.read().kind(), DeclarationKind::Struct)
                                    {
                                        create_struct_object_from_raw(declaration, result, scope)
                                            .into()
                                    } else if result.is_null() {
                                        v8::null(scope).into()
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        create_ns_ctor_instance_object(
                                            return_sig.as_str(),
                                            None,
                                            None,
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into()
                                    };
                                rv.set(ret);
                                return v8::Intercepted::kYes;
                            }
                        }

                        if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                            unsafe {
                                set_ret_val(result, scope, rv, return_type);
                            }
                            return v8::Intercepted::kYes;
                        }

                        return v8::Intercepted::kNo;
                    }

                    if let Some(method) = find_class_method(clazz, &name) {

                        crate::class_helpers::register_overload_siblings(clazz, &name, &method);
                        let declaration = Arc::new(RwLock::new(method.clone()));
                        let declaration = Box::into_raw(Box::new(
                            DeclarationFFI::new_with_instance(declaration, dec.instance.clone()),
                        ));
                        let ext = v8::External::new(scope, declaration as _);

                        let builder = v8::Function::builder(
                            |scope: &mut v8::PinScope<'_, '_>,
                             args: v8::FunctionCallbackArguments,
                             mut retval: v8::ReturnValue| {
                                let dec = unsafe { args.data().cast::<v8::External>() };
                                let dec = dec.value() as *mut DeclarationFFI;
                                let dec = unsafe { &*dec };
                                let lock = dec.read();
                                let Some(method) =
                                    lock.as_any().downcast_ref::<MethodDeclaration>()
                                else {
                                    return;
                                };
                                let Some(ns_instance) =
                                    crate::ns_proxy::this_instance(scope, args.this_object())
                                        .or_else(|| dec.instance.clone())
                                else {
                                    return;
                                };
                                let mut method =
                                    {
                                    let alt = crate::class_helpers::overload_for_argc(method, args.length() as usize);
                                    let method = alt.as_ref().unwrap_or(method);
                                    MethodCall::new(method, method.is_sealed(), ns_instance, false)
                                };
                                let (ret, result, outs) = method.call(scope, &args);

                                if ret.is_err() {
                                    let detail = crate::error::format_hresult_message(ret);
                                    let message = v8::String::new(scope, &detail).unwrap();
                                    let error = v8::Exception::error(scope, message);
                                    scope.throw_exception(error);
                                    return;
                                }

                                if !outs.is_empty() {
                                    let mut arr_len = outs.len();
                                    if !method.is_void() {
                                        arr_len += 1;
                                    }
                                    let arr = v8::Array::new(scope, arr_len as i32);
                                    let mut idx = 0u32;

                                    if !method.is_void() {
                                        let return_sig = method.return_type().to_string();
                                        let mut return_value_opt: Option<Local<v8::Value>> = None;
                                        if return_sig.contains('.') {
                                            if let Some(declaration) =
                                                MetadataReader::find_by_name(return_sig.as_str())
                                            {
                                                if matches!(
                                                    declaration.read().kind(),
                                                    DeclarationKind::Struct
                                                ) {
                                                    let obj = crate::create_struct_object_from_raw(
                                                        declaration,
                                                        result,
                                                        scope,
                                                    )
                                                    .into();
                                                    return_value_opt = Some(obj);
                                                } else if !result.is_null() {
                                                    let instance =
                                                        unsafe { IUnknown::from_raw(result) };
                                                    let retv: Local<v8::Value> =
                                                        create_ns_ctor_instance_object(
                                                            return_sig.as_str(),
                                                            None,
                                                            None,
                                                            declaration,
                                                            Some(instance),
                                                            scope,
                                                        )
                                                        .into();
                                                    return_value_opt = Some(retv);
                                                } else {
                                                    return_value_opt = Some(v8::null(scope).into());
                                                }
                                            }
                                        }
                                        if return_value_opt.is_none() {
                                            if let Ok(return_type) =
                                                NativeType::try_from(return_sig.as_str())
                                            {
                                                let v = unsafe {
                                                    read_value_from_ptr(
                                                        result as *const c_void,
                                                        scope,
                                                        return_type,
                                                    )
                                                };
                                                return_value_opt = Some(v);
                                            }
                                        }
                                        if let Some(rv) = return_value_opt {
                                            arr.set_index(scope, idx, rv);
                                            idx += 1;
                                        }
                                    }

                                    for outv in outs.into_iter() {
                                        arr.set_index(scope, idx, outv);
                                        idx += 1;
                                    }
                                    retval.set(arr.into());
                                    return;
                                }

                                if method.is_void() {
                                    retval.set_undefined();
                                    return;
                                }

                                let return_sig = method.return_type().to_string();
                                if return_sig.contains('.') {
                                    if let Some(declaration) =
                                        MetadataReader::find_by_name(return_sig.as_str())
                                    {
                                        let ret: Local<v8::Value> = if matches!(
                                            declaration.read().kind(),
                                            DeclarationKind::Struct
                                        ) {
                                            create_struct_object_from_raw(
                                                declaration,
                                                result,
                                                scope,
                                            )
                                            .into()
                                        } else if result.is_null() {
                                            v8::null(scope).into()
                                        } else {
                                            let instance = unsafe { IUnknown::from_raw(result) };
                                            create_ns_ctor_instance_object(
                                                return_sig.as_str(),
                                                None,
                                                None,
                                                declaration,
                                                Some(instance),
                                                scope,
                                            )
                                            .into()
                                        };
                                        retval.set(ret.into());
                                        return;
                                    }
                                }

                                if let Ok(return_type) = NativeType::try_from(return_sig.as_str()) {
                                    unsafe {
                                        set_ret_val(result, scope, retval, return_type);
                                    }
                                }
                            },
                        )
                        .data(ext.into())
                        .build(scope)
                        .unwrap();

                        let func: Local<v8::Value> = builder.into();
                        if let Some(store_field) = holder.get_internal_field(scope, 1) {
                            let store = unsafe { store_field.cast::<v8::Map>() };
                            store.set(scope, key.into(), func);
                        }
                        rv.set(func);
                        return v8::Intercepted::kYes;
                    }

                    v8::Intercepted::kNo
                },
            )
            .data(v8::External::new(scope, declaration_ptr as _).into()),
    );

    instance_tmpl.set_indexed_property_handler(
        v8::IndexedPropertyHandlerConfiguration::new()
            .setter(handle_indexed_property_setter)
            .getter(handle_indexed_property_getter)
            .data(v8::External::new(scope, declaration_ptr as _).into()),
    );

    instance_tmpl.set_internal_field_count(2);
    tmpl.set_class_name(name);

    {
        let lock = declaration.read();

        if lock.kind() != DeclarationKind::Class {
            let iid = match lock.kind() {
                DeclarationKind::Interface => lock
                    .as_any()
                    .downcast_ref::<InterfaceDeclaration>()
                    .map(|d| d.id()),
                DeclarationKind::GenericInterfaceInstance => lock
                    .as_any()
                    .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                    .map(|d| d.id()),
                _ => None,
            };
            let full_name = lock.full_name().to_string();
            drop(lock);
            attach_has_instance_to_template(scope, tmpl, iid, &full_name);
            let Some(func) = tmpl.get_function(scope) else {
                CREATING_CTORS.with(|set| {
                    set.borrow_mut().remove(name_str);
                });
                return v8::undefined(scope).into();
            };
            CREATING_CTORS.with(|set| {
                set.borrow_mut().remove(name_str);
            });
            return func.into();
        }

        let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
            CREATING_CTORS.with(|set| {
                set.borrow_mut().remove(name_str);
            });
            return v8::undefined(scope).into();
        };
        let mut added_names: AHashSet<String> = AHashSet::new();

        for method in clazz.methods().iter() {
            let name = v8::String::new(scope, method.name());
            let is_static = method.is_static();

            if !is_static {
                continue;
            }

            // Skip duplicate method/property names to avoid adding the same
            // descriptor multiple times to the FunctionTemplate which can
            // corrupt V8 internal descriptor arrays.
            let m_name = method.name();
            if added_names.contains(m_name) {
                continue;
            }
            added_names.insert(m_name.to_string());

            let parent = Arc::clone(&declaration);

            let mut declaration =
                DeclarationFFI::new_with_instance(Arc::new(RwLock::new(method.clone())), None);

            declaration.parent = Some(parent);

            let declaration = Box::into_raw(Box::new(declaration));

            let ext = v8::External::new(scope, declaration as _);

            let func = v8::FunctionTemplate::builder(
                |scope: &mut v8::PinScope<'_, '_>,
                 args: v8::FunctionCallbackArguments,
                 mut retval: v8::ReturnValue| {
                    let dec = unsafe { args.data().cast::<v8::External>() };

                    let dec = dec.value() as *mut DeclarationFFI;

                    let dec = unsafe { &*dec };

                    let lock = dec.read();

                    let Some(method) = lock.as_any().downcast_ref::<MethodDeclaration>() else {
                        return;
                    };

                    let return_type = method.return_type();

                    let signature = method
                        .metadata()
                        .map(|m| Signature::to_string(m, &return_type))
                        .unwrap_or_default();

                    let factory = match resolve_class_factory_from_parent(dec) {
                        Ok(factory) => factory,
                        Err(error) => {
                            throw_js_error(
                                scope,
                                format!(
                                    "Failed to resolve WinRT static method factory for {}: {}",
                                    method.name(),
                                    error.message()
                                )
                                .as_str(),
                            );
                            return;
                        }
                    };

                    let mut method = MethodCall::new(method, method.is_sealed(), factory, false);

                    let (ret, result, outs) = method.call(scope, &args);

                    if ret.is_ok() {
                        if !outs.is_empty() {
                            let mut arr_len = outs.len();
                            if !method.is_void() {
                                arr_len += 1;
                            }
                            let arr = v8::Array::new(scope, arr_len as i32);
                            let mut idx = 0u32;

                            if !method.is_void() {
                                let mut return_value_opt: Option<Local<v8::Value>> = None;
                                if signature.contains('.') {
                                    if let Some(declaration) =
                                        MetadataReader::find_by_name(signature.as_str())
                                    {
                                        if matches!(
                                            declaration.read().kind(),
                                            DeclarationKind::Struct
                                        ) {
                                            return_value_opt = Some(
                                                create_struct_object_from_raw(
                                                    declaration,
                                                    result,
                                                    scope,
                                                )
                                                .into(),
                                            );
                                        } else if !result.is_null() {
                                            let instance = unsafe { IUnknown::from_raw(result) };
                                            return_value_opt = Some(
                                                create_ns_ctor_instance_object(
                                                    signature.as_str(),
                                                    dec.instance.clone(),
                                                    dec.parent.clone(),
                                                    declaration,
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into(),
                                            );
                                        } else {
                                            return_value_opt = Some(v8::null(scope).into());
                                        }
                                    }
                                }

                                if return_value_opt.is_none() {
                                    if signature == "Boolean" {
                                        return_value_opt = Some(
                                            v8::Boolean::new(scope, unsafe {
                                                *(result as *mut bool)
                                            })
                                            .into(),
                                        );
                                    } else if signature == "Guid" {
                                        let obj = unsafe { guid_ptr_to_js_object(result, scope) };
                                        return_value_opt = Some(obj.into());
                                    } else if !signature.contains('.') {
                                        if let Ok(return_type) =
                                            NativeType::try_from(signature.as_str())
                                        {
                                            let v = unsafe {
                                                read_value_from_ptr(
                                                    result as *const c_void,
                                                    scope,
                                                    return_type,
                                                )
                                            };
                                            return_value_opt = Some(v);
                                        }
                                    }
                                }

                                if let Some(rv) = return_value_opt {
                                    arr.set_index(scope, idx, rv);
                                    idx += 1;
                                }
                            }

                            for outv in outs.into_iter() {
                                arr.set_index(scope, idx, outv);
                                idx += 1;
                            }
                            retval.set(arr.into());
                            return;
                        }

                        unsafe {
                            match signature.as_str() {
                                "Boolean" => retval.set_bool(*(result as *mut bool)),
                                "Guid" => {
                                    let obj = unsafe { guid_ptr_to_js_object(result, scope) };
                                    retval.set(obj.into());
                                }
                                _ if !signature.contains('.') => {
                                    // Primitive / value-type return: use set_ret_val when possible.
                                    match NativeType::try_from(signature.as_str()) {
                                        Ok(return_type) => {
                                            set_ret_val(result, scope, retval, return_type);
                                        }
                                        Err(_) => {
                                            retval.set_undefined();
                                        }
                                    }
                                }
                                _ => {
                                    if result.is_null() {
                                        retval.set(v8::null(scope).into());
                                        return;
                                    }
                                    if let Some(declaration) =
                                        MetadataReader::find_by_name(signature.as_str())
                                    {
                                        let ret: Local<v8::Value> = if matches!(
                                            declaration.read().kind(),
                                            DeclarationKind::Struct
                                        ) {
                                            create_struct_object_from_raw(
                                                declaration,
                                                result,
                                                scope,
                                            )
                                            .into()
                                        } else {
                                            let instance = IUnknown::from_raw(result);
                                            create_ns_ctor_instance_object(
                                                signature.as_str(),
                                                dec.instance.clone(),
                                                dec.parent.clone(),
                                                declaration,
                                                Some(instance),
                                                scope,
                                            )
                                            .into()
                                        };
                                        retval.set(ret.into());
                                    } else {
                                        let instance = IUnknown::from_raw(result);
                                        let Some(declaration) =
                                            MetadataReader::find_by_name(signature.as_str())
                                        else {
                                            return;
                                        };
                                        let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                                            signature.as_str(),
                                            dec.instance.clone(),
                                            dec.parent.clone(),
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into();
                                        retval.set(ret.into());
                                    }
                                } // end _ (COM object)
                            } // end match signature
                        } // end unsafe
                    } else {
                        let detail = crate::error::format_hresult_message(ret);
                        let message = v8::String::new(scope, &detail).unwrap();
                        let error = v8::Exception::error(scope, message.into());
                        scope.throw_exception(error);
                    }
                },
            )
            .data(ext.into())
            .build(scope);

            tmpl.set(name.unwrap().into(), func.into());
        }

        // Register lazy accessor properties for each static property, including
        // inherited statics from base classes (e.g. UIElement.PointerPressedEvent on Panel).
        // The WinRT getter is only invoked when JS actually reads the property,
        // avoiding eager FFI calls at class-lookup time that crash for types
        // with many static DependencyProperty members (e.g. ScrollViewer).
        let all_static_props = crate::class_helpers::collect_class_properties_with_declaring(clazz);
        for (property, declaring_class_name) in all_static_props.iter() {
            if !property.is_static() {
                continue;
            }

            let prop_name_str = property.name();
            if added_names.contains(prop_name_str) {
                continue;
            }
            added_names.insert(prop_name_str.to_string());

            let Some(prop_name) = v8::String::new(scope, property.name()) else {
                continue;
            };

            let mut prop_dec =
                DeclarationFFI::new_with_instance(Arc::new(RwLock::new(property.clone())), None);
            // Lazy: for own-class statics use the declaration parent; for inherited
            // statics store only the class name so RoGetActivationFactory is called
            // on first property access, not at constructor-build time.
            if declaring_class_name.as_str() == clazz.full_name() {
                prop_dec.parent = Some(Arc::clone(&declaration));
            } else {
                prop_dec.static_factory_class = Some(declaring_class_name.clone());
            }
            let prop_ext = Box::into_raw(Box::new(prop_dec));
            let prop_ext = v8::External::new(scope, prop_ext as _);

            let getter = v8::FunctionTemplate::builder(
                |scope: &mut v8::PinScope<'_, '_>,
                 args: v8::FunctionCallbackArguments,
                 mut retval: v8::ReturnValue| {
                    let dec = unsafe { args.data().cast::<v8::External>() };
                    let dec = dec.value() as *mut DeclarationFFI;
                    let dec = unsafe { &*dec };

                    let lock = dec.read();
                    let Some(property) = lock.as_any().downcast_ref::<PropertyDeclaration>() else {
                        return;
                    };

                    let signature = {
                        let sig = property.getter().return_type();
                        match property.getter().metadata() {
                            Some(md) => Signature::to_string(md, &sig),
                            None => return,
                        }
                    };

                    let factory = match resolve_class_factory_from_parent(dec) {
                        Ok(f) => f,
                        Err(e) => {
                            throw_js_error(
                                scope,
                                &format!(
                                    "Failed to resolve static property factory: {}",
                                    e.message()
                                ),
                            );
                            return;
                        }
                    };

                    let prop_call_opt = PropertyCall::new(property, false, factory, false);
                    if prop_call_opt.is_none() {
                        return;
                    }

                    let mut prop_call = prop_call_opt.unwrap();

                    let (hresult, result, _outs) = prop_call.call_with_values(scope, &[]);

                    // If the call failed because the process is unpackaged, do not provide a runtime shim.
                    // HRESULT 0x80073D54 = "The process has no package identity.": let callers/tests handle fallback.
                    if !hresult.is_ok() && (hresult.0 as u32) == 0x80073D54u32 {
                        // If the static getter failed because the process is unpackaged,
                        // do not synthesize values here; let the caller/tests detect the
                        // missing package identity via the diagnostic marker on the
                        // constructor/object and provide any test-only fallbacks.

                        // Expose a small, non-invasive diagnostic on the JS constructor/object
                        // so code running in V8 can inspect why the value was absent.
                        if let Some(obj) = args.this().to_object(scope) {
                            let key = v8::String::new(scope, "__missingPackageIdentity__").unwrap();
                            let val =
                                v8::String::new(scope, &hresult.message().to_string()).unwrap();
                            let _ = obj.set(scope, key.into(), val.into());
                        }

                        retval.set_undefined();
                        return;
                    }

                    if hresult.is_ok() {
                        unsafe {
                            match signature.as_str() {
                                "Boolean" => {
                                    retval.set_bool(*(result as *mut bool));
                                }
                                "Guid" => {
                                    let obj = unsafe { guid_ptr_to_js_object(result, scope) };
                                    retval.set(obj.into());
                                }
                                _ if !signature.contains('.') => {
                                    match NativeType::try_from(signature.as_str()) {
                                        Ok(return_type) => {
                                            set_ret_val(result, scope, retval, return_type);
                                        }
                                        Err(_) => {
                                            retval.set_undefined();
                                        }
                                    }
                                }
                                _ => {
                                    if result.is_null() {
                                        retval.set(v8::null(scope).into());
                                        return;
                                    }
                                    if let Some(declaration) =
                                        MetadataReader::find_by_name(signature.as_str())
                                    {
                                        let ret: Local<v8::Value> = if matches!(
                                            declaration.read().kind(),
                                            DeclarationKind::Struct
                                        ) {
                                            create_struct_object_from_raw(
                                                declaration,
                                                result,
                                                scope,
                                            )
                                            .into()
                                        } else {
                                            let instance = IUnknown::from_raw(result);
                                            create_ns_ctor_instance_object(
                                                signature.as_str(),
                                                dec.instance.clone(),
                                                dec.parent.clone(),
                                                declaration,
                                                Some(instance),
                                                scope,
                                            )
                                            .into()
                                        };
                                        retval.set(ret.into());
                                    } else {
                                        let instance = IUnknown::from_raw(result);
                                        let Some(ret_decl) =
                                            MetadataReader::find_by_name(signature.as_str())
                                        else {
                                            return;
                                        };
                                        let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                                            signature.as_str(),
                                            dec.instance.clone(),
                                            dec.parent.clone(),
                                            ret_decl,
                                            Some(instance),
                                            scope,
                                        )
                                        .into();
                                        retval.set(ret.into());
                                    }
                                }
                            }
                        }
                    }
                },
            )
            .data(prop_ext.into())
            .build(scope);

            tmpl.set_accessor_property(
                prop_name.into(),
                Some(getter),
                None,
                v8::PropertyAttribute::DONT_DELETE,
            );
        }

        attach_has_instance_to_template(scope, tmpl, None, &clazz.full_name().to_string());
    }

    let Some(func) = tmpl.get_function(scope) else {
        CREATING_CTORS.with(|set| {
            set.borrow_mut().remove(name_str);
        });
        return v8::undefined(scope).into();
    };

    {
        let lock = declaration.read();
        if let Some(full_name) = match lock.kind() {
            DeclarationKind::Class => lock
                .as_any()
                .downcast_ref::<ClassDeclaration>()
                .map(|clazz| clazz.full_name().to_string()),
            _ => None,
        } {
            let key = v8::String::new(scope, "__typeName__").unwrap();
            let value = v8::String::new(scope, full_name.as_str()).unwrap();
            func.set(scope, key.into(), value.into());
        }
    }
    let ret = func;
    CREATING_CTORS.with(|set| {
        set.borrow_mut().remove(name_str);
    });
    ret.into()
}

#[cfg(feature = "classic")]
fn create_ns_struct_ctor_object<'a>(
    name: &str,
    declaration: Arc<RwLock<dyn Declaration>>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    let name = v8::String::new(scope, name).unwrap();

    let ext = DeclarationFFI::new(Arc::clone(&declaration));

    let ext = Box::into_raw(Box::new(ext));

    let ext = v8::External::new(scope, ext as _);

    let tmpl = FunctionTemplate::builder(
        |scope: &mut v8::PinScope<'_, '_>,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let dec = unsafe { args.data().cast::<v8::External>() };

            let dec = dec.value() as *mut DeclarationFFI;

            let dec = unsafe { &mut *dec };

            let lock = dec.write();

            let ext = args.data();

            let object_tmpl = v8::ObjectTemplate::new(scope);
            object_tmpl.set_internal_field_count(1);

            let mut field_args: Vec<NativeValue> = Vec::new();

            let mut field_types: Vec<NativeType> = Vec::new();

            let Some(struct_dec) = lock.as_any().downcast_ref::<StructDeclaration>() else {
                return;
            };

            // Support both positional args `new Thickness(5, 10, 15, 20)` and
            // named-field object `new Thickness({ Left: 5, Top: 10, Right: 15, Bottom: 20 })`.
            let use_positional = args.length() > 0 && !args.get(0).is_object();
            let named_object: Option<v8::Local<v8::Object>> = if !use_positional {
                match args.get(0).to_object(scope) {
                    Some(obj) => Some(obj),
                    None => {
                        throw_js_error(
                            scope,
                            "Expected object or positional arguments for struct constructor",
                        );
                        return;
                    }
                }
            } else {
                None
            };

            for (field_idx, field) in struct_dec.fields().iter().enumerate() {
                let Some(metadata) = field.base().metadata() else {
                    continue;
                };
                let field_type = Signature::to_string(metadata, &field.type_());

                let Ok(native_type) = NativeType::try_from(field_type.as_str()) else {
                    continue;
                };

                field_types.push(native_type.clone());

                let field_value = if use_positional {
                    Some(args.get(field_idx as i32))
                } else {
                    let Some(name) = v8::String::new(scope, field.name()) else {
                        continue;
                    };
                    named_object.unwrap().get(scope, name.into())
                };

                match field_value {
                    None => {
                        let message = format!("Missing key {}", field.name());
                        let message = v8::String::new(scope, message.as_str()).unwrap();
                        let error = v8::Exception::error(scope, message.into());
                        scope.throw_exception(error);
                    }
                    Some(field) => {
                        let value = match native_type {
                            NativeType::Void => Err(error::type_error(
                                "Void is not a valid WinRT struct field type",
                            )),
                            NativeType::Bool => ffi_parse_bool_arg(field),
                            NativeType::U8 => ffi_parse_u8_arg(field),
                            NativeType::I8 => ffi_parse_i8_arg(field),
                            NativeType::U16 => ffi_parse_u16_arg(field),
                            NativeType::I16 => ffi_parse_i16_arg(field),
                            NativeType::U32 => ffi_parse_u32_arg(field),
                            NativeType::I32 => ffi_parse_i32_arg(field),
                            NativeType::U64 => ffi_parse_u64_arg(scope, field),
                            NativeType::I64 => ffi_parse_i64_arg(scope, field),
                            NativeType::USize => ffi_parse_usize_arg(scope, field),
                            NativeType::ISize => ffi_parse_isize_arg(scope, field),
                            NativeType::F32 => ffi_parse_f32_arg(field),
                            NativeType::F64 => ffi_parse_f64_arg(field),
                            NativeType::Pointer => ffi_parse_pointer_arg(scope, field),
                            NativeType::Buffer => ffi_parse_buffer_arg(scope, field),
                            NativeType::Function => ffi_parse_function_arg(scope, field),
                            NativeType::Struct(_) => ffi_parse_struct_arg(scope, field),
                            NativeType::String => ffi_parse_string_arg(scope, field),
                        };
                        match value {
                            Ok(value) => {
                                field_args.push(value);
                            }
                            Err(err) => {
                                let message = err.to_string();
                                let message = v8::String::new(scope, message.as_str()).unwrap();
                                let error = v8::Exception::error(scope, message.into());
                                scope.throw_exception(error);
                            }
                        }
                    }
                }
            }

            let mut struct_size = 0_usize;

            let params = field_types
                .clone()
                .into_iter()
                .map(|item| {
                    struct_size = struct_size + item.size();
                    libffi::middle::Type::try_from(item)
                })
                .collect::<Result<Vec<libffi::middle::Type>, error::AnyError>>();

            if params.is_err() {
                return;
            }

            let mut struct_buf: Vec<u8> = unsafe { vec![0_u8; struct_size] };

            struct_buf.shrink_to_fit();

            let mut position = 0_isize;

            for (field_type, field_value) in field_types.iter().zip(field_args.iter()) {
                let size = field_type.size();

                unsafe {
                    let buffer = struct_buf.as_mut_ptr();
                    let buffer = buffer.offset(position);

                    let value: *mut u8 = std::mem::transmute(field_value.as_arg(field_type));

                    let slice = std::slice::from_raw_parts_mut(buffer, size);

                    std::ptr::copy(value, slice.as_mut_ptr(), size);
                }

                position = position + size as isize;
            }

            let name = lock.name().to_string();

            drop(lock);

            dec.struct_instance = Some((struct_buf, field_types));

            let _name = v8::String::new(scope, name.as_str()).unwrap();

            let getter = |scope: &mut v8::PinScope<'_, '_>,
                          key: Local<v8::Name>,
                          args: v8::PropertyCallbackArguments,
                          mut rv: v8::ReturnValue<v8::Value>|
             -> v8::Intercepted {
                let key = key.to_rust_string_lossy(scope);

                let this = args.data();

                let dec = unsafe { this.cast::<v8::External>() };

                let dec = dec.value() as *mut DeclarationFFI;

                let dec = unsafe { &*dec };

                let lock = dec.read();

                if key == "toString" {
                    let name = lock.name();

                    let name = v8::String::new(scope, name).unwrap();
                    let func = v8::Function::builder(
                        |_scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            retval.set(args.data());
                        },
                    )
                    .data(name.into())
                    .build(scope);

                    if let Some(f) = func {
                        rv.set(f.into());
                    }
                    return v8::Intercepted::kYes;
                }

                let Some(struct_dec) = lock.as_any().downcast_ref::<StructDeclaration>() else {
                    return v8::Intercepted::kNo;
                };

                let mut offset = 0;
                let instance = dec.struct_instance.as_ref();
                let mut position = 0;
                for field in struct_dec.fields() {
                    if field.name() == key.as_str() {
                        if let Some((buffer, types)) = instance {
                            let mut current_field_position = 0;
                            for field_type in types.iter() {
                                let size = field_type.size();

                                if position == current_field_position {
                                    unsafe {
                                        let buffer = buffer.as_ptr();
                                        let buffer = buffer.offset(offset);

                                        let slice = std::slice::from_raw_parts(buffer, size);

                                        match field_type {
                                            NativeType::Void => {}
                                            NativeType::Bool => {
                                                let ret: &u8 = std::mem::transmute(
                                                    slice.as_ptr() as *const u8
                                                );
                                                rv.set_bool(*ret == 1);
                                            }
                                            NativeType::U8 => {
                                                let ret: &u8 = std::mem::transmute(
                                                    slice.as_ptr() as *const u8
                                                );
                                                rv.set_uint32(*ret as u32);
                                            }
                                            NativeType::I8 => {
                                                let ret: &i8 = std::mem::transmute(
                                                    slice.as_ptr() as *const i8
                                                );
                                                rv.set_int32(*ret as i32);
                                            }
                                            NativeType::U16 => {
                                                let ret: &u16 = std::mem::transmute(
                                                    slice.as_ptr() as *const u16
                                                );
                                                rv.set_uint32(*ret as u32);
                                            }
                                            NativeType::I16 => {
                                                let ret: &i16 = std::mem::transmute(
                                                    slice.as_ptr() as *const i16
                                                );
                                                rv.set_int32(*ret as i32);
                                            }
                                            NativeType::U32 => {
                                                let ret: &u32 = std::mem::transmute(
                                                    slice.as_ptr() as *const u32
                                                );
                                                rv.set_uint32(*ret);
                                            }
                                            NativeType::I32 => {
                                                let ret: &i32 = std::mem::transmute(
                                                    slice.as_ptr() as *const i32
                                                );
                                                rv.set_int32(*ret);
                                            }
                                            NativeType::U64 => {
                                                let ret: u64 =
                                                    *std::mem::transmute::<*const u64, &u64>(
                                                        slice.as_ptr() as *const u64,
                                                    );

                                                let local_value: v8::Local<v8::Value> =
                                                    if ret > MAX_SAFE_INTEGER as u64 {
                                                        v8::BigInt::new_from_u64(scope, ret).into()
                                                    } else {
                                                        v8::Number::new(scope, ret as f64).into()
                                                    };

                                                rv.set(local_value);
                                            }
                                            NativeType::I64 => {
                                                let ret: i64 =
                                                    *std::mem::transmute::<*const i64, &i64>(
                                                        slice.as_ptr() as *const i64,
                                                    );
                                                let local_value: v8::Local<v8::Value> = if ret
                                                    > MAX_SAFE_INTEGER as i64
                                                    || ret < MIN_SAFE_INTEGER as i64
                                                {
                                                    v8::BigInt::new_from_i64(scope, ret).into()
                                                } else {
                                                    v8::Number::new(scope, ret as f64).into()
                                                };
                                                rv.set(local_value);
                                            }
                                            NativeType::USize => {}
                                            NativeType::ISize => {}
                                            NativeType::F32 => {
                                                //let ret: &f32 = std::mem::transmute(slice.as_ptr() as *const f32);

                                                let ret: f32 = if cfg!(target_endian = "big") {
                                                    f32::from_be_bytes(
                                                        <[u8; 4]>::try_from(slice).unwrap(),
                                                    )
                                                } else {
                                                    f32::from_le_bytes(
                                                        <[u8; 4]>::try_from(slice).unwrap(),
                                                    )
                                                };

                                                rv.set(v8::Number::new(scope, ret as f64).into());
                                            }
                                            NativeType::F64 => {
                                                let ret: &f64 = std::mem::transmute(
                                                    slice.as_ptr() as *const f64
                                                );
                                                rv.set(v8::Number::new(scope, *ret).into());
                                            }
                                            NativeType::Pointer => {}
                                            NativeType::Buffer => {}
                                            NativeType::Function => {}
                                            NativeType::Struct(_) => {}
                                            NativeType::String => {
                                                // TODO
                                            }
                                        }
                                    }
                                }

                                current_field_position = current_field_position + 1;

                                offset = offset + size as isize;
                            }
                        }
                        break;
                    }
                    position = position + 1;
                }
                v8::Intercepted::kYes
            };

            let setter = |scope: &mut v8::PinScope<'_, '_>,
                          key: Local<v8::Name>,
                          value: Local<v8::Value>,
                          args: v8::PropertyCallbackArguments,
                          _rv: v8::ReturnValue<()>|
             -> v8::Intercepted {
                let key = key.to_rust_string_lossy(scope);

                let this = args.data();

                let dec = unsafe { this.cast::<v8::External>() };

                let dec = dec.value() as *mut DeclarationFFI;

                let instance = unsafe { (&mut *dec).struct_instance.as_mut() };

                let dec = unsafe { &mut *dec };

                let lock = dec.write();

                let Some(struct_dec) = lock.as_any().downcast_ref::<StructDeclaration>() else {
                    return v8::Intercepted::kNo;
                };

                let mut offset = 0;

                let mut position = 0;
                for field in struct_dec.fields() {
                    if field.name() == key.as_str() {
                        if let Some((buffer, types)) = instance {
                            let field = value;

                            let mut current_field_position = 0;
                            for field_type in types.iter() {
                                let size = field_type.size();

                                if position == current_field_position {
                                    let value = match field_type {
                                        NativeType::Void => Err(error::type_error(
                                            "Void is not a valid WinRT struct field type",
                                        )),
                                        NativeType::Bool => ffi_parse_bool_arg(field),
                                        NativeType::U8 => ffi_parse_u8_arg(field),
                                        NativeType::I8 => ffi_parse_i8_arg(field),
                                        NativeType::U16 => ffi_parse_u16_arg(field),
                                        NativeType::I16 => ffi_parse_i16_arg(field),
                                        NativeType::U32 => ffi_parse_u32_arg(field),
                                        NativeType::I32 => ffi_parse_i32_arg(field),
                                        NativeType::U64 => ffi_parse_u64_arg(scope, field),
                                        NativeType::I64 => ffi_parse_i64_arg(scope, field),
                                        NativeType::USize => ffi_parse_usize_arg(scope, field),
                                        NativeType::ISize => ffi_parse_isize_arg(scope, field),
                                        NativeType::F32 => ffi_parse_f32_arg(field),
                                        NativeType::F64 => ffi_parse_f64_arg(field),
                                        NativeType::Pointer => ffi_parse_pointer_arg(scope, field),
                                        NativeType::Buffer => ffi_parse_buffer_arg(scope, field),
                                        NativeType::Function => {
                                            ffi_parse_function_arg(scope, field)
                                        }
                                        NativeType::Struct(_) => ffi_parse_struct_arg(scope, field),
                                        NativeType::String => ffi_parse_string_arg(scope, field),
                                    };
                                    match value {
                                        Ok(value) => unsafe {
                                            let buffer = buffer.as_mut_ptr();
                                            let buffer = buffer.offset(offset);

                                            let value: *mut u8 =
                                                std::mem::transmute(value.as_arg(field_type));

                                            let slice =
                                                std::slice::from_raw_parts_mut(buffer, size);

                                            std::ptr::copy(value, slice.as_mut_ptr(), size);
                                        },
                                        Err(err) => {
                                            let message = err.to_string();
                                            let message =
                                                v8::String::new(scope, message.as_str()).unwrap();
                                            let error = v8::Exception::error(scope, message.into());
                                            scope.throw_exception(error);
                                        }
                                    }
                                }

                                current_field_position = current_field_position + 1;

                                offset = offset + size as isize;
                            }
                        }
                        break;
                    }
                    position = position + 1;
                }
                v8::Intercepted::kYes
            };

            object_tmpl.set_named_property_handler(
                v8::NamedPropertyHandlerConfiguration::new()
                    .getter(getter)
                    .setter(setter)
                    .data(ext),
            );

            let Some(object) = object_tmpl.new_instance(scope) else {
                return;
            };

            object.set_internal_field(0, ext.into());

            retval.set(object.into());
        },
    )
    .data(ext.into())
    .build(scope);
    tmpl.set_class_name(name);

    let Some(func) = tmpl.get_function(scope) else {
        return v8::undefined(scope).into();
    };
    let ret = func;

    ret.into()
}

#[cfg(feature = "classic")]
pub(crate) fn create_struct_object_from_raw<'a>(
    declaration: Arc<RwLock<dyn Declaration>>,
    raw_data: *mut c_void,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Object> {
    let fallback = v8::Object::new(scope);

    // Build the byte buffer from raw_data before setting up the template.
    let (struct_buf, field_types) = {
        let lock = declaration.read();
        let Some(struct_dec) = lock.as_any().downcast_ref::<StructDeclaration>() else {
            return fallback;
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut types: Vec<NativeType> = Vec::new();
        let mut offset: isize = 0;
        for field in struct_dec.fields() {
            let Some(metadata) = field.base().metadata() else {
                continue;
            };
            let field_type_str = Signature::to_string(metadata, &field.type_());
            let Ok(native_type) = NativeType::try_from(field_type_str.as_str()) else {
                continue;
            };
            let size = native_type.size() as isize;
            let field_ptr = unsafe { (raw_data as *const u8).offset(offset) as *mut c_void };
            let buf_start = buf.len();
            buf.extend(std::iter::repeat(0u8).take(size as usize));
            unsafe {
                std::ptr::copy_nonoverlapping(
                    field_ptr as *const u8,
                    buf[buf_start..].as_mut_ptr(),
                    size as usize,
                );
            }
            types.push(native_type);
            offset += size;
        }
        (buf, types)
    };

    // Store the byte buffer in a DeclarationFFI so native code can read it back
    // via ffi_parse_pointer_arg → try_get_external_handle, AND so the property
    // interceptors can keep reads/writes in sync with the same buffer.
    let mut dec_ffi = DeclarationFFI::new(Arc::clone(&declaration));
    dec_ffi.struct_instance = Some((struct_buf, field_types));
    let dec_raw = Box::into_raw(Box::new(dec_ffi));
    let ext = v8::External::new(scope, dec_raw as *mut c_void);

    // Use property interceptors so that JS writes (e.g. `size.Width = 300`) go
    // through to the buffer, not just to a detached plain JS property.
    let tmpl = v8::ObjectTemplate::new(scope);
    tmpl.set_internal_field_count(1);
    tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .getter(crate::ns_proxy::ns_struct_field_getter)
            .setter(crate::ns_proxy::ns_struct_field_setter)
            .enumerator(crate::ns_proxy::ns_struct_field_enumerator)
            .data(ext.into()),
    );
    let Some(object) = tmpl.new_instance(scope) else {
        return fallback;
    };
    object.set_internal_field(0, ext.into());
    object
}

#[cfg(feature = "classic")]
fn init_meta(
    scope: &mut v8::ContextScope<v8::HandleScope<v8::Context>>,
    context: Local<v8::Context>,
) {
    let global = context.global(scope);
    let Some(global_metadata) = MetadataReader::find_by_name("") else {
        return;
    };
    let data = global_metadata.read();
    let ns = data.as_any().downcast_ref::<NamespaceDeclaration>();
    if let Some(global_namespaces) = ns {
        let full_name = global_namespaces.full_name();
        for ns in global_namespaces.children() {
            let full_name = if full_name.is_empty() {
                ns.to_string()
            } else {
                format!("{}.{}", full_name, ns)
            };

            let name: Local<v8::Name> = v8::String::new(scope, ns.as_str()).unwrap().into();
            if let Some(name_space) = MetadataReader::find_by_name(full_name.as_str()) {
                let object = create_ns_object(ns, name_space, scope);
                global.define_own_property(
                    scope,
                    name,
                    object,
                    v8::PropertyAttribute::READ_ONLY
                        | v8::PropertyAttribute::DONT_DELETE
                        | v8::PropertyAttribute::NONE,
                );
            }
        }
    }
}

// Setter for the namespace/enum/struct *proxy* objects (the ones returned by
// `create_ns_object`). These are not instances: they're traversal handles like
// `Windows` or `Windows.UI.Popups`. The rule is:
//   - Names that resolve to real WinRT metadata are immutable (writes are ignored).
//   - Anything else is stored in the per-object side map so user code can stash
//     custom properties (e.g. `Windows.myShim = ...`) without breaking lookups.
#[cfg(feature = "classic")]
fn handle_named_property_setter(
    scope: &mut v8::PinScope<'_, '_>,
    key: Local<v8::Name>,
    value: Local<v8::Value>,
    args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    let this = args.holder();
    let Some(dec_field) = this.get_internal_field(scope, 0) else {
        return v8::Intercepted::kNo;
    };
    let dec = unsafe { dec_field.cast::<v8::External>() }.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let lock = dec.read();
    let kind = lock.kind();

    let Some(store_field) = this.get_internal_field(scope, 1) else {
        return v8::Intercepted::kNo;
    };
    let store = unsafe { store_field.cast::<v8::Map>() };

    let name = key.to_rust_string_lossy(scope);

    // Returns true if `name` is a name reserved by the WinRT metadata for this
    // declaration kind. Reserved names are read-only.
    let is_reserved = match kind {
        DeclarationKind::Namespace => lock
            .as_any()
            .downcast_ref::<NamespaceDeclaration>()
            .map(|d| d.children().contains(&name))
            .unwrap_or(false),
        DeclarationKind::Enum => lock
            .as_any()
            .downcast_ref::<EnumDeclaration>()
            .map(|d| d.enum_for_name(&name).is_some())
            .unwrap_or(false),
        DeclarationKind::Class => lock
            .as_any()
            .downcast_ref::<ClassDeclaration>()
            .map(|d| class_has_member_named(d, &name))
            .unwrap_or(false),
        DeclarationKind::Interface => lock
            .as_any()
            .downcast_ref::<InterfaceDeclaration>()
            .map(|d| {
                d.methods().iter().any(|m| m.name() == name)
                    || d.properties().iter().any(|p| p.name() == name)
            })
            .unwrap_or(false),
        DeclarationKind::Struct => lock
            .as_any()
            .downcast_ref::<StructDeclaration>()
            .map(|d| d.fields().iter().any(|f| f.name() == name))
            .unwrap_or(false),
        // For everything else there's no metadata to clash with, so the
        // assignment can be stored verbatim.
        _ => false,
    };

    // For class instances: wire WinRT event handlers via the add/remove ABI methods.
    // `button.Click = delegate` calls add_Click; replacing removes the prior handler first.
    if kind == DeclarationKind::Class && !is_reserved {
        if let Some(class) = lock.as_any().downcast_ref::<ClassDeclaration>() {
            if let Some((add_method, remove_method)) = find_event_methods(class, &name) {
                let instance = dec.instance.clone();
                drop(lock);
                return crate::ns_proxy::wire_winrt_event(
                    scope,
                    &name,
                    instance,
                    &add_method,
                    &remove_method,
                    value,
                );
            }
        }
    }

    if !is_reserved
        && matches!(
            kind,
            DeclarationKind::Interface | DeclarationKind::GenericInterfaceInstance
        )
    {
        if let Some((add_method, remove_method)) =
            crate::class_helpers::find_interface_event_methods(&*lock, &name)
        {
            let instance = dec.instance.clone();
            drop(lock);
            return crate::ns_proxy::wire_winrt_event(
                scope,
                &name,
                instance,
                &add_method,
                &remove_method,
                value,
            );
        }
    }

    if !is_reserved {
        store.set(scope, key.into(), value);
        v8::Intercepted::kYes
    } else {
        v8::Intercepted::kNo
    }
}

#[cfg(feature = "classic")]
fn handle_named_property_query(
    _scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<v8::Name>,
    _args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Integer>,
) -> v8::Intercepted {
    // NONE
    rv.set_int32(0);
    v8::Intercepted::kNo
}

#[cfg(feature = "classic")]
fn handle_named_property_getter(
    scope: &mut v8::PinScope<'_, '_>,
    key: v8::Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    let this = args.holder();
    let Some(dec) = this.get_internal_field(scope, 0) else {
        return v8::Intercepted::kNo;
    };
    let dec = unsafe { dec.cast::<v8::External>() };
    let dec = dec.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let lock = dec.read();
    let Some(store) = this.get_internal_field(scope, 1) else {
        return v8::Intercepted::kNo;
    };
    let store = unsafe { store.cast::<v8::Map>() };
    let kind = lock.kind();

    if key.is_string() {
        if let Some(cache) = store.get(scope, key.into()) {
            if !cache.is_null_or_undefined() {
                rv.set(cache);
                return v8::Intercepted::kYes;
            }
        }

        let name = match key.to_string(scope) {
            Some(s) => s.to_rust_string_lossy(scope),
            None => return v8::Intercepted::kNo,
        };
        match kind {
            DeclarationKind::Namespace => {
                let parent = dec.inner.clone();
                let dec = lock.as_any().downcast_ref::<NamespaceDeclaration>();
                if let Some(dec) = dec {
                    let full_name = format!("{}.{}", dec.full_name(), name.as_str());

                    if let Some(dec) = MetadataReader::find_by_name_or_generic(full_name.as_str()) {
                        let declaration = Arc::clone(&dec);
                        let lock = dec.read();

                        match lock.kind() {
                            DeclarationKind::Struct => {
                                let Some(struct_dec) =
                                    lock.as_any().downcast_ref::<StructDeclaration>()
                                else {
                                    return v8::Intercepted::kNo;
                                };
                                let name = struct_dec.name().to_string();
                                drop(lock);

                                let ret = create_ns_struct_ctor_object(
                                    name.as_str(),
                                    Arc::clone(&dec),
                                    scope,
                                );
                                let ret: Local<v8::Value> = ret.into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                            DeclarationKind::Class => {
                                let ret: Local<v8::Value> = create_ns_ctor_object(
                                    lock.name(),
                                    Some(parent),
                                    declaration,
                                    scope,
                                )
                                .into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                            DeclarationKind::Interface
                            | DeclarationKind::GenericInterface
                            | DeclarationKind::GenericInterfaceInstance
                            | DeclarationKind::Delegate
                            | DeclarationKind::GenericDelegate
                            | DeclarationKind::GenericDelegateInstance
                            | DeclarationKind::Event => {
                                let ret: Local<v8::Value> = create_ns_ctor_object(
                                    lock.name(),
                                    Some(parent),
                                    declaration,
                                    scope,
                                )
                                .into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                            _ => {
                                let ret: Local<v8::Value> =
                                    create_ns_object(name.as_str(), declaration, scope).into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                        }
                        return v8::Intercepted::kYes;
                    }

                    // Name isn't a real namespace child: let V8 fall back to
                    // Object.prototype defaults so toString/valueOf/etc. work.
                    return v8::Intercepted::kNo;
                }
            }
            DeclarationKind::Class => {
                let clazz_dec = lock.as_any().downcast_ref::<ClassDeclaration>();

                if let Some(clazz_dec) = clazz_dec {
                    if let Some(method) = find_class_method(clazz_dec, name.as_str()) {
                        crate::class_helpers::register_overload_siblings(clazz_dec, name.as_str(), &method);
                        let declaration = Arc::new(RwLock::new(method));

                        let declaration = Box::into_raw(Box::new(
                            DeclarationFFI::new_with_instance(declaration, dec.instance.clone()),
                        ));

                        let ext = v8::External::new(scope, declaration as _);

                        let builder = v8::Function::builder(
                            |scope: &mut v8::PinScope<'_, '_>,
                             args: v8::FunctionCallbackArguments,
                             _retval: v8::ReturnValue| {
                                let _length = args.length();

                                let dec = unsafe { args.data().cast::<v8::External>() };

                                let dec = dec.value() as *mut DeclarationFFI;

                                let dec = unsafe { &*dec };

                                let lock = dec.read();

                                let Some(method) =
                                    lock.as_any().downcast_ref::<MethodDeclaration>()
                                else {
                                    return;
                                };

                                let Some(ns_instance) =
                                    crate::ns_proxy::this_instance(scope, args.this_object())
                                        .or_else(|| dec.instance.clone())
                                else {
                                    return;
                                };

                                let mut method =
                                    {
                                    let alt = crate::class_helpers::overload_for_argc(method, args.length() as usize);
                                    let method = alt.as_ref().unwrap_or(method);
                                    MethodCall::new(method, method.is_sealed(), ns_instance, false)
                                };

                                let (_ret, _result, _outs) = method.call(scope, &args);
                            },
                        )
                        .data(ext.into())
                        .build(scope);

                        let Some(func) = builder else {
                            return v8::Intercepted::kNo;
                        };

                        let func: Local<v8::Value> = func.into();
                        store.set(scope, key.into(), func);
                        rv.set(func);
                        return v8::Intercepted::kYes;
                    }

                    if dec.instance.is_some() && find_event_methods(clazz_dec, &name).is_some() {
                        let handler =
                            crate::ns_proxy::read_winrt_event(scope, dec.instance.as_ref(), &name);
                        rv.set(handler);
                        return v8::Intercepted::kYes;
                    }
                }
            }
            DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance
            | DeclarationKind::Delegate
            | DeclarationKind::GenericDelegate
            | DeclarationKind::GenericDelegateInstance
            | DeclarationKind::Event => {
                if let Some(impl_key) = v8::String::new(scope, "__implementation__") {
                    if let Some(implementation) = store.get(scope, impl_key.into()) {
                        if let Some(implementation) = implementation.to_object(scope) {
                            if let Some(value) = implementation.get(scope, key.into()) {
                                if !value.is_null_or_undefined() {
                                    store.set(scope, key.into(), value);
                                    rv.set(value);
                                    return v8::Intercepted::kYes;
                                }
                            }
                        }
                    }
                }

                if dec.instance.is_some()
                    && crate::class_helpers::find_interface_event_methods(&*lock, &name).is_some()
                {
                    let handler =
                        crate::ns_proxy::read_winrt_event(scope, dec.instance.as_ref(), &name);
                    rv.set(handler);
                    return v8::Intercepted::kYes;
                }
            }
            DeclarationKind::Enum => {
                let dec = lock.as_any().downcast_ref::<EnumDeclaration>();
                if let Some(dec) = dec {
                    if let Some(value) = dec.enum_for_name(name.as_str()) {
                        match value.value() {
                            Value::Int32(value) => {
                                rv.set_int32(value);
                                // Store as v8::Integer (Smi) so the cached value passes
                                // v8::Local::<v8::Int32>::try_from when used as a setter arg.
                                let ret: Local<v8::Value> = v8::Integer::new(scope, value).into();
                                store.set(scope, key.into(), ret);
                                return v8::Intercepted::kYes;
                            }
                            Value::Uint32(value) => {
                                rv.set_uint32(value);
                                let ret: Local<v8::Value> =
                                    v8::Integer::new_from_unsigned(scope, value).into();
                                store.set(scope, key.into(), ret);
                                return v8::Intercepted::kYes;
                            }
                            _ => {}
                        }
                    }

                    // Name isn't an enum member (e.g. `toString`, `valueOf`).
                    // Don't intercept: let V8 fall back to Object.prototype
                    // so coercion-style operations (console.log on the enum,
                    // template-string interpolation, etc.) work.
                    return v8::Intercepted::kNo;
                }
            }
            DeclarationKind::EnumMember => {}
            DeclarationKind::Struct => {}
            DeclarationKind::StructField => {}
            DeclarationKind::Property => {}
            DeclarationKind::Method => {
                let dec = lock.as_any().downcast_ref::<ClassDeclaration>();

                if let Some(dec) = dec {
                    for method in dec.methods() {
                        let mut name = method.overload_name();
                        if name.is_empty() {
                            name = method.name();
                        }
                        // let cached_item = store.get(scope, key.into());
                        // if let Some(cache) = cached_item {
                        //     if !cache.is_null_or_undefined() {
                        //         rv.set(cache);
                        //         return;
                        //     }
                        // }

                        // let full_name = format!("{}.{}", dec.full_name(), name.as_str());
                        // if let Some(dec) = MetadataReader::find_by_name(full_name.as_str()) {
                        //     let declaration = Arc::clone(&dec);
                        //     let lock = dec.read();
                        //
                        //     match lock.kind() {
                        //         DeclarationKind::Class => {
                        //             let ret: Local<v8::Value> = create_ns_ctor_object(lock.name(), declaration, scope).into();
                        //             rv.set(ret.into());
                        //         }
                        //         _ => {
                        //             let ret: Local<v8::Value> = create_ns_object(name.as_str(), declaration, scope).into();
                        //             rv.set(ret.into());
                        //         }
                        //     }
                        //
                        //
                        //     //  store.set(scope, key.into(), ret.into());
                        //     return;
                        // }
                        //
                        // rv.set_undefined();
                        // return;
                    }
                }
            }
            DeclarationKind::Parameter => {}
        }
        // Fell through every arm without setting rv: let V8 do its default
        // lookup (returns `undefined` for missing names, which is what JS
        // expects for unknown WinRT properties).
        return v8::Intercepted::kNo;
    }

    v8::Intercepted::kNo
}

#[cfg(feature = "classic")]
fn handle_indexed_property_setter(
    _scope: &mut v8::PinScope<'_, '_>,
    _index: u32,
    _value: v8::Local<v8::Value>,
    _args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    v8::Intercepted::kNo
}

#[cfg(feature = "classic")]
fn handle_indexed_property_getter(
    _scope: &mut v8::PinScope<'_, '_>,
    _index: u32,
    _args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    v8::Intercepted::kNo
}

#[cfg(feature = "classic")]
fn handle_ns_func(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut _retval: v8::ReturnValue,
) {
    // scope.throw_exception(v8::Exception::error(scope, v8::String::new("")))
}

// A JsDelegate wraps a `v8::Global<v8::Function>` inside a minimal COM object
// so it can be passed directly to WinRT event-add methods.  Every delegate
// type shares a single vtable; the per-instance GUID stored in the struct
// makes QueryInterface work correctly for each concrete type.

#[cfg(feature = "classic")]
#[repr(C)]
struct JsDelegateVtbl {
    query_interface:
        unsafe extern "system" fn(*mut JsDelegate, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut JsDelegate) -> u32,
    release: unsafe extern "system" fn(*mut JsDelegate) -> u32,
    // Declared with 4 usize params so the same slot works for delegates with
    // 0 to 4 pointer-sized arguments.  Callers pass only what they need; extras
    // land in dead registers and are never read (guarded by param_types.len()).
    invoke: unsafe extern "system" fn(*mut JsDelegate, usize, usize, usize, usize) -> HRESULT,
}

#[cfg(feature = "classic")]
pub(crate) static JS_DELEGATE_VTBL: JsDelegateVtbl = JsDelegateVtbl {
    query_interface: js_delegate_query_interface,
    add_ref: js_delegate_add_ref,
    release: js_delegate_release,
    invoke: js_delegate_invoke,
};

#[cfg(feature = "classic")]
pub(crate) struct JsDelegateData {
    pub(crate) js_func: v8::Global<v8::Function>,
    pub(crate) param_types: Vec<NativeType>,
}

#[cfg(feature = "classic")]
#[repr(C)]
pub(crate) struct JsDelegate {
    pub(crate) vtable: *const JsDelegateVtbl,
    pub(crate) ref_count: AtomicU32,
    pub(crate) guid: GUID,
    pub(crate) data: *mut JsDelegateData,
}

#[cfg(feature = "classic")]
unsafe impl Send for JsDelegate {}
#[cfg(feature = "classic")]
unsafe impl Sync for JsDelegate {}

#[cfg(feature = "classic")]
unsafe extern "system" fn js_delegate_query_interface(
    this: *mut JsDelegate,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = &*this;
    if *iid == IUnknown::IID || *iid == d.guid {
        *out = this as *mut c_void;
        js_delegate_add_ref(this);
        HRESULT(0)
    } else {
        *out = std::ptr::null_mut();
        HRESULT(0x80004002u32 as i32) // E_NOINTERFACE
    }
}

#[cfg(feature = "classic")]
unsafe extern "system" fn js_delegate_add_ref(this: *mut JsDelegate) -> u32 {
    (*this).ref_count.fetch_add(1, AtomicOrdering::Relaxed) + 1
}

#[cfg(feature = "classic")]
unsafe extern "system" fn js_delegate_release(this: *mut JsDelegate) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, AtomicOrdering::Release);
    if prev == 1 {
        std::sync::atomic::fence(AtomicOrdering::Acquire);
        let b = Box::from_raw(this);
        drop(Box::from_raw(b.data));
        // b (JsDelegate) dropped here
    }
    prev - 1
}

#[cfg(feature = "classic")]
unsafe extern "system" fn js_delegate_invoke(
    this: *mut JsDelegate,
    p0: usize,
    p1: usize,
    p2: usize,
    _p3: usize,
) -> HRESULT {
    // Wrap everything in catch_unwind so Rust panics cannot propagate through
    // the WinRT C++ caller stack (which would be UB and cause CLR FailFast).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        js_delegate_invoke_inner(this, p0, p1, p2)
    }));
    match result {
        Ok(hr) => hr,
        Err(_) => HRESULT(0x80004005u32 as i32), // E_FAIL on panic
    }
}

#[cfg(feature = "classic")]
fn js_delegate_invoke_inner(this: *mut JsDelegate, p0: usize, p1: usize, p2: usize) -> HRESULT {
    if this.is_null() {
        return HRESULT(0x80004005u32 as i32);
    }
    let data = unsafe {
        let data_ptr = (*this).data;
        if data_ptr.is_null() {
            return HRESULT(0x80004005u32 as i32);
        }
        &*data_ptr
    };

    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return HRESULT(0x80004005u32 as i32);
    }

    // Re-entrancy guard (see DELEGATE_DEPTH). Mutating XAML inside a delegate (e.g. setting
    // Content in ContainerContentChanging) can synchronously re-fire another delegate while
    // this V8 scope is still active; pushing a fresh root HandleScope from the raw isolate
    // then corrupts the scope stack ("HandleScope and Context do not belong to the same
    // Isolate"). On re-entry we adopt the already-active scope via CallbackScope instead.
    let reentrant = DELEGATE_DEPTH.with(|c| {
        let d = c.get();
        c.set(d + 1);
        d > 0
    });
    struct DepthGuard;
    impl Drop for DepthGuard {
        fn drop(&mut self) {
            DELEGATE_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
    let _depth_guard = DepthGuard;

    if reentrant {
        let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
        // CallbackScope adopts the currently-entered handle scope (no new root scope).
        let mut cb = unsafe { v8::CallbackScope::new(isolate) };
        let mut base = {
            let pinned = unsafe { std::pin::Pin::new_unchecked(&mut cb) };
            pinned.init()
        };
        let base = &mut base;
        let ctx_global = match base.get_slot::<v8::Global<v8::Context>>() {
            Some(g) => g.clone(),
            None => return HRESULT(0x80004005u32 as i32),
        };
        let context = v8::Local::new(base, &ctx_global);
        let scope = &mut v8::ContextScope::new(base, context);
        js_delegate_run(data, scope, p0, p1, p2)
    } else {
        let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
        v8::scope!(base, isolate);
        let ctx_global = match base.get_slot::<v8::Global<v8::Context>>() {
            Some(g) => g.clone(),
            None => return HRESULT(0x80004005u32 as i32),
        };
        let context = v8::Local::new(base, &ctx_global);
        let scope = &mut v8::ContextScope::new(base, context);
        js_delegate_run(data, scope, p0, p1, p2)
    }
}

/// Builds the JS argument list and invokes the delegate function within an already
/// context-entered scope. Shared by both the top-level and re-entrant paths above.
#[cfg(feature = "classic")]
fn js_delegate_run(
    data: &JsDelegateData,
    scope: &mut v8::PinScope<'_, '_>,
    p0: usize,
    p1: usize,
    p2: usize,
) -> HRESULT {
    // TryCatch so JS exceptions don't escape into WinRT C++ frames.
    v8::tc_scope!(tc, scope);

    let func = v8::Local::new(tc, &data.js_func);
    let recv = v8::undefined(tc);

    let params_raw = [p0, p1, p2];
    let n = data.param_types.len().min(3);
    let mut js_args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(n);

    for i in 0..n {
        let raw = params_raw[i] as *mut c_void;
        let val: v8::Local<v8::Value> = match data.param_types[i] {
            NativeType::Pointer => {
                if raw.is_null() {
                    v8::null(tc).into()
                } else {
                    // Delegate [in] parameters are borrowed COM pointers (the caller
                    // owns the ref for the duration of Invoke). Clone (AddRef) before
                    // wrapping so the proxy can outlive this stack frame.
                    let owned: IUnknown = unsafe {
                        let borrowed = std::mem::ManuallyDrop::new(IUnknown::from_raw(raw));
                        (*borrowed).clone()
                    };
                    // Try to resolve the concrete WinRT type so the JS callback
                    // receives a fully typed proxy (with property/method access)
                    // rather than an opaque plain object.
                    let proxy = (|| -> Option<v8::Local<v8::Value>> {
                        if let Some(key) = com_identity(&owned) {
                            let hit = INSTANCE_CACHE
                                .with(|cache| cache.borrow().get(&key).and_then(|w| w.to_local(tc)));
                            if let Some(local) = hit {
                                return Some(local.into());
                            }
                        }
                        let inspectable = owned.cast::<IInspectable>().ok()?;
                        let class_name = inspectable.GetRuntimeClassName().ok()?;
                        let name_str = class_name.to_string();
                        let decl = MetadataReader::find_by_name(&name_str)?;
                        Some(
                            create_ns_ctor_instance_object(
                                &name_str,
                                None,
                                None,
                                decl,
                                Some(owned.clone()),
                                tc,
                            )
                            .into(),
                        )
                    })();
                    proxy.unwrap_or_else(|| v8::External::new(tc, raw).into())
                }
            }
            NativeType::Bool => v8::Boolean::new(tc, (raw as u8) != 0).into(),
            NativeType::U8 => v8::Integer::new_from_unsigned(tc, raw as u8 as u32).into(),
            NativeType::I8 => v8::Integer::new(tc, raw as i8 as i32).into(),
            NativeType::U16 => v8::Integer::new_from_unsigned(tc, raw as u16 as u32).into(),
            NativeType::I16 => v8::Integer::new(tc, raw as i16 as i32).into(),
            NativeType::U32 => v8::Integer::new_from_unsigned(tc, raw as u32).into(),
            NativeType::I32 => v8::Integer::new(tc, raw as i32).into(),
            NativeType::U64 => v8::Number::new(tc, raw as u64 as f64).into(),
            NativeType::I64 => v8::Number::new(tc, raw as i64 as f64).into(),
            _ => v8::undefined(tc).into(),
        };
        js_args.push(val);
    }

    let _ = func.call(tc, recv.into(), &js_args);
    if tc.has_caught() {
        if let Some(ex) = tc.exception() {
            let msg = ex.to_rust_string_lossy(tc);
            store_last_js_error(msg);
        }
        tc.reset();
    }
    // Delegates can fire from inside XAML's render walk; never drain Promise
    // continuations there (fail-fast 0xC000027B): defer when possible.
    if !defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }
    HRESULT(0)
}

/// Resolves the NativeType for a single delegate `Invoke` parameter signature.
///
/// Unlike `ffi_native_type_from_signature`, this also resolves named WinRT enum types
/// (e.g. `Windows.Foundation.AsyncStatus`) to `NativeType::U32` rather than Pointer,
/// so that `js_delegate_invoke_inner` receives them as plain integers instead of
/// trying to dereference them as COM vtable pointers.
fn ffi_type_for_delegate_param(sig: &str) -> NativeType {
    let base = crate::helpers::ffi_native_type_from_signature(sig);
    if matches!(base, NativeType::Pointer) && sig.contains('.') {
        let stripped = crate::helpers::strip_generic_suffix(sig);
        if let Some(decl) = MetadataReader::find_by_name(stripped) {
            if matches!(decl.read().kind(), DeclarationKind::Enum) {
                return NativeType::U32;
            }
        }
    }
    base
}

/// Extract the GUID and input-parameter NativeTypes from a delegate declaration.
pub(crate) fn js_delegate_params_from_declaration(
    lock: &dyn Declaration,
    kind: DeclarationKind,
) -> Option<(GUID, Vec<NativeType>)> {
    let build = |method: &MethodDeclaration, guid: GUID| -> (GUID, Vec<NativeType>) {
        let params = method
            .parameters()
            .iter()
            .filter(|p| !p.is_out())
            .filter_map(|p| {
                let sig = Signature::to_string(p.metadata()?, &p.type_());
                Some(ffi_type_for_delegate_param(&sig))
            })
            .collect();
        (guid, params)
    };

    Some(match kind {
        DeclarationKind::Delegate => {
            let d = lock.as_any().downcast_ref::<DelegateDeclaration>()?;
            build(d.invoke_method(), d.id())
        }
        DeclarationKind::GenericDelegate => {
            let d = lock.as_any().downcast_ref::<GenericDelegateDeclaration>()?;
            build(d.invoke_method(), d.id())
        }
        DeclarationKind::GenericDelegateInstance => {
            let d = lock
                .as_any()
                .downcast_ref::<GenericDelegateInstanceDeclaration>()?;
            build(d.invoke_method(), d.id())
        }
        _ => return None,
    })
}

/// Resolves a delegate type's GUID and input-parameter NativeTypes from an IID-name string.
///
/// `iid_name` must be the full IID-form name, e.g.:
///   - `"RoutedEventHandler"` (non-generic)
///   - `"EventHandler\`1<SuspendingEventArgs>"` (closed generic instance)
///
/// Used by `delegate_info_from_add_method` and by property/method setters that
/// auto-wrap raw JS functions as `JsDelegate` COM objects.
pub(crate) fn delegate_info_from_type_sig(iid_name: &str) -> Option<(GUID, Vec<NativeType>)> {
    if let Some(open_name) = iid_name.split_once('<').map(|(prefix, _)| prefix) {
        // Generic delegate instance: compute the parameterized GUID.
        let guid = GenericInstanceIdBuilder::generate_id_from_name(iid_name);

        // Derive param_types from the open-generic delegate's Invoke signature.
        let open_decl = MetadataReader::find_by_name(open_name)?;
        let open_lock = open_decl.read();
        let open_delegate = open_lock
            .as_any()
            .downcast_ref::<GenericDelegateDeclaration>()?;
        let invoke = open_delegate.invoke_method();
        let param_types = invoke
            .parameters()
            .iter()
            .filter(|p| !p.is_out())
            .filter_map(|p| {
                let sig_str = Signature::to_string(p.metadata()?, &p.type_());
                Some(ffi_type_for_delegate_param(&sig_str))
            })
            .collect();
        Some((guid, param_types))
    } else {
        // Non-generic delegate: look up by the exact type name.
        let decl = MetadataReader::find_by_name(iid_name)?;
        let lock = decl.read();
        let kind = lock.kind();
        js_delegate_params_from_declaration(&*lock, kind)
    }
}

/// Derives the delegate (GUID, param_types) expected by a WinRT event's `add_*`
/// method from the method's first parameter type.
pub(crate) fn delegate_info_from_add_method(
    add_method: &MethodDeclaration,
) -> Option<(GUID, Vec<NativeType>)> {
    let params = add_method.parameters();
    let param = params.first()?;
    let metadata = param.metadata()?;
    let sig = param.type_();

    // `to_iid_string` preserves the backtick+arity required by GenericInstanceIdBuilder
    // and by MetadataReader for open-generic lookup (e.g. "EventHandler`1<...>").
    let iid_name = Signature::to_iid_string(metadata, &sig);
    if iid_name.is_empty() {
        return None;
    }
    delegate_info_from_type_sig(&iid_name)
}

/// JS-callable handler for `__nsAsDelegate(typeName, fn)`.
///
/// Looks up `typeName` in the WinRT metadata, derives the delegate's GUID and
/// input-parameter NativeTypes via `js_delegate_params_from_declaration`, then
/// allocates a `JsDelegate` COM object and returns `{ handle: External }`:
/// the same shape that WinRT event-add methods expect.
#[cfg(feature = "classic")]
pub(crate) fn handle_as_delegate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        throw_js_error(scope, "__nsAsDelegate(typeName, fn): expected 2 arguments");
        return;
    }
    let Some(name_v8) = args.get(0).to_string(scope) else {
        throw_js_error(scope, "__nsAsDelegate: first argument must be a string");
        return;
    };
    let type_name = name_v8.to_rust_string_lossy(scope);

    let Ok(func) = v8::Local::<v8::Function>::try_from(args.get(1)) else {
        throw_js_error(scope, "__nsAsDelegate: second argument must be a function");
        return;
    };

    let Some((guid, param_types)) = delegate_info_from_type_sig(&type_name) else {
        throw_js_error(
            scope,
            &format!("{} is not a known WinRT delegate type", type_name),
        );
        return;
    };

    let data = Box::new(JsDelegateData {
        js_func: v8::Global::new(scope, func),
        param_types,
    });
    let delegate = Box::new(JsDelegate {
        vtable: &JS_DELEGATE_VTBL as *const _,
        ref_count: AtomicU32::new(1),
        guid,
        data: Box::into_raw(data),
    });
    let raw = Box::into_raw(delegate) as *mut c_void;

    let result_obj = v8::Object::new(scope);
    if let Some(key) = v8::String::new(scope, "handle") {
        result_obj.set(scope, key.into(), v8::External::new(scope, raw).into());
    }
    retval.set(result_obj.into());
}

/// __nsMakeItemsSource(count) → { handle } wrapping a native IVector<IInspectable>
/// of `count` boxed Int32 indices, assignable to a XAML ItemsSource so WinUI virtualizes.
#[cfg(feature = "classic")]
pub(crate) fn handle_make_items_source(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let count = if args.length() >= 1 {
        args.get(0).integer_value(scope).unwrap_or(0).max(0) as u32
    } else {
        0
    };

    match crate::js_observable_vector::make_index_vector(count) {
        Ok(inspectable) => {
            let raw = windows_core::Interface::into_raw(inspectable) as *mut c_void;
            let result_obj = v8::Object::new(scope);
            if let Some(key) = v8::String::new(scope, "handle") {
                result_obj.set(scope, key.into(), v8::External::new(scope, raw).into());
            }
            retval.set(result_obj.into());
        }
        Err(_e) => {
            throw_js_error(
                scope,
                "NSWinRT.makeItemsSource: failed to build native vector",
            );
        }
    }
}

/// __nsExtendItemsSource({ handle }, newCount): grow an existing items source (from
/// __nsMakeItemsSource) in place to `newCount` items, firing VectorChanged so WinUI adds only the new
/// rows (preserves scroll position + already-realized cells). Used by the ListView for infinite-scroll
/// append instead of replacing the whole ItemsSource.
#[cfg(feature = "classic")]
pub(crate) fn handle_extend_items_source(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let Some(obj) = args.get(0).to_object(scope) else {
        return;
    };
    let Some(key) = v8::String::new(scope, "handle") else {
        return;
    };
    let Some(handle_val) = obj.get(scope, key.into()) else {
        return;
    };
    if !handle_val.is_external() {
        return;
    }
    let ext = unsafe { handle_val.cast::<v8::External>() };
    let raw = ext.value() as *mut c_void;
    if raw.is_null() {
        return;
    }
    let new_count = if args.length() >= 2 {
        args.get(1).integer_value(scope).unwrap_or(0).max(0) as u32
    } else {
        0
    };
    // Borrow the IInspectable the External owns without releasing it (the External keeps ownership).
    let inspectable =
        std::mem::ManuallyDrop::new(unsafe { windows_core::IInspectable::from_raw(raw) });
    let _ = crate::js_observable_vector::extend_index_vector(&inspectable, new_count);
}

/// Borrow (without releasing) the IInspectable that __nsMakeItemsSource's `{ handle }` External owns.
/// Returns None when the handle arg is missing/non-external/null. The External keeps ownership, so the
/// returned value is wrapped in ManuallyDrop and must not be dropped by the caller.
#[cfg(feature = "classic")]
fn items_source_handle(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
) -> Option<std::mem::ManuallyDrop<windows_core::IInspectable>> {
    let obj = args.get(0).to_object(scope)?;
    let key = v8::String::new(scope, "handle")?;
    let handle_val = obj.get(scope, key.into())?;
    if !handle_val.is_external() {
        return None;
    }
    let ext = unsafe { handle_val.cast::<v8::External>() };
    let raw = ext.value() as *mut c_void;
    if raw.is_null() {
        return None;
    }
    Some(std::mem::ManuallyDrop::new(unsafe {
        windows_core::IInspectable::from_raw(raw)
    }))
}

/// __nsInsertItemsSource({ handle }, index, count): insert `count` rows at `index` into an items
/// source (from __nsMakeItemsSource), firing VectorChanged(ItemInserted) so WinUI adds only the new
/// rows. Used by the ListView for granular ObservableArray add/splice without a full rebuild.
#[cfg(feature = "classic")]
pub(crate) fn handle_insert_items_source(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let Some(inspectable) = items_source_handle(scope, &args) else {
        return;
    };
    let index = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u32;
    let count = args.get(2).integer_value(scope).unwrap_or(0).max(0) as u32;
    let _ = crate::js_observable_vector::insert_index_vector(&inspectable, index, count);
}

/// __nsRemoveItemsSource({ handle }, index, count): remove `count` rows at `index`, firing
/// VectorChanged(ItemRemoved) so WinUI drops only those rows. Granular ObservableArray delete/splice.
#[cfg(feature = "classic")]
pub(crate) fn handle_remove_items_source(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let Some(inspectable) = items_source_handle(scope, &args) else {
        return;
    };
    let index = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u32;
    let count = args.get(2).integer_value(scope).unwrap_or(0).max(0) as u32;
    let _ = crate::js_observable_vector::remove_index_vector(&inspectable, index, count);
}

/// __nsResetItemsSource({ handle }, newCount): rebuild the items source to `newCount` rows and fire a
/// SINGLE VectorChanged(Reset). WinRT has no range event, so this is the one-event way to apply a bulk
/// change (wholesale replace / large splice / filter); WinUI re-realizes only the visible containers.
#[cfg(feature = "classic")]
pub(crate) fn handle_reset_items_source(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let Some(inspectable) = items_source_handle(scope, &args) else {
        return;
    };
    let new_count = if args.length() >= 2 {
        args.get(1).integer_value(scope).unwrap_or(0).max(0) as u32
    } else {
        0
    };
    let _ = crate::js_observable_vector::reset_index_vector(&inspectable, new_count);
}

/// __nsUpdateItemsSource({ handle }, index, count): fire VectorChanged(ItemChanged) for `count` rows
/// at `index` (count unchanged) so WinUI re-realizes just those containers. Granular setItem/update.
#[cfg(feature = "classic")]
pub(crate) fn handle_update_items_source(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let Some(inspectable) = items_source_handle(scope, &args) else {
        return;
    };
    let index = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u32;
    let count = args.get(2).integer_value(scope).unwrap_or(0).max(0) as u32;
    let _ = crate::js_observable_vector::update_index_vector(&inspectable, index, count);
}

#[cfg(feature = "classic")]
impl Runtime {
    #[cfg(feature = "classic")]
    pub fn new(app_root: &str) -> Self {
        // Look for a sealed app.nsbundle next to app_root before anything else touches the
        // filesystem for JS source: every read/resolve point below consults the decrypted
        // in-memory table first and falls back to disk when none was found.
        crate::source_protect::init_from_app_root(app_root);

        INIT.call_once(|| {
            // --expose-gc makes gc() available as a global JS function so callers
            // can trigger a full GC sweep (useful for debugging and test harnesses).
            v8::V8::set_flags_from_string("--expose-gc");
            let platform = v8::new_default_platform(0, false).make_shared();
            v8::V8::initialize_platform(platform);
            v8::V8::initialize();
        });

        // A prior Runtime on this thread may have set the teardown flag; clear it
        // so this runtime's backing-store deleters release COM refs normally.
        crate::global_fns::set_com_teardown(false);

        let winrt_initialized = match unsafe { RoInitialize(RO_INIT_SINGLETHREADED) } {
            Ok(_) => true,
            // Any failure (including RPC_E_CHANGED_MODE=0x80010106 and the ASTA apartment
            // model used by UWP/XAML hosts) means WinRT was already initialized externally.
            // Skip RoUninitialize on drop rather than panicking.
            Err(_) => false,
        };

        // RoGetMetaDataFile only resolves system/app-package metadata; third-party
        // .winmd files (e.g. WebView2) must be registered manually or scanned here.
        let scan_dirs = [
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf())),
            Some(std::path::PathBuf::from(app_root)),
        ];
        for dir in scan_dirs.into_iter().flatten() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_winmd = path
                    .extension()
                    .map_or(false, |ext| ext.eq_ignore_ascii_case("winmd"));
                if is_winmd {
                    if let Some(path_str) = path.to_str() {
                        if let Err(err) =
                            metadata::meta_data_reader::MetadataReader::register_winmd_file(
                                path_str,
                            )
                        {
                            eprintln!("[NativeScript] winmd sideload skipped: {}", err);
                        }
                    }
                }
            }
        }

        // Create the message-only HWND for native UI-thread dispatch. Must run
        // on the UI thread (here, in Runtime::new) before any cross-thread posts.
        crate::ui_dispatcher::init_ui_dispatcher();

        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);
        isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 100);

        // Microtasks must drain only at the runtime's explicit checkpoint sites
        // (pump_dispatcher and the native→JS callback exits). V8's default kAuto
        // policy also drains whenever a Function::call returns at embedder call
        // depth zero: which includes delegate invokes fired from inside XAML's
        // render walk (e.g. a JS handler on CompositionTarget.Rendering), where
        // running Promise continuations re-enters the XAML core and fail-fasts
        // with 0xC000027B. Explicit policy lets `defer_microtask_drain` move
        // that drain to a DispatcherQueue work item outside the walk.
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

        // Dynamic `import()`: load + link + evaluate the requested graph, then settle the returned
        // promise with the module namespace. After the graph's own evaluation promise (TLA) settles,
        // and with the real exception on failure.
        isolate.set_host_initialize_import_meta_object_callback(esm_import_meta);
        isolate.set_host_import_module_dynamically_callback(
            |scope: &mut v8::PinScope<'_, '_>,
             _host_defined_options: v8::Local<v8::Data>,
             resource_name: v8::Local<v8::Value>,
             specifier: v8::Local<v8::String>,
             _import_attributes: v8::Local<v8::FixedArray>|
             -> Option<v8::Local<v8::Promise>> {
                let resolver = v8::PromiseResolver::new(scope)?;
                let spec = specifier.to_rust_string_lossy(scope);
                let referrer_path = value_to_string(scope, resource_name);
                let resolved = resolve_esm_path(&spec, referrer_path.as_deref());
                // Volatile URLs (configureLoader) are re-fetched on every import.
                if crate::esm_http::is_http(&resolved) && crate::esm_http::is_volatile(&resolved) {
                    evict_module_key(&resolved);
                }

                match esm_load_and_evaluate(scope, &spec, referrer_path.as_deref(), &resolved, None) {
                    Err(exc) => {
                        let exc = v8::Local::new(scope, &exc);
                        resolver.reject(scope, exc);
                    }
                    Ok((module, result)) => {
                        let module = v8::Local::new(scope, &module);
                        let result = v8::Local::new(scope, &result);
                        let ns = module.get_module_namespace();
                        match v8::Local::<v8::Promise>::try_from(result) {
                            Ok(p) if p.state() == v8::PromiseState::Rejected => {
                                let exc = p.result(scope);
                                resolver.reject(scope, exc);
                            }
                            Ok(p) if p.state() == v8::PromiseState::Pending => {
                                let to_ns = v8::Function::builder(
                                    |_scope: &mut v8::PinScope<'_, '_>,
                                     args: v8::FunctionCallbackArguments,
                                     mut rv: v8::ReturnValue| {
                                        rv.set(args.data());
                                    },
                                )
                                .data(ns)
                                .build(scope)?;
                                let chained = p.then(scope, to_ns)?;
                                resolver.resolve(scope, chained.into());
                            }
                            _ => {
                                resolver.resolve(scope, ns);
                            }
                        }
                    }
                }
                Some(resolver.get_promise(scope))
            },
        );

        let global_context;
        {
            v8::scope!(scope, &mut isolate);

            let mut global_template = v8::ObjectTemplate::new(scope);

            globals::performance::init_performance(scope, &mut global_template);
            globals::time::init_time(scope, &mut global_template);

            let context = v8::Context::new(
                scope,
                v8::ContextOptions {
                    global_template: Some(global_template),
                    ..Default::default()
                },
            );
            {
                let scope = &mut v8::ContextScope::new(scope, context);
                preload_sbg_manifest();
                init_global(scope, context);
                globals::console::init_console(scope, context);
                globals::performance::install_fast_now(scope, context);
                init_meta(scope, context);
                crate::global_fns::init_async_helpers(scope, app_root);
                crate::win32::prewarm_known_fns();
                global_context = v8::Global::new(scope, context);
            }
        }

        // Store the global context in the isolate slot so the JS delegate
        // Invoke trampoline can enter V8 without an existing scope on the stack.
        {
            let ctx_clone = global_context.clone();
            isolate.set_slot(ctx_clone);
        }
        isolate.set_slot(crate::ns_proxy::InstanceTemplateCache::new());
        // Don't capture `&mut *isolate as *mut v8::Isolate` here: the wrapper's
        // address moves with the `OwnedIsolate` into `Self`, dangling our pointer.
        // The caller must invoke `register_delegate_isolate_ptr` after boxing.

        Self {
            isolate,
            global_context,
            app_root: app_root.to_string(),
            winrt_initialized,
        }
    }

    /// Must be called after the Runtime is at a stable address (e.g. boxed),
    /// so the captured isolate pointer doesn't dangle.
    #[cfg(feature = "classic")]
    pub fn register_delegate_isolate_ptr(&mut self) {
        let raw_isolate: *mut v8::Isolate = &mut *self.isolate as *mut v8::Isolate;
        DELEGATE_ISOLATE_PTR.with(|cell| cell.set(raw_isolate));
    }

    /// Provides mutable access to the underlying V8 isolate.
    /// Used by the devtools integration to attach a `V8Inspector`.
    #[cfg(feature = "classic")]
    pub fn isolate_mut(&mut self) -> &mut v8::Isolate {
        &mut self.isolate
    }

    /// Returns the persistent context handle.
    /// Used by the devtools integration to register the context with the inspector.
    #[cfg(feature = "classic")]
    pub fn global_context(&self) -> &v8::Global<v8::Context> {
        &self.global_context
    }

    #[cfg(feature = "classic")]
    pub fn run_module(&mut self, script: &str, filename: &str) {
        v8::scope!(scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.global_context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let resolved_path = {
            let p = locate_entry_module(filename, &self.app_root);
            p.canonicalize().unwrap_or(p).to_string_lossy().into_owned()
        };
        if let Some(dir) = Path::new(&resolved_path).parent() {
            ESM_APP_DIR.with(|d| *d.borrow_mut() = Some(dir.to_path_buf()));
            crate::esm_http::init_security_from_app_dir(dir);
        }

        macro_rules! check_exception {
            ($tc:ident) => {
                if $tc.has_caught() {
                    let mut error_report = String::new();
                    if let Some(msg) = $tc.message() {
                        let text = msg.get($tc).to_rust_string_lossy($tc);
                        let line = msg.get_line_number($tc).unwrap_or(0);
                        let file_name = msg
                            .get_script_resource_name($tc)
                            .map(|v| v.to_rust_string_lossy($tc))
                            .unwrap_or_else(|| "<unknown>".to_string());
                        error_report.push_str(&format!("{} ({}:{})\n", text, file_name, line));
                        if let Some(stack) = msg.get_stack_trace($tc) {
                            for i in 0..stack.get_frame_count() {
                                if let Some(frame) = stack.get_frame($tc, i) {
                                    let fn_name = frame
                                        .get_function_name($tc)
                                        .map(|s| s.to_rust_string_lossy($tc))
                                        .unwrap_or_else(|| "<anonymous>".to_string());
                                    let file = frame
                                        .get_script_name($tc)
                                        .map(|s| s.to_rust_string_lossy($tc))
                                        .unwrap_or_else(|| "<unknown>".to_string());
                                    let line_str = format!(
                                        "    at {} ({}:{}:{})\n",
                                        fn_name,
                                        file,
                                        frame.get_line_number(),
                                        frame.get_column()
                                    );
                                    error_report.push_str(&line_str);
                                }
                            }
                        }
                    } else if let Some(exc) = $tc.exception() {
                        let text = exc.to_rust_string_lossy($tc);
                        error_report.push_str(&text);
                    }
                    if !error_report.is_empty() {
                        crate::store_last_js_error(error_report);
                    }
                    return;
                }
            };
        }

        compile_module_graph(tc, script, &resolved_path);
        check_exception!(tc);

        let root_global = ESM_MODULE_REGISTRY.with(|r| r.borrow().get(&resolved_path).cloned());
        let Some(root_global) = root_global else {
            crate::store_last_js_error("ESM: root module was not compiled".to_string());
            return;
        };
        let module = v8::Local::new(tc, &root_global);

        if module
            .instantiate_module(tc, resolve_module_callback)
            .is_none()
        {
            check_exception!(tc);
            return;
        }

        let Some(result) = module.evaluate(tc) else {
            check_exception!(tc);
            return;
        };

        check_exception!(tc);
        tc.perform_microtask_checkpoint();
        report_module_evaluation(tc, result);
        flush_module_code_caches(tc);
    }

    #[cfg(feature = "classic")]
    pub fn run_script(&mut self, script: &str, filename: &str) {
        // Delegate ESM bundles to the native V8 module loader.
        let is_esm = filename.ends_with(".mjs") || {
            let trimmed = script.trim_start();
            trimmed.starts_with("import ") || trimmed.starts_with("export ")
        };
        if is_esm {
            self.run_module(script, filename);
            return;
        }

        v8::scope!(scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.global_context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let Some(code) = v8::String::new(tc, script) else {
            return;
        };
        let origin = v8::String::new(tc, filename).map(|name| {
            v8::ScriptOrigin::new(
                tc,
                name.into(),
                0,
                0,
                false,
                -1,
                None,
                false,
                false,
                false,
                None,
            )
        });
        // Bytecode-cache large chunks (vendor.js/bundle.js) so subsequent cold starts skip the
        // parse+compile of multi-MB sources. Small scripts skip it (caching overhead > benefit). The
        // cache is keyed by content hash, so a livesync edit simply misses and recompiles.
        let cache_path = if script.len() >= 65536 {
            code_cache_path(filename, script)
        } else {
            None
        };
        let cached_bytes = cache_path.as_ref().and_then(|p| std::fs::read(p).ok());

        let mut consumed_from_cache = false;
        let compiled = if let Some(bytes) = cached_bytes {
            consumed_from_cache = true;
            let cached = v8::script_compiler::CachedData::new(&bytes);
            let mut source =
                v8::script_compiler::Source::new_with_cached_data(code, origin.as_ref(), cached);
            v8::script_compiler::compile(
                tc,
                &mut source,
                v8::script_compiler::CompileOptions::ConsumeCodeCache,
                v8::script_compiler::NoCacheReason::NoReason,
            )
        } else {
            let mut source = v8::script_compiler::Source::new(code, origin.as_ref());
            v8::script_compiler::compile(
                tc,
                &mut source,
                v8::script_compiler::CompileOptions::NoCompileOptions,
                v8::script_compiler::NoCacheReason::NoReason,
            )
        };

        if let Some(compiled) = compiled {
            // First compile of this content: produce + persist the bytecode for the next launch.
            if !consumed_from_cache {
                if let Some(path) = cache_path.as_ref() {
                    if let Some(cached) = compiled.get_unbound_script(tc).create_code_cache() {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(path, &**cached);
                    }
                }
            }
            compiled.run(tc);
        }

        // Log any uncaught JS exception. Do NOT call tc.rethrow() here: run_script is
        // invoked from FFI (runtime_runscript) where there is no outer V8 TryCatch.
        // Rethrowing into an empty V8 scope causes V8 to call its fatal-error handler
        // → abort() → System.ExecutionEngineException in the CLR.
        if tc.has_caught() {
            let mut error_report = String::new();
            if let Some(msg) = tc.message() {
                let text = msg.get(tc).to_rust_string_lossy(tc);
                let line = msg.get_line_number(tc).unwrap_or(0);
                let file_name = msg
                    .get_script_resource_name(tc)
                    .map(|v| v.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "<unknown>".to_string());
                error_report.push_str(&format!("{} ({}:{})\n", text, file_name, line));
                if let Some(stack) = msg.get_stack_trace(tc) {
                    for i in 0..stack.get_frame_count() {
                        if let Some(frame) = stack.get_frame(tc, i) {
                            let fn_name = frame
                                .get_function_name(tc)
                                .map(|s| s.to_rust_string_lossy(tc))
                                .unwrap_or_else(|| "<anonymous>".to_string());
                            let file = frame
                                .get_script_name(tc)
                                .map(|s| s.to_rust_string_lossy(tc))
                                .unwrap_or_else(|| "<unknown>".to_string());
                            let line_str = format!(
                                "    at {} ({}:{}:{})\n",
                                fn_name,
                                file,
                                frame.get_line_number(),
                                frame.get_column()
                            );
                            error_report.push_str(&line_str);
                        }
                    }
                }
            } else if let Some(exc) = tc.exception() {
                let text = exc.to_rust_string_lossy(tc);
                error_report.push_str(&text);
            }
            if !error_report.is_empty() {
                store_last_js_error(error_report);
            }
            return;
        }

        // Drain any synchronous Promise microtasks that the script may have queued.
        tc.perform_microtask_checkpoint();
    }

    #[cfg(feature = "classic")]
    pub fn eval_script_to_string(&mut self, script: &str) -> Option<String> {
        v8::scope!(scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.global_context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let code = v8::String::new(tc, script)?;
        let compiled = v8::Script::compile(tc, code, None)?;
        let value = compiled.run(tc)?;

        if tc.has_caught() {
            if let Some(msg) = tc.message() {
                let text = msg.get(tc).to_rust_string_lossy(tc);
                let line = msg.get_line_number(tc).unwrap_or(0);
                eprintln!(
                    "[NativeScript] Worker eval exception at line {}: {}",
                    line, text
                );
            }
            tc.rethrow();
            return None;
        }

        tc.perform_microtask_checkpoint();

        value.to_string(tc).map(|s| s.to_rust_string_lossy(tc))
    }

    /// Invokes the JS global `__nsOnAppEvent(kind, message)` if defined. Called by the host
    /// (via `runtime_notify_app_event`) to forward lifecycle events; runs on the V8/UI thread.
    #[cfg(feature = "classic")]
    pub fn notify_app_event(&mut self, kind: i32, message: Option<&str>) {
        v8::scope!(scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.global_context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let global = context.global(tc);
        let Some(key) = v8::String::new(tc, "__nsOnAppEvent") else {
            return;
        };
        let Some(value) = global.get(tc, key.into()) else {
            return;
        };
        let Ok(func) = v8::Local::<v8::Function>::try_from(value) else {
            return; // handler not registered (yet): nothing to do
        };

        let recv: v8::Local<v8::Value> = global.into();

        // kind is an i32 → wrap as a V8 Integer; message (Option<&str>) becomes the 2nd arg if present.
        let kind_val: v8::Local<v8::Value> = v8::Integer::new(tc, kind).into();
        if let Some(arg) = message.and_then(|m| v8::String::new(tc, m)) {
            func.call(tc, recv, &[kind_val, arg.into()]);
        } else {
            func.call(tc, recv, &[kind_val]);
        };

        // Swallow handler errors: no outer TryCatch here (see run_script).
        if tc.has_caught() {
            let _ = tc.exception();
        } else if !defer_microtask_drain() {
            tc.perform_microtask_checkpoint();
        }
    }

    pub fn dispose(&self) {}
}

#[cfg(feature = "classic")]
impl Drop for Runtime {
    fn drop(&mut self) {
        // Every thread-local that holds isolate-tied state (v8::Global, v8::Weak,
        // raw isolate pointers) must be cleared here, while `self.isolate` is
        // still alive. Anything left behind dangles into freed isolate memory
        // and crashes the next Runtime created on this thread.
        INSTANCE_CACHE.with(|cache| cache.borrow_mut().clear());
        EVENT_REGISTRY.with(|m| m.borrow_mut().clear());
        ESM_MODULE_REGISTRY.with(|m| m.borrow_mut().clear());
        ESM_HASH_TO_PATH.with(|m| m.borrow_mut().clear());
        PENDING_MODULE_CODE_CACHES.with(|q| q.borrow_mut().clear());
        ESM_HTTP_URLS.with(|m| m.borrow_mut().clear());
        ESM_LOAD_ERRORS.with(|m| m.borrow_mut().clear());
        crate::esm_http::clear_thread_vocabulary();
        DOTNET_JS_CALLBACKS.with(|m| m.borrow_mut().clear());
        DOTNET_ONESHOT_JS_CALLBACKS.with(|m| m.borrow_mut().clear());
        crate::timers::clear_thread_tasks();
        crate::websocket::clear_thread_sockets();
        crate::globals::url::clear_thread_url_ctor();
        crate::inspector::clear_thread_dispatchers();
        crate::global_fns::clear_thread_dispatchers();
        ISOLATE.with(|cell| *cell.borrow_mut() = None);
        DELEGATE_ISOLATE_PTR.with(|cell| cell.set(std::ptr::null_mut()));
        // A queued drain work item may never run once the host's dispatcher
        // stops; reset so a future Runtime on this thread can schedule drains.
        MICROTASK_DRAIN_QUEUED.with(|cell| cell.set(false));
        if self.winrt_initialized {
            // Signal backing-store deleters (e.g. zero-copy IBuffer ArrayBuffers)
            // to skip COM Release once the isolate is disposed below: that runs
            // after RoUninitialize, so Release would hit a dead apartment.
            crate::global_fns::set_com_teardown(true);
            unsafe { RoUninitialize() };
        }
    }
}

#[cfg(feature = "classic")]
struct WorkerValueSerializer;

#[cfg(feature = "classic")]
impl v8::ValueSerializerImpl for WorkerValueSerializer {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        message: v8::Local<'s, v8::String>,
    ) {
        let error = v8::Exception::error(scope, message);
        scope.throw_exception(error);
    }
}

#[cfg(feature = "classic")]
struct WorkerValueDeserializer;

#[cfg(feature = "classic")]
impl v8::ValueDeserializerImpl for WorkerValueDeserializer {}

#[cfg(feature = "classic")]
impl Runtime {
    /// Serialize a single V8 value to structured-clone bytes using V8's own
    /// `ValueSerializer`.  Returns `None` if the value is not cloneable (e.g.
    /// a function or a circular object); in that case an exception has already
    /// been thrown into `scope`.
    #[cfg(feature = "classic")]
    pub fn serialize_value<'s, 'v>(
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'v, v8::Value>,
    ) -> Option<Vec<u8>> {
        use v8::ValueSerializerHelper;
        let context = scope.get_current_context();
        let ser = v8::ValueSerializer::new(scope, Box::new(WorkerValueSerializer));
        ser.write_header();
        if ser.write_value(context, value).unwrap_or(false) {
            Some(ser.release())
        } else {
            None
        }
    }

    /// Deserialize structured-clone bytes produced by `serialize_value` back
    /// into a V8 value in the current context.
    #[cfg(feature = "classic")]
    pub fn deserialize_value<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        bytes: &[u8],
    ) -> Option<v8::Local<'s, v8::Value>> {
        use v8::ValueDeserializerHelper;
        let context = scope.get_current_context();
        let de = v8::ValueDeserializer::new(scope, Box::new(WorkerValueDeserializer), bytes);
        if !de.read_header(context).unwrap_or(false) {
            return None;
        }
        de.read_value(context)
    }

    /// Drain `globalThis.__nsWorkerOutbox`, serialize every item with V8's
    /// structured-clone algorithm, and return the resulting byte blobs.
    #[cfg(feature = "classic")]
    pub fn drain_outbox_bytes(&mut self) -> Vec<Result<Vec<u8>, String>> {
        v8::scope!(scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.global_context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let script_src =
            "(function(){var o=globalThis.__nsWorkerOutbox||[];return o.splice(0);})()";
        let Some(src) = v8::String::new(tc, script_src) else {
            return Vec::new();
        };
        let Some(script) = v8::Script::compile(tc, src, None) else {
            return Vec::new();
        };
        let Some(result) = script.run(tc) else {
            return Vec::new();
        };
        let Ok(array) = v8::Local::<v8::Array>::try_from(result) else {
            return Vec::new();
        };

        let len = array.length();
        let mut out = Vec::with_capacity(len as usize);

        for i in 0..len {
            let Some(item) = array.get_index(tc, i) else {
                continue;
            };
            match Self::serialize_value(tc, item) {
                Some(bytes) => out.push(Ok(bytes)),
                None => {
                    let msg = if tc.has_caught() {
                        let s = tc
                            .message()
                            .and_then(|m| Some(m.get(tc).to_rust_string_lossy(tc)))
                            .unwrap_or_else(|| "DataCloneError".to_string());
                        tc.reset();
                        s
                    } else {
                        "DataCloneError: value could not be cloned".to_string()
                    };
                    out.push(Err(msg));
                }
            }
        }

        out
    }

    /// Deserialize `payload_bytes` and deliver them to the worker's
    /// `__nsDispatchToWorker` JS function.
    #[cfg(feature = "classic")]
    pub fn dispatch_to_worker(&mut self, payload_bytes: &[u8]) {
        v8::scope!(scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.global_context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let data_value = match Self::deserialize_value(tc, payload_bytes) {
            Some(v) => v,
            None => {
                eprintln!("[NativeScript] Worker dispatch: failed to deserialize message");
                return;
            }
        };

        let ctx = tc.get_current_context();
        let global = ctx.global(tc);
        let Some(fn_name) = v8::String::new(tc, "__nsDispatchToWorker") else {
            return;
        };
        let Some(fn_val) = global.get(tc, fn_name.into()) else {
            return;
        };
        let Ok(dispatch_fn) = v8::Local::<v8::Function>::try_from(fn_val) else {
            return;
        };
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        dispatch_fn.call(tc, recv, &[data_value]);

        tc.perform_microtask_checkpoint();
    }
}

#[cfg(all(test, feature = "classic"))]
mod color_test;

#[cfg(all(test, feature = "classic"))]
mod error_handling_test;

#[cfg(all(test, feature = "classic"))]
mod module_load_test;

#[cfg(all(test, feature = "classic"))]
mod esm_test;

#[cfg(all(test, feature = "classic"))]
mod esm_http_test;

#[cfg(all(test, feature = "classic"))]
mod instance_cache_test;

#[cfg(all(test, feature = "classic"))]
mod interop_test;

#[cfg(all(test, feature = "classic"))]
mod js_delegate_tests {
    use super::{
        js_delegate_add_ref, js_delegate_query_interface, js_delegate_release, JsDelegate,
        JS_DELEGATE_VTBL,
    };
    use std::ffi::c_void;
    use std::sync::atomic::AtomicU32;
    use windows::core::{IUnknown, Interface, GUID, HRESULT};

    /// Build a JsDelegate with a null data pointer for reference-count-only tests.
    /// Callers must ensure the delegate's ref_count never reaches 0 (which would
    /// try to free the null data pointer).
    unsafe fn make_test_delegate(guid: GUID) -> *mut JsDelegate {
        Box::into_raw(Box::new(JsDelegate {
            vtable: &JS_DELEGATE_VTBL as *const _,
            ref_count: AtomicU32::new(1),
            guid,
            data: std::ptr::null_mut(),
        }))
    }

    #[test]
    fn js_delegate_add_ref_increments_count() {
        unsafe {
            let ptr = make_test_delegate(GUID::zeroed());
            assert_eq!(js_delegate_add_ref(ptr), 2);
            assert_eq!(js_delegate_add_ref(ptr), 3);
            // Release back to 1 so the destructor is not triggered.
            js_delegate_release(ptr);
            js_delegate_release(ptr);
            // At ref_count=1 we stop (one more release would trigger Box::from_raw on null data).
            // Leak intentionally to avoid UB in a test-only stub.
            let _ = Box::from_raw(ptr); // free only the JsDelegate; data is null but not accessed
        }
    }

    #[test]
    fn js_delegate_release_decrements_count() {
        unsafe {
            let ptr = make_test_delegate(GUID::zeroed());
            js_delegate_add_ref(ptr); // -> 2
            js_delegate_add_ref(ptr); // -> 3
            assert_eq!(js_delegate_release(ptr), 2);
            assert_eq!(js_delegate_release(ptr), 1);
            // Leave at 1 and free manually.
            let _ = Box::from_raw(ptr);
        }
    }

    #[test]
    fn js_delegate_query_interface_iunknown_succeeds() {
        unsafe {
            let guid = GUID::zeroed();
            let ptr = make_test_delegate(guid);
            let mut out: *mut c_void = std::ptr::null_mut();
            let hr = js_delegate_query_interface(ptr, &IUnknown::IID, &mut out);
            assert_eq!(hr, HRESULT(0), "QI for IUnknown should return S_OK");
            assert_eq!(out, ptr as *mut c_void);
            // QI called AddRef internally; balance it.
            js_delegate_release(ptr);
            // Now ref_count is back to 1; free manually.
            let _ = Box::from_raw(ptr);
        }
    }

    #[test]
    fn js_delegate_query_interface_matching_guid_succeeds() {
        let test_guid = GUID {
            data1: 0xDEAD_BEEF,
            data2: 0xCAFE,
            data3: 0xBABE,
            data4: [1, 2, 3, 4, 5, 6, 7, 8],
        };
        unsafe {
            let ptr = make_test_delegate(test_guid);
            let mut out: *mut c_void = std::ptr::null_mut();
            let hr = js_delegate_query_interface(ptr, &test_guid, &mut out);
            assert_eq!(
                hr,
                HRESULT(0),
                "QI for the delegate's own GUID should return S_OK"
            );
            assert_eq!(out, ptr as *mut c_void);
            js_delegate_release(ptr);
            let _ = Box::from_raw(ptr);
        }
    }

    #[test]
    fn js_delegate_query_interface_unknown_guid_returns_e_nointerface() {
        let other_guid = GUID {
            data1: 0x1111_1111,
            data2: 0x2222,
            data3: 0x3333,
            data4: [0; 8],
        };
        unsafe {
            let ptr = make_test_delegate(GUID::zeroed());
            let mut out: *mut c_void = std::ptr::null_mut();
            let hr = js_delegate_query_interface(ptr, &other_guid, &mut out);
            assert_eq!(
                hr,
                HRESULT(0x80004002u32 as i32),
                "QI for unknown GUID should return E_NOINTERFACE"
            );
            assert!(out.is_null(), "out pointer should be null on E_NOINTERFACE");
            let _ = Box::from_raw(ptr);
        }
    }
}
