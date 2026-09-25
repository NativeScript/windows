#[cfg(feature = "classic")]
use crate::class_helpers::{collect_class_methods, collect_class_properties};
#[cfg(feature = "classic")]
use crate::DeclarationFFI;
#[cfg(feature = "classic")]
use metadata::declarations::class_declaration::ClassDeclaration;
#[cfg(feature = "classic")]
use metadata::declarations::declaration::{Declaration, DeclarationKind};
#[cfg(feature = "classic")]
use metadata::meta_data_reader::MetadataReader;
use std::cell::RefCell;
use std::collections::HashMap;
#[cfg(feature = "classic")]
use std::ffi::c_int;
use std::sync::OnceLock;
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    self, GetConsoleMode, GetStdHandle, CONSOLE_MODE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::EventLog::{
    RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE,
    EVENTLOG_WARNING_TYPE, REPORT_EVENT_TYPE,
};

#[cfg(feature = "classic")]
pub fn init_console(
    scope: &mut v8::ContextScope<v8::HandleScope<v8::Context>>,
    context: v8::Local<v8::Context>,
) {
    let console = v8::Object::new(scope);

    macro_rules! bind {
        ($name:expr, $cb:expr) => {{
            let f = v8::Function::new(scope, $cb).unwrap();
            let key: v8::Local<v8::Value> = v8::String::new(scope, $name).unwrap().into();
            console.set(scope, key, f.into());
        }};
    }

    bind!("log", handle_console_log);
    bind!("info", handle_console_log); // alias — same output, different semantics
    bind!("dir", handle_console_dir);
    bind!("warn", handle_console_warn);
    bind!("error", handle_console_error);
    bind!("trace", handle_console_trace);
    bind!("assert", handle_console_assert);
    bind!("time", handle_console_time);
    bind!("timeEnd", handle_console_time_end);
    bind!("timeLog", handle_console_time_log);
    bind!("table", handle_console_table);

    let global = context.global(scope);
    let key = v8::String::new(scope, "console").unwrap();
    global.define_own_property(
        scope,
        key.into(),
        console.into(),
        v8::PropertyAttribute::READ_ONLY,
    );
}

