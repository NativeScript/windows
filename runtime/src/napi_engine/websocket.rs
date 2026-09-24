//! Node-API bindings for the native WebSocket (`crate::websocket`, shared with the classic
//! runtime): `__nsWebSocketOpen` / `__nsWebSocketSend` / `__nsWebSocketClose`, plus `pump`, which
//! the event loop runs each turn to deliver socket events on the JS thread. The WHATWG `WebSocket`
//! class over these natives lives in the shared JS prelude. The Vite HMR client needs it, and
//! `@valor/nativescript-websockets` has no Windows implementation.
//!
//! Install-if-missing: a host that already has `WebSocket` (Node 22+, Bun, Deno) keeps its own.

use std::cell::RefCell;
use std::collections::HashMap;

use napi::{sys, CallContext, Env, JsFunction, JsGlobal, JsObject, JsUnknown, NapiRaw, NapiValue, ValueType};

use crate::globals::console::write_console;
use crate::websocket::{self as socket, Event};

thread_local! {
    /// Socket id -> its `onEvent(type, a, b, c)` callback, pinned until the close event.
    static CALLBACKS: RefCell<HashMap<i32, (sys::napi_env, sys::napi_ref)>> =
        RefCell::new(HashMap::new());
}

fn has_function(global: &JsGlobal, name: &str) -> bool {
    matches!(
        global
            .get_named_property::<JsUnknown>(name)
            .and_then(|v| v.get_type()),
        Ok(ValueType::Function)
    )
}

fn arg_string(ctx: &CallContext, index: usize) -> napi::Result<String> {
    if ctx.length <= index {
        return Ok(String::new());
    }
    let v = ctx.get::<JsUnknown>(index)?;
    if matches!(v.get_type()?, ValueType::Undefined | ValueType::Null) {
        return Ok(String::new());
    }
    Ok(v.coerce_to_string()?.into_utf8()?.as_str()?.to_owned())
}

/// Bytes of an ArrayBuffer or typed array; `None` for anything else.
unsafe fn binary_bytes(env: sys::napi_env, value: sys::napi_value) -> Option<Vec<u8>> {
    let mut is = false;
    if sys::napi_is_arraybuffer(env, value, &mut is) == sys::Status::napi_ok && is {
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0usize;
        if sys::napi_get_arraybuffer_info(env, value, &mut data, &mut len) != sys::Status::napi_ok {
            return Some(Vec::new());
        }
        return Some(if data.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const u8, len).to_vec()
        });
    }
    if sys::napi_is_typedarray(env, value, &mut is) == sys::Status::napi_ok && is {
        let mut kind = 0;
        let mut length = 0usize;
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut buffer: sys::napi_value = std::ptr::null_mut();
        let mut offset = 0usize;
        if sys::napi_get_typedarray_info(
            env, value, &mut kind, &mut length, &mut data, &mut buffer, &mut offset,
        ) != sys::Status::napi_ok
        {
            return Some(Vec::new());
        }
        let element_size = match kind {
            sys::TypedarrayType::int16_array | sys::TypedarrayType::uint16_array => 2,
            sys::TypedarrayType::int32_array
            | sys::TypedarrayType::uint32_array
            | sys::TypedarrayType::float32_array => 4,
            sys::TypedarrayType::float64_array
            | sys::TypedarrayType::bigint64_array
            | sys::TypedarrayType::biguint64_array => 8,
            _ => 1,
        };
        let bytes = length * element_size;
        return Some(if data.is_null() || bytes == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const u8, bytes).to_vec()
        });
    }
    None
}

