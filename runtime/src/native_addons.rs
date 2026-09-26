//! Loading native Node-API addons: `require('./x.node')` and `require('system_lib://x.node')`
//! (the form NativeScript plugins use on Android) resolve here instead of being read as JS.
//!
//! Each addon gets its own `napi_env` over the main context with the Node-API version it declares
//! (`node_api_module_get_api_version_v1`, else 8), and is initialised once per process, like Node.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

const SYSTEM_LIB: &str = "system_lib://";
const DEFAULT_MODULE_API_VERSION: i32 = 8;

struct Addon {
    key: String,
    exports: v8::Global<v8::Value>,
}

thread_local! {
    static ADDONS: RefCell<Vec<Addon>> = const { RefCell::new(Vec::new()) };
}

/// Where `system_lib://name` looks, in order: next to the host executable (where the app's native
/// libraries are deployed), then the app root.
fn resolve(specifier: &str, caller_file: &str, app_root: &str) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if let Some(name) = specifier.strip_prefix(SYSTEM_LIB) {
        let mut dirs = Vec::new();
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            dirs.push(dir);
        }
        if !app_root.is_empty() {
            dirs.push(PathBuf::from(app_root));
        }
        dirs.into_iter().map(|d| d.join(name)).collect()
    } else {
        let path = Path::new(specifier);
        if path.is_absolute() {
            vec![path.to_path_buf()]
        } else {
            let base = Path::new(caller_file)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(app_root));
            vec![base.join(path)]
        }
    };
    candidates.into_iter().find(|p| p.is_file())
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn throw(scope: &mut v8::PinScope, message: &str) {
    if let Some(text) = v8::String::new(scope, message) {
        let error = v8::Exception::error(scope, text);
        scope.throw_exception(error);
    }
}

/// `__nsLoadNativeAddon(specifier, callerFile, appRoot)` → the addon's exports.
pub(crate) fn handle_load_native_addon(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let specifier = args.get(0).to_rust_string_lossy(scope);
    let caller = if args.length() > 1 && args.get(1).is_string() {
        args.get(1).to_rust_string_lossy(scope)
    } else {
        String::new()
    };
    let app_root = if args.length() > 2 && args.get(2).is_string() {
        args.get(2).to_rust_string_lossy(scope)
    } else {
        String::new()
    };

    let Some(path) = resolve(&specifier, &caller, &app_root) else {
        throw(scope, &format!("Cannot find native module: {specifier}"));
        return;
    };
    let path = path.canonicalize().unwrap_or(path);
    let key = path.to_string_lossy().to_lowercase();

    let cached = ADDONS.with(|a| {
        a.borrow()
            .iter()
            .find(|addon| addon.key == key)
            .map(|addon| v8::Local::new(scope, &addon.exports))
    });
    if let Some(exports) = cached {
        retval.set(exports);
        return;
    }

    match load(scope, &path) {
        Ok(exports) => {
            let global = v8::Global::new(scope, exports);
            ADDONS.with(|a| a.borrow_mut().push(Addon { key, exports: global }));
            retval.set(exports);
        }
        Err(message) => throw(scope, &message),
    }
}

fn load<'s>(scope: &mut v8::PinScope<'s, '_>, path: &Path) -> Result<v8::Local<'s, v8::Value>, String> {
    let display = path.display().to_string();
    let wide_path = wide(path);

    // A static-constructor `napi_module_register` call happens inside LoadLibrary.
    let _ = crate::node_api::take_legacy_module();
    let module = unsafe {
        LoadLibraryExW(
            PCWSTR(wide_path.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    }
    .map_err(|e| format!("Failed to load native module {display}: {e}"))?;

    let init: napi_v8_shim::AddonInit = match unsafe { GetProcAddress(module, PCSTR(c"napi_register_module_v1".as_ptr() as *const u8)) } {
        Some(f) => unsafe { std::mem::transmute(f) },
        None => crate::node_api::take_legacy_module()
            .ok_or_else(|| format!("{display} is not a Node-API module (no napi_register_module_v1)"))?,
    };

    let version = match unsafe { GetProcAddress(module, PCSTR(c"node_api_module_get_api_version_v1".as_ptr() as *const u8)) } {
        Some(f) => {
            let get: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute(f) };
            unsafe { get() }
        }
        None => DEFAULT_MODULE_API_VERSION,
    };

    let context = scope.get_current_context();
    let env = unsafe { napi_v8_shim::ns_napi_env_create(&*context as *const v8::Context as *const _, version) };
    if env.is_null() {
        return Err(format!("Failed to create a Node-API environment for {display}"));
    }
    crate::node_api::register_env(env);
    crate::node_api::set_module_file_name(env, &format!("file:///{}", display.replace('\\', "/")));

    let exports = v8::Object::new(scope);
    let exports_value: v8::Local<v8::Value> = exports.into();
    v8::tc_scope!(tc, scope);
    let result = unsafe {
        napi_v8_shim::ns_napi_call_module_init(env, init, &*exports_value as *const v8::Value as *mut _)
    };
    if tc.has_caught() {
        let message = tc
            .exception()
            .and_then(|e| e.to_string(tc))
            .map(|s| s.to_rust_string_lossy(tc))
            .unwrap_or_else(|| "exception".into());
        return Err(format!("Initialising native module {display} threw: {message}"));
    }
    if result.is_null() {
        return Ok(exports_value);
    }
    // A napi_value is the pointer inside a v8::Local in the current handle scope.
    Ok(unsafe { std::mem::transmute::<*mut std::ffi::c_void, v8::Local<'s, v8::Value>>(result) })
}