#[cfg(feature = "classic")]
fn handle_item_log(
    scope: &mut v8::PinScope<'_, '_>,
    item: v8::Local<v8::Value>,
    output: &mut String,
    is_last: bool,
    rich: bool,
) {
    if item.is_array_buffer_view() {
        output.push_str(&item.to_rust_string_lossy(scope));
        if !is_last {
            output.push(' ');
        }
        return;
    }

    if item.is_array() {
        if let Ok(arr) = v8::Local::<v8::Array>::try_from(item) {
            let len = arr.length() as usize;
            output.push('[');
            for i in 0..len {
                if i > 0 {
                    output.push_str(", ");
                }
                if let Some(child) = arr.get_index(scope, i as u32) {
                    handle_item_log(scope, child, output, true, false);
                }
            }
            output.push(']');
            if !is_last {
                output.push(' ');
            }
        }
        return;
    }

    if item.is_function() {
        output.push_str(&item.to_rust_string_lossy(scope));
        if !is_last {
            output.push(' ');
        }
        return;
    }

    if item.is_object() {
        let obj = match v8::Local::<v8::Object>::try_from(item) {
            Ok(o) => o,
            Err(_) => {
                output.push_str(&item.to_rust_string_lossy(scope));
                if !is_last {
                    output.push(' ');
                }
                return;
            }
        };

        // Prefer explicit __typeName__ metadata when present
        if let Some(type_key) = v8::String::new(scope, "__typeName__") {
            if let Some(type_val) = obj.get(scope, type_key.into()) {
                if type_val.is_string() {
                    let full_name = type_val.to_rust_string_lossy(scope);
                    if let Some(declaration) = MetadataReader::find_by_name(full_name.as_ref()) {
                        let lock = declaration.read();
                        if let Some(class_dec) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                            if !rich {
                                output.push_str(class_dec.name());
                                if !is_last {
                                    output.push(' ');
                                }
                                return;
                            }
                            output.push_str(&format!("{} (constructor) {{\n", class_dec.name()));
                            let props = collect_class_properties(class_dec);
                            for p in props.iter().filter(|p| p.is_static()) {
                                output.push_str(&format!("  {}: <static>\n", p.name()));
                            }
                            let methods = collect_class_methods(class_dec);
                            let proto_methods: Vec<_> =
                                methods.iter().filter(|m| !m.is_static()).collect();
                            if !proto_methods.is_empty() {
                                output.push_str("  prototype methods: [");
                                for (i, m) in proto_methods.iter().enumerate() {
                                    if i > 0 {
                                        output.push_str(", ");
                                    }
                                    let mut mn = m.overload_name().to_string();
                                    if mn.is_empty() {
                                        mn = m.name().to_string();
                                    }
                                    output.push_str(&mn);
                                }
                                output.push_str("]\n");
                            }
                            output.push_str("}\n");
                            if !is_last {
                                output.push(' ');
                            }
                            return;
                        }
                    }
                }
            }
        }

        // WinRT native proxy
        if let Some(type_name) = winrt_type_name_from_object(scope, obj) {
            if !rich {
                output.push_str(&type_name);
                if !is_last {
                    output.push(' ');
                }
                return;
            }
            output.push_str(&format!("{} {{\n", type_name));
            output.push_str("  properties: <native>\n");
            output.push_str("  methods: <native>\n");
            output.push_str("}\n");
            if !is_last {
                output.push(' ');
            }
            return;
        }

        // Rich inspection for console.dir
        if rich {
            if let Some(prop_names) =
                obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            {
                output.push_str("{\n");
                let length = prop_names.length() as usize;
                for i in 0..length {
                    if let Some(name_val) = prop_names.get_index(scope, i as u32) {
                        if let Ok(name_str) = v8::Local::<v8::String>::try_from(name_val) {
                            let key = name_str.to_rust_string_lossy(scope);
                            v8::tc_scope!(tc, scope);
                            let prop_val = obj.get(tc, name_str.into());
                            if tc.has_caught() {
                                output.push_str(&format!("  {}: <getter threw>\n", key));
                                continue;
                            }
                            if let Some(v) = prop_val {
                                if v.is_function() {
                                    output.push_str(&format!("  {}: ()\n", key));
                                } else if v.is_array() {
                                    if let Ok(arr) = v8::Local::<v8::Array>::try_from(v) {
                                        if let Some(json) = v8::json::stringify(tc, arr.into()) {
                                            output.push_str(&format!(
                                                "  {}: {}\n",
                                                key,
                                                json.to_rust_string_lossy(tc)
                                            ));
                                        } else {
                                            output.push_str(&format!("  {}: [Array]\n", key));
                                        }
                                    } else {
                                        output.push_str(&format!("  {}: [Array]\n", key));
                                    }
                                } else if v.is_object() {
                                    if let Ok(obj_val) = v8::Local::<v8::Object>::try_from(v) {
                                        let s = transform_js_object(tc, obj_val);
                                        output.push_str(&format!("  {}: {}\n", key, s));
                                    } else {
                                        output.push_str(&format!("  {}: <object>\n", key));
                                    }
                                } else if let Some(sv) = v.to_string(tc) {
                                    output.push_str(&format!(
                                        "  {}: {}\n",
                                        key,
                                        sv.to_rust_string_lossy(tc)
                                    ));
                                } else {
                                    output.push_str(&format!("  {}: <unavailable>\n", key));
                                }
                            } else {
                                output.push_str(&format!("  {}: <unavailable>\n", key));
                            }
                        }
                    }
                }
                output.push_str("}\n");
                if !is_last {
                    output.push(' ');
                }
                return;
            }
        }

        // Shallow summary for plain JS objects
        {
            v8::tc_scope!(tc, scope);
            if let Some(prop_names) =
                obj.get_own_property_names(tc, v8::GetPropertyNamesArgs::default())
            {
                let mut parts: Vec<String> = Vec::new();
                let length = prop_names.length() as usize;
                for i in 0..length {
                    if let Some(name_val) = prop_names.get_index(tc, i as u32) {
                        if let Ok(name_str) = v8::Local::<v8::String>::try_from(name_val) {
                            let key = name_str.to_rust_string_lossy(tc);
                            let prop_val = obj.get(tc, name_str.into());
                            if tc.has_caught() {
                                parts.push(format!("{}: <getter threw>", key));
                                continue;
                            }
                            if let Some(v) = prop_val {
                                // Prefer short native description when available.
                                if let Some(desc) = short_js_value_description(tc, v) {
                                    parts.push(format!("{}: {}", key, desc));
                                    continue;
                                }
                                // Fallbacks for common types
                                if v.is_function() {
                                    parts.push(format!("{}: ()", key));
                                } else if v.is_string() || v.is_number() || v.is_boolean() {
                                    if let Some(sv) = v.to_string(tc) {
                                        parts.push(format!(
                                            "{}: {}",
                                            key,
                                            sv.to_rust_string_lossy(tc)
                                        ));
                                    } else {
                                        parts.push(format!("{}: <unavailable>", key));
                                    }
                                } else if v.is_object() {
                                    if let Ok(o) = v8::Local::<v8::Object>::try_from(v) {
                                        let s = transform_js_object(tc, o);
                                        parts.push(format!("{}: {}", key, s));
                                    } else {
                                        parts.push(format!("{}: <object>", key));
                                    }
                                } else {
                                    if let Some(sv) = v.to_string(tc) {
                                        parts.push(format!(
                                            "{}: {}",
                                            key,
                                            sv.to_rust_string_lossy(tc)
                                        ));
                                    } else {
                                        parts.push(format!("{}: <unavailable>", key));
                                    }
                                }
                            } else {
                                parts.push(format!("{}: <unavailable>", key));
                            }
                        }
                    }
                }
                if !parts.is_empty() {
                    output.push_str(&format!("{{ {} }}", parts.join(", ")));
                    if !is_last {
                        output.push(' ');
                    }
                    return;
                }
            }
        }

        // Fall through to generic stringification when no short summary produced.
        v8::tc_scope!(tc, scope);
        if let Some(s) = item.to_string(tc) {
            if !tc.has_caught() {
                let s = s.to_rust_string_lossy(tc);
                if !s.contains("[object Object]") {
                    output.push_str(&s);
                    if !is_last {
                        output.push(' ');
                    }
                    return;
                }
            }
        }
        if let Some(json) = v8::json::stringify(tc, item) {
            let s = json.to_rust_string_lossy(tc);
            output.push_str(if s.contains("circular structure") {
                "#CR"
            } else {
                &s
            });
        } else {
            output.push_str("[object Object]");
        }
        if !is_last {
            output.push(' ');
        }
        return;
    }

    output.push_str(&item.to_rust_string_lossy(scope));
    if !is_last {
        output.push(' ');
    }
}