pub fn install_websocket(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;
    if has_function(&global, "WebSocket") || has_function(&global, "__nsWebSocketOpen") {
        return Ok(());
    }

    // `__nsWebSocketOpen(url, protocols, onEvent) → id`. `onEvent(type, a, b, c)` receives
    // `('open', protocol)`, `('message', data)`, `('error', message)` and
    // `('close', code, reason, wasClean)`.
    let open = env.create_function_from_closure("__nsWebSocketOpen", |ctx: CallContext| {
        let url = arg_string(&ctx, 0)?;
        let mut protocols = Vec::new();
        if ctx.length > 1 {
            let list = ctx.get::<JsUnknown>(1)?;
            if list.is_array()? {
                let list: JsObject = unsafe { list.cast() };
                for i in 0..list.get_array_length()? {
                    let p = list.get_element::<JsUnknown>(i)?;
                    protocols.push(p.coerce_to_string()?.into_utf8()?.as_str()?.to_owned());
                }
            }
        }
        let callback = ctx.get::<JsFunction>(2)?;
        let env = ctx.env.raw();
        let mut callback_ref: sys::napi_ref = std::ptr::null_mut();
        let status = unsafe { sys::napi_create_reference(env, callback.raw(), 1, &mut callback_ref) };
        if status != sys::Status::napi_ok || callback_ref.is_null() {
            return Err(napi::Error::from_reason("WebSocket: failed to pin the event callback"));
        }
        let id = socket::connect(url, protocols);
        CALLBACKS.with(|c| c.borrow_mut().insert(id, (env, callback_ref)));
        Ok(id)
    })?;
    global.set_named_property("__nsWebSocketOpen", open)?;

    // `__nsWebSocketSend(id, data)`: a string (text frame) or ArrayBuffer/typed array (binary
    // frame). Returns false when the socket isn't open.
    let send = env.create_function_from_closure("__nsWebSocketSend", |ctx: CallContext| {
        let id = ctx.get::<JsUnknown>(0)?.coerce_to_number()?.get_int32()?;
        let data = if ctx.length > 1 { Some(ctx.get::<JsUnknown>(1)?) } else { None };
        let binary = match &data {
            Some(d) if d.get_type()? == ValueType::Object => unsafe { binary_bytes(ctx.env.raw(), d.raw()) },
            _ => None,
        };
        let sent = match binary {
            Some(bytes) => socket::send(id, &bytes, true),
            None => socket::send(id, arg_string(&ctx, 1)?.as_bytes(), false),
        };
        ctx.env.get_boolean(sent)
    })?;
    global.set_named_property("__nsWebSocketSend", send)?;

    // `__nsWebSocketClose(id, code, reason)`.
    let close = env.create_function_from_closure("__nsWebSocketClose", |ctx: CallContext| {
        let id = ctx.get::<JsUnknown>(0)?.coerce_to_number()?.get_int32()?;
        let code = if ctx.length > 1 {
            ctx.get::<JsUnknown>(1)?.coerce_to_number()?.get_uint32().unwrap_or(0)
        } else {
            0
        };
        let code = if code > 0 { code as u16 } else { 1000 };
        socket::shutdown(id, code, &arg_string(&ctx, 2)?);
        ctx.env.get_undefined()
    })?;
    global.set_named_property("__nsWebSocketClose", close)?;
    Ok(())
}

/// Deliver queued socket events to their JS callbacks. Runs each event-loop turn on the JS thread.
pub fn pump(env: &Env) {
    let events = socket::take_events();
    if events.is_empty() {
        return;
    }
    for (id, event) in events {
        let Some((cb_env, callback_ref)) = CALLBACKS.with(|c| c.borrow().get(&id).copied()) else {
            continue;
        };
        let closing = matches!(event, Event::Close { .. });
        unsafe { deliver(env, callback_ref, event) };
        if closing {
            CALLBACKS.with(|c| c.borrow_mut().remove(&id));
            unsafe {
                let _ = sys::napi_delete_reference(cb_env, callback_ref);
            }
        }
    }
}

unsafe fn deliver(env: &Env, callback_ref: sys::napi_ref, event: Event) {
    let raw = env.raw();
    let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
    if sys::napi_open_handle_scope(raw, &mut scope) != sys::Status::napi_ok {
        return;
    }
    let mut func: sys::napi_value = std::ptr::null_mut();
    if sys::napi_get_reference_value(raw, callback_ref, &mut func) == sys::Status::napi_ok
        && !func.is_null()
    {
        let argv = event_args(env, event).unwrap_or_default();
        let mut recv: sys::napi_value = std::ptr::null_mut();
        let _ = sys::napi_get_undefined(raw, &mut recv);
        let mut result: sys::napi_value = std::ptr::null_mut();
        if sys::napi_call_function(raw, recv, func, argv.len(), argv.as_ptr(), &mut result)
            != sys::Status::napi_ok
        {
            let mut exc: sys::napi_value = std::ptr::null_mut();
            if sys::napi_get_and_clear_last_exception(raw, &mut exc) == sys::Status::napi_ok
                && !exc.is_null()
            {
                let msg = JsUnknown::from_raw_unchecked(raw, exc)
                    .coerce_to_string()
                    .and_then(|s| s.into_utf8())
                    .and_then(|s| Ok(s.as_str()?.to_owned()))
                    .unwrap_or_else(|_| "<unprintable exception>".into());
                write_console(&format!("[NativeScript] WebSocket event handler error: {msg}\n"));
            }
        }
    }
    let _ = sys::napi_close_handle_scope(raw, scope);
}

fn event_args(env: &Env, event: Event) -> napi::Result<Vec<sys::napi_value>> {
    let s = |v: &str| -> napi::Result<sys::napi_value> { Ok(unsafe { env.create_string(v)?.raw() }) };
    Ok(match event {
        Event::Open(protocol) => vec![s("open")?, s(&protocol)?],
        Event::Text(text) => vec![s("message")?, s(&text)?],
        Event::Binary(bytes) => {
            let buffer = env.create_arraybuffer_with_data(bytes)?.into_raw();
            vec![s("message")?, unsafe { buffer.raw() }]
        }
        Event::Error(message) => vec![s("error")?, s(&message)?],
        Event::Close { code, reason, clean } => vec![
            s("close")?,
            unsafe { env.create_int32(code as i32)?.raw() },
            s(&reason)?,
            unsafe { env.get_boolean(clean)?.raw() },
        ],
    })
}
