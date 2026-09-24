//! Rust half of the ES module loader in `csrc/win_jsr.cpp`. The C++ side owns the V8 module
//! records (compile, link, evaluate, `import()`, `import.meta`); these callbacks answer its
//! engine-neutral questions through `runtime::esm_loader`, the same resolver, source loader and
//! error text the classic runtime uses. Strings handed to C++ are `CString`s it frees with
//! `ns_esm_free`.

use std::ffi::{c_char, c_void, CStr, CString};

use runtime::esm_loader;

fn to_str(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn to_c(s: String) -> *mut c_char {
    CString::new(s).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "C" fn ns_esm_resolve(spec: *const c_char, referrer: *const c_char) -> *mut c_char {
    let spec = to_str(spec).unwrap_or_default();
    to_c(esm_loader::resolve(&spec, to_str(referrer).as_deref()))
}

/// Source text for a registry key, or NULL (the reason is recorded for `ns_esm_not_found`).
#[no_mangle]
pub extern "C" fn ns_esm_read_source(key: *const c_char) -> *mut c_char {
    match to_str(key).and_then(|k| esm_loader::read_source(&k)) {
        Some(source) => to_c(source),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn ns_esm_prefetch(keys: *const *const c_char, count: usize) {
    if keys.is_null() || count == 0 {
        return;
    }
    let keys: Vec<String> = unsafe { std::slice::from_raw_parts(keys, count) }
        .iter()
        .filter_map(|&k| to_str(k))
        .collect();
    esm_loader::prefetch(&keys);
}

#[no_mangle]
pub extern "C" fn ns_esm_not_found(
    spec: *const c_char,
    referrer: *const c_char,
    key: *const c_char,
) -> *mut c_char {
    let spec = to_str(spec).unwrap_or_default();
    let key = to_str(key).unwrap_or_default();
    to_c(esm_loader::not_found_message(&spec, to_str(referrer).as_deref(), &key))
}

#[no_mangle]
pub extern "C" fn ns_esm_is_volatile(key: *const c_char) -> i32 {
    to_str(key).is_some_and(|k| esm_loader::is_volatile(&k)) as i32
}

#[no_mangle]
pub extern "C" fn ns_esm_forget(key: *const c_char) {
    if let Some(k) = to_str(key) {
        esm_loader::forget(&k);
    }
}

/// An uncaught error from evaluating a module graph: the trace log plus the host's last-JS-error
/// slot (surfaced in the crash report).
#[no_mangle]
pub extern "C" fn ns_esm_report_error(report: *const c_char) {
    let report = to_str(report).unwrap_or_default();
    runtime::debug_output(&format!("[NativeScript] Uncaught error evaluating module: {report}\n"));
    runtime::store_last_js_error(report);
}

#[no_mangle]
pub extern "C" fn ns_esm_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Keys of every registered module record.
pub fn registered_keys() -> Vec<String> {
    extern "C" fn visit(key: *const c_char, user: *mut c_void) {
        let keys = unsafe { &mut *(user as *mut Vec<String>) };
        if let Some(k) = to_str(key) {
            keys.push(k);
        }
    }
    let mut keys: Vec<String> = Vec::new();
    unsafe { crate::ffi::js_esm_for_each_key(visit, &mut keys as *mut Vec<String> as *mut c_void) };
    keys
}

/// Drop a module record (and its recorded load failure). True when one was registered.
pub fn evict(key: &str) -> bool {
    let Ok(c) = CString::new(key) else {
        return false;
    };
    unsafe { crate::ffi::js_esm_evict(c.as_ptr()) != 0 }
}

/// `ns:module` natives the prelude's `__nsModuleBuiltin` wraps: `__nsModuleConfigureLoader(json)`,
/// `__nsModuleInvalidate(urls) → removed count` and `__nsModuleLoadedUrls() → string[]`.
#[cfg(feature = "host_dll")]
pub fn install_ns_module(env: &napi::Env) -> napi::Result<()> {
    use napi::{CallContext, JsObject, JsUnknown, ValueType};

    let mut global = env.get_global()?;

    let configure = env.create_function_from_closure("__nsModuleConfigureLoader", |ctx: CallContext| {
        let json = ctx.get::<JsUnknown>(0)?.coerce_to_string()?.into_utf8()?.as_str()?.to_owned();
        esm_loader::configure_loader(&json).map_err(napi::Error::from_reason)?;
        ctx.env.get_undefined()
    })?;
    global.set_named_property("__nsModuleConfigureLoader", configure)?;

    // Registry eviction plus a one-shot cache-bust nonce on each evicted URL's next fetch.
    let invalidate = env.create_function_from_closure("__nsModuleInvalidate", |ctx: CallContext| {
        let list = ctx.get::<JsUnknown>(0)?;
        if !list.is_array()? {
            return Err(napi::Error::from_reason("invalidateModules expects an array of URL strings"));
        }
        let list: JsObject = unsafe { list.cast() };
        let mut keys: Vec<String> = Vec::new();
        for i in 0..list.get_array_length()? {
            let value = list.get_element::<JsUnknown>(i)?;
            if value.get_type()? != ValueType::String {
                return Err(napi::Error::from_reason(format!(
                    "invalidateModules: urls[{i}] must be a string"
                )));
            }
            let url = value.coerce_to_string()?.into_utf8()?.as_str()?.to_owned();
            let key = esm_loader::registry_key_for(&url);
            if !key.is_empty() && !keys.contains(&key) {
                keys.push(key);
            }
        }
        let removed = keys.iter().filter(|k| evict(k)).count();
        let http_keys: Vec<String> = keys.into_iter().filter(|k| esm_loader::is_http(k)).collect();
        esm_loader::mark_keys_for_cache_bust(&http_keys);
        ctx.env.create_int32(removed as i32)
    })?;
    global.set_named_property("__nsModuleInvalidate", invalidate)?;

    // The URL-keyed (served) modules currently registered.
    let loaded = env.create_function_from_closure("__nsModuleLoadedUrls", |ctx: CallContext| {
        let mut urls: Vec<String> =
            registered_keys().into_iter().filter(|k| k.contains("://")).collect();
        urls.sort();
        let mut array = ctx.env.create_array_with_length(urls.len())?;
        for (i, url) in urls.iter().enumerate() {
            array.set_element(i as u32, ctx.env.create_string(url)?)?;
        }
        Ok(array)
    })?;
    global.set_named_property("__nsModuleLoadedUrls", loaded)?;
    Ok(())
}