/// Extract the WinRT type name from a native proxy object by inspecting the
/// DeclarationFFI External in internal field 0.  Checks the instance, then its
/// constructor, then constructor.prototype.  Returns None for plain JS objects.
///
/// All casts use TryFrom rather than unchecked cast so V8 built-in objects
/// whose slot 0 holds a non-External value (SMI, backing-store pointer, …)
/// never cause a BadType panic.
#[cfg(feature = "classic")]
fn winrt_type_name_from_object(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<String> {
    if let Some(name) = winrt_name_from_slot(scope, obj) {
        return Some(name);
    }
    let ctor_key = v8::String::new(scope, "constructor")?;
    let ctor_val = obj.get(scope, ctor_key.into())?;
    if !ctor_val.is_object() {
        return None;
    }
    let ctor_obj = v8::Local::<v8::Object>::try_from(ctor_val).ok()?;
    if let Some(name) = winrt_name_from_slot(scope, ctor_obj) {
        return Some(name);
    }
    let proto_key = v8::String::new(scope, "prototype")?;
    let proto_val = ctor_obj.get(scope, proto_key.into())?;
    if !proto_val.is_object() {
        return None;
    }
    let proto_obj = v8::Local::<v8::Object>::try_from(proto_val).ok()?;
    winrt_name_from_slot(scope, proto_obj)
}

#[cfg(feature = "classic")]
fn winrt_name_from_slot(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<String> {
    let field = obj.get_internal_field(scope, 0)?;
    // TryFrom returns Err for non-External slots (e.g. V8 SMIs) — safe fallthrough.
    let ext = v8::Local::<v8::External>::try_from(field).ok()?;
    let dec_ptr = ext.value() as *mut DeclarationFFI;
    if dec_ptr.is_null() {
        return None;
    }
    let dec = unsafe { &*dec_ptr };
    let lock = dec.read();
    if !matches!(
        lock.kind(),
        DeclarationKind::Class
            | DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance
    ) {
        return None;
    }
    Some(lock.name().to_string())
}

/// Detect once whether stdout is attached to a real console.
fn console_handle() -> Option<HANDLE> {
    static PROBED: OnceLock<Option<isize>> = OnceLock::new();
    let raw = *PROBED.get_or_init(|| unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE).ok()?;
        if h.is_invalid() {
            return None;
        }
        let mut mode = CONSOLE_MODE::default();
        if GetConsoleMode(h, &mut mode).is_ok() {
            Some(h.0 as isize)
        } else {
            None
        }
    });
    raw.map(|p| HANDLE(p as *mut _))
}

pub(crate) fn write_console(value: &str) {
    if let Some(handle) = console_handle() {
        let wide: Vec<u16> = value.encode_utf16().collect();
        let _ = unsafe { Console::WriteConsoleW(handle, &wide, None, None) };
    }
    // Write to runtime-configurable trace log if enabled. Preserve legacy
    // `NS_DEBUG` env override for verbose debugging during development.
    if crate::is_log_to_console() || std::env::var("NS_DEBUG").is_ok() {
        crate::debug_output(value);
    }
    let event_type: REPORT_EVENT_TYPE = if value.starts_with("[ERROR]") {
        EVENTLOG_ERROR_TYPE
    } else if value.starts_with("[WARN]") {
        EVENTLOG_WARNING_TYPE
    } else {
        EVENTLOG_INFORMATION_TYPE
    };
    report_event(value, event_type);
}

pub(crate) fn report_event(message: &str, event_type: REPORT_EVENT_TYPE) {
    static EVENT_SOURCE: OnceLock<isize> = OnceLock::new();
    let h_raw = *EVENT_SOURCE.get_or_init(|| unsafe {
        let source_w: Vec<u16> = "NativeScript\0".encode_utf16().collect();
        RegisterEventSourceW(PCWSTR::null(), PCWSTR::from_raw(source_w.as_ptr()))
            .map(|h| h.0 as isize)
            .unwrap_or(0)
    });
    if h_raw == 0 {
        return;
    }
    let h = HANDLE(h_raw as *mut _);
    let msg_w: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    let strings = [PCWSTR::from_raw(msg_w.as_ptr())];
    unsafe {
        let _ = ReportEventW(h, event_type, 0, 0, None, 0, Some(&strings), None);
    }
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_log(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let mut value = String::from("[INFO] ");
    let length = args.length() as usize;
    for i in 0..length {
        handle_item_log(
            scope,
            args.get(i as c_int),
            &mut value,
            i == length.saturating_sub(1),
            false,
        );
    }
    value.push('\n');
    write_console(&value);
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_warn(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let mut value = String::from("[WARN] ");
    let length = args.length() as usize;
    for i in 0..length {
        handle_item_log(
            scope,
            args.get(i as c_int),
            &mut value,
            i == length.saturating_sub(1),
            false,
        );
    }
    value.push('\n');
    write_console(&value);
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_error(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let mut value = String::from("[ERROR] ");
    let length = args.length() as usize;
    let mut printed_stack = false;

    for i in 0..length {
        let is_last = i == length.saturating_sub(1);
        let arg = args.get(i as c_int);

        let used_stack = 'stack: {
            if !arg.is_object() {
                break 'stack false;
            }
            let Ok(obj) = v8::Local::<v8::Object>::try_from(arg) else {
                break 'stack false;
            };
            let Some(key) = v8::String::new(scope, "stack") else {
                break 'stack false;
            };
            v8::tc_scope!(tc, scope);
            let Some(stack_val) = obj.get(tc, key.into()) else {
                break 'stack false;
            };
            if tc.has_caught() || !stack_val.is_string() {
                break 'stack false;
            }

            // Prefer remapped stack if JS-side remapper is present: global.__ns_remapStack
            let stack_str = stack_val.to_rust_string_lossy(tc);
            let remapped_opt = (|| {
                // Get current context and global object from the TryCatch scope
                let context = tc.get_current_context();
                let global = context.global(tc);
                let remap_key = v8::String::new(tc, "__ns_remapStack")?;
                let remap_val = global.get(tc, remap_key.into())?;
                if !remap_val.is_function() {
                    return None;
                }
                let func = v8::Local::<v8::Function>::try_from(remap_val).ok()?;
                let arg = v8::String::new(tc, &stack_str)?.into();
                let this = global.into();
                let result = func.call(tc, this, &[arg])?;
                if result.is_string() {
                    Some(result.to_rust_string_lossy(tc))
                } else {
                    None
                }
            })();

            if let Some(r) = remapped_opt {
                value.push_str(&r);
            } else {
                value.push_str(&stack_str);
            }
            if !is_last {
                value.push(' ');
            }
            true
        };

        if used_stack {
            printed_stack = true;
        } else {
            handle_item_log(scope, arg, &mut value, is_last, false);
        }
    }

    if !printed_stack {
        if let Some(stack) = v8::StackTrace::current_stack_trace(scope, 3) {
            if stack.get_frame_count() > 0 {
                if let Some(frame) = stack.get_frame(scope, 0) {
                    let script = frame
                        .get_script_name(scope)
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    let line = frame.get_line_number();
                    if !script.is_empty() {
                        value.push_str(&format!("\n    at {}:{}", script, line));
                    }
                }
            }
        }
    }

    value.push('\n');
    write_console(&value);
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_dir(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let mut value = String::new();
    let length = args.length() as usize;
    for i in 0..length {
        handle_item_log(
            scope,
            args.get(i as c_int),
            &mut value,
            i == length.saturating_sub(1),
            true,
        );
    }
    value.push('\n');
    write_console(&value);
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_trace(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let mut value = String::from("[TRACE] ");
    let length = args.length() as usize;
    if length == 0 {
        value.push_str("Trace");
    } else {
        for i in 0..length {
            handle_item_log(
                scope,
                args.get(i as c_int),
                &mut value,
                i == length.saturating_sub(1),
                false,
            );
        }
    }

    if let Some(stack) = v8::StackTrace::current_stack_trace(scope, 10) {
        value.push('\n');
        let frame_count = stack.get_frame_count();
        for i in 0..frame_count {
            if let Some(frame) = stack.get_frame(scope, i) {
                let func = frame
                    .get_function_name(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "<anonymous>".to_string());
                let script = frame
                    .get_script_name(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| "VM".to_string());
                let line = frame.get_line_number();
                let col = frame.get_column();
                value.push_str(&format!("    at {} ({}:{}:{})\n", func, script, line, col));
            }
        }
    } else {
        value.push('\n');
    }
    write_console(&value);
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_assert(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let passes = args.length() > 0 && args.get(0).boolean_value(scope);
    if passes {
        return;
    }

    let mut value = String::from("[ERROR] Assertion failed");
    if args.length() > 1 {
        value.push_str(": ");
        let length = args.length() as usize;
        for i in 1..length {
            handle_item_log(
                scope,
                args.get(i as c_int),
                &mut value,
                i == length.saturating_sub(1),
                false,
            );
        }
    } else {
        value.push_str(": console.assert");
    }
    value.push('\n');
    write_console(&value);
}

thread_local! {
    /// Per-JS-thread timer store — maps label → start Instant. Shared with the napi console
    /// port so console.time started on one backend can be ended on the other.
    pub(crate) static CONSOLE_TIMERS: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_time(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let label = if args.length() > 0 {
        args.get(0).to_rust_string_lossy(scope)
    } else {
        "default".to_string()
    };
    CONSOLE_TIMERS.with(|t| {
        let mut map = t.borrow_mut();
        if map.contains_key(&label) {
            write_console(&format!("[WARN] Timer '{}' already exists\n", label));
        } else {
            map.insert(label, Instant::now());
        }
    });
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_time_end(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let label = if args.length() > 0 {
        args.get(0).to_rust_string_lossy(scope)
    } else {
        "default".to_string()
    };
    // Extra data args after the label (timeEnd also prints them per spec)
    let extra = format_extra_args(scope, &args, 1);
    CONSOLE_TIMERS.with(|t| {
        let mut map = t.borrow_mut();
        if let Some(start) = map.remove(&label) {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let msg = if extra.is_empty() {
                format!("[INFO] {}: {:.3}ms - timer ended\n", label, ms)
            } else {
                format!("[INFO] {}: {:.3}ms {}- timer ended\n", label, ms, extra)
            };
            write_console(&msg);
        } else {
            write_console(&format!("[WARN] Timer '{}' does not exist\n", label));
        }
    });
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_time_log(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let label = if args.length() > 0 {
        args.get(0).to_rust_string_lossy(scope)
    } else {
        "default".to_string()
    };
    let extra = format_extra_args(scope, &args, 1);
    CONSOLE_TIMERS.with(|t| {
        let map = t.borrow();
        if let Some(start) = map.get(&label) {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let msg = if extra.is_empty() {
                format!("[INFO] {}: {:.3}ms\n", label, ms)
            } else {
                format!("[INFO] {}: {:.3}ms {}\n", label, ms, extra)
            };
            write_console(&msg);
        } else {
            write_console(&format!("[WARN] Timer '{}' does not exist\n", label));
        }
    });
}

/// Format args[start..] into a space-separated string for timeEnd/timeLog extras.
#[cfg(feature = "classic")]
fn format_extra_args(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    start: i32,
) -> String {
    let len = args.length();
    if start >= len {
        return String::new();
    }
    let mut out = String::new();
    for i in start..len {
        handle_item_log(scope, args.get(i), &mut out, i == len - 1, false);
    }
    out
}

#[cfg(feature = "classic")]
pub(crate) fn handle_console_table(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    if args.length() == 0 {
        write_console("[INFO] (no data)\n");
        return;
    }
    let data = args.get(0);

    // Optional second arg: array of column names to include
    let filter: Option<Vec<String>> = if args.length() > 1 {
        let ca = args.get(1);
        if let Ok(arr) = v8::Local::<v8::Array>::try_from(ca) {
            let mut cols = Vec::new();
            for i in 0..arr.length() {
                if let Some(v) = arr.get_index(scope, i) {
                    cols.push(v.to_rust_string_lossy(scope));
                }
            }
            Some(cols)
        } else {
            None
        }
    } else {
        None
    };

    let mut out = String::from("[INFO] \n");

    if data.is_array() {
        if let Ok(arr) = v8::Local::<v8::Array>::try_from(data) {
            out.push_str(&table_from_array(scope, arr, filter.as_deref()));
        }
    } else if data.is_object() {
        if let Ok(obj) = v8::Local::<v8::Object>::try_from(data) {
            out.push_str(&table_from_object(scope, obj));
        }
    } else {
        // Primitive — just log it
        handle_item_log(scope, data, &mut out, true, false);
        out.push('\n');
    }

    write_console(&out);
}

/// Format an array of rows (objects or primitives) as a table.
#[cfg(feature = "classic")]
fn table_from_array(
    scope: &mut v8::PinScope<'_, '_>,
    arr: v8::Local<v8::Array>,
    filter: Option<&[String]>,
) -> String {
    let row_count = arr.length() as usize;
    if row_count == 0 {
        return "(empty)\n".to_string();
    }

    // Discover all column names across all rows (first pass)
    let mut cols: Vec<String> = vec!["(index)".to_string()];
    for i in 0..row_count as u32 {
        if let Some(row_val) = arr.get_index(scope, i) {
            if row_val.is_object() && !row_val.is_array() {
                if let Ok(row_obj) = v8::Local::<v8::Object>::try_from(row_val) {
                    if let Some(keys) =
                        row_obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
                    {
                        for k in 0..keys.length() {
                            if let Some(kv) = keys.get_index(scope, k) {
                                let col = kv.to_rust_string_lossy(scope);
                                if let Some(f) = filter {
                                    if !f.iter().any(|fc| fc == &col) {
                                        continue;
                                    }
                                }
                                if !cols.contains(&col) {
                                    cols.push(col);
                                }
                            }
                        }
                    }
                }
            } else if cols.len() < 2 {
                // Array of primitives → add a "Values" column
                if !cols.contains(&"Values".to_string()) {
                    cols.push("Values".to_string());
                }
            }
        }
    }

    // Build data rows (second pass)
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(row_count);
    for i in 0..row_count as u32 {
        let mut row = vec![i.to_string()];
        if let Some(row_val) = arr.get_index(scope, i) {
            if row_val.is_object() && !row_val.is_array() {
                if let Ok(row_obj) = v8::Local::<v8::Object>::try_from(row_val) {
                    for col in cols.iter().skip(1) {
                        v8::tc_scope!(tc, scope);
                        if let Some(key) = v8::String::new(tc, col) {
                            let cell = if let Some(v) = row_obj.get(tc, key.into()) {
                                if !tc.has_caught() {
                                    let mut s = String::new();
                                    handle_item_log(tc, v, &mut s, true, false);
                                    s
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            };
                            row.push(cell);
                        } else {
                            row.push(String::new());
                        }
                    }
                }
            } else {
                // Primitive value
                let mut s = String::new();
                handle_item_log(scope, row_val, &mut s, true, false);
                row.push(s);
                // Pad remaining columns
                while row.len() < cols.len() {
                    row.push(String::new());
                }
            }
        }
        while row.len() < cols.len() {
            row.push(String::new());
        }
        rows.push(row);
    }

    render_table(&cols, &rows)
}

/// Format a plain JS object as a key→value table.
#[cfg(feature = "classic")]
fn table_from_object(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>) -> String {
    let cols = vec!["(index)".to_string(), "Values".to_string()];
    let mut rows: Vec<Vec<String>> = Vec::new();

    if let Some(keys) = obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default()) {
        for k in 0..keys.length() {
            if let Some(kv) = keys.get_index(scope, k) {
                let key_str = kv.to_rust_string_lossy(scope);
                v8::tc_scope!(tc, scope);
                let cell = if let Some(v) = obj.get(tc, kv) {
                    if !tc.has_caught() {
                        let mut s = String::new();
                        handle_item_log(tc, v, &mut s, true, false);
                        s
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                rows.push(vec![key_str, cell]);
            }
        }
    }

    render_table(&cols, &rows)
}

/// Render a table with box-drawing characters given column headers and row data.
/// Engine-neutral; shared with the napi console port.
pub(crate) fn render_table(cols: &[String], rows: &[Vec<String>]) -> String {
    // Compute column widths: max of header width and widest cell in that column
    let mut widths: Vec<usize> = cols.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let mut out = String::new();

    // Top border:  ┌──────┬──────┐
    out.push('┌');
    for (i, &w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < widths.len() { '┬' } else { '┐' });
    }
    out.push('\n');

    // Header:  │ col  │ col  │
    out.push('│');
    for (col, &w) in cols.iter().zip(widths.iter()) {
        out.push_str(&format!(" {:<width$} │", col, width = w));
    }
    out.push('\n');

    // Header/body separator:  ├──────┼──────┤
    out.push('├');
    for (i, &w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < widths.len() { '┼' } else { '┤' });
    }
    out.push('\n');

    // Data rows:  │ val  │ val  │
    for row in rows {
        out.push('│');
        for (i, &w) in widths.iter().enumerate() {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            out.push_str(&format!(" {:<width$} │", cell, width = w));
        }
        out.push('\n');
    }

    // Bottom border:  └──────┴──────┘
    out.push('└');
    for (i, &w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < widths.len() { '┴' } else { '┘' });
    }
    out.push('\n');

    out
}

#[cfg(feature = "classic")]
fn transform_js_object(scope: &mut v8::PinScope<'_, '_>, object: v8::Local<v8::Object>) -> String {
    v8::tc_scope!(tc, scope);
    if let Some(val) = object.to_string(tc) {
        let s = val.to_rust_string_lossy(tc);
        if !s.contains("[object Object]") {
            return s;
        }
    }
    if tc.has_caught() {
        return String::new();
    }
    if let Some(json) = v8::json::stringify(tc, object.into()) {
        let s = json.to_rust_string_lossy(tc);
        if s.contains("circular structure") {
            return "#CR".to_string();
        }
        return s;
    }
    String::new()
}

/// Return a short description for a JS value when it's a native WinRT proxy
/// or an External pointer. Examples: `StackPanel@0x12345`, `External@0xabc`.
#[cfg(feature = "classic")]
fn short_js_value_description(
    scope: &mut v8::PinScope<'_, '_>,
    val: v8::Local<v8::Value>,
) -> Option<String> {
    if val.is_null_or_undefined() {
        return Some("null".to_string());
    }
    if let Ok(ext) = v8::Local::<v8::External>::try_from(val) {
        return Some(format!("External@0x{:x}", ext.value() as usize));
    }
    if val.is_object() {
        // Try to extract DotNet wrapper descriptions from their toString(),
        // e.g. "[DotNetObject NativeScript.Widgets.FlexboxLayout #1]".
        v8::tc_scope!(tc, scope);
        if let Some(sv) = val.to_string(tc) {
            let s = sv.to_rust_string_lossy(tc);
            if s.contains("DotNetObject") {
                let parts: Vec<&str> = s.split_whitespace().collect();
                // Prefer the token that looks like a dotted type name
                for p in parts.iter() {
                    if p.contains('.') {
                        let type_name =
                            p.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '_');
                        // Try to find an ID token like "#1" following it
                        let mut id_suffix = String::new();
                        if let Some(pos) = parts.iter().position(|x| *x == *p) {
                            if parts.len() > pos + 1 {
                                let next = parts[pos + 1];
                                if next.starts_with('#') {
                                    let id = next.trim_matches(|c: char| !c.is_numeric());
                                    if !id.is_empty() {
                                        id_suffix = format!("#{}", id);
                                    }
                                }
                            }
                        }
                        return Some(format!("DotNet.{}{}", type_name, id_suffix));
                    }
                }
            }

            // Try to extract a WinRT type name from internal slot
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(val) {
                if let Some(name) = winrt_type_name_from_object(tc, obj) {
                    // Try to get __native_ptr for identity if present
                    if let Some(ptr_key) = v8::String::new(tc, "__native_ptr") {
                        if let Some(pv) = obj.get(tc, ptr_key.into()) {
                            if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(pv) {
                                return Some(format!("{}@0x{:x}", name, bi.u64_value().0));
                            }
                        }
                    }
                    return Some(format!("{}", name));
                }
            }
        }
    }
    None
}
