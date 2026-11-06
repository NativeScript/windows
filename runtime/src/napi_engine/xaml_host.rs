//! Win32 host windows for system XAML Islands (`DesktopWindowXamlSource`): attach, keyboard
//! pre-translation, placement and window events. Content and XAML events stay in JS.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;

use napi::{sys, Env, JsFunction, JsObject, JsUnknown, NapiRaw, ValueType};
use windows::core::{IUnknown, Interface, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetClientRect,
    GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    IsWindow, IsWindowVisible, LoadCursorW, RegisterClassExW, SetForegroundWindow, SetWindowPos,
    SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, HHOOK,
    IDC_ARROW, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE,
    WA_INACTIVE, WH_GETMESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE,
    WM_DPICHANGED, WM_KEYFIRST, WM_KEYLAST, WM_NCDESTROY, WM_NULL, WM_SETFOCUS, WM_SIZE,
    WNDCLASSEXW, WS_CAPTION, WS_CLIPCHILDREN, WS_EX_DLGMODALFRAME, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_SYSMENU,
};

use crate::napi_engine::value::try_get_external_handle;

const CLASS_NAME: PCWSTR = windows::core::w!("NsWinRTXamlHost");

/// `IDesktopWindowXamlSourceNative2` (windows.ui.xaml.hosting.desktopwindowxamlsource.h).
const IID_XAML_SOURCE_NATIVE2: GUID = GUID::from_u128(0xe3dcd8c7_3057_4692_99c3_7b7720afda31);

#[repr(C)]
struct XamlSourceNative2Vtbl {
    base: windows::core::IUnknown_Vtbl,
    attach_to_window: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    window_handle: unsafe extern "system" fn(*mut c_void, *mut HWND) -> HRESULT,
    pre_translate_message: unsafe extern "system" fn(*mut c_void, *const MSG, *mut BOOL) -> HRESULT,
}

/// An `IDesktopWindowXamlSourceNative2` reference (held as `IUnknown` for ref counting).
#[derive(Clone)]
struct SourceNative(IUnknown);

impl SourceNative {
    fn from_source(source: &IUnknown) -> windows::core::Result<Self> {
        let mut raw = std::ptr::null_mut();
        unsafe { source.query(&IID_XAML_SOURCE_NATIVE2, &mut raw) }.ok()?;
        Ok(Self(unsafe { IUnknown::from_raw(raw) }))
    }

    fn vtbl(&self) -> &XamlSourceNative2Vtbl {
        unsafe { &**(self.0.as_raw() as *const *const XamlSourceNative2Vtbl) }
    }

    fn attach(&self, parent: HWND) -> windows::core::Result<HWND> {
        unsafe { (self.vtbl().attach_to_window)(self.0.as_raw(), parent) }.ok()?;
        let mut island = HWND::default();
        unsafe { (self.vtbl().window_handle)(self.0.as_raw(), &mut island) }.ok()?;
        Ok(island)
    }

    fn pre_translate(&self, msg: &MSG) -> bool {
        let mut handled = BOOL(0);
        let hr = unsafe { (self.vtbl().pre_translate_message)(self.0.as_raw(), msg, &mut handled) };
        hr.is_ok() && handled.as_bool()
    }
}

struct Listener {
    env: sys::napi_env,
    func: sys::napi_ref,
}

struct Host {
    island: HWND,
    source: SourceNative,
    listener: Option<Listener>,
}

thread_local! {
    // Keyed by host HWND.
    static HOSTS: RefCell<HashMap<isize, Host>> = RefCell::new(HashMap::new());
    static KEY_HOOK: Cell<isize> = const { Cell::new(0) };
}

fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn island_of(hwnd: HWND) -> Option<HWND> {
    HOSTS.with(|h| h.borrow().get(&key(hwnd)).map(|h| h.island))
}

fn has_listener(hwnd: HWND) -> bool {
    HOSTS.with(|h| {
        h.borrow()
            .get(&key(hwnd))
            .is_some_and(|h| h.listener.is_some())
    })
}

fn loword(v: usize) -> i32 {
    (v & 0xFFFF) as u16 as i32
}

fn hiword(v: usize) -> i32 {
    ((v >> 16) & 0xFFFF) as u16 as i32
}

/// Queue `event` for the window's listener, so JS never runs inside the window procedure.
fn emit(hwnd: HWND, event: &'static str) {
    let k = key(hwnd);
    if !crate::ui_dispatcher::enqueue_on_ui_thread(move || call_listener(k, event)) {
        call_listener(k, event);
    }
}

fn call_listener(k: isize, event: &str) {
    let listener = HOSTS.with(|h| {
        h.borrow()
            .get(&k)
            .and_then(|h| h.listener.as_ref().map(|l| (l.env, l.func)))
    });
    if let Some((env, func)) = listener {
        unsafe { call_js(env, func, event) };
    }
}

/// Call `func(event)`; a throw is recorded as the last JS error.
unsafe fn call_js(env: sys::napi_env, func: sys::napi_ref, event: &str) {
    let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
    if sys::napi_open_handle_scope(env, &mut scope) != sys::Status::napi_ok {
        return;
    }
    let mut f: sys::napi_value = std::ptr::null_mut();
    let mut arg: sys::napi_value = std::ptr::null_mut();
    let mut recv: sys::napi_value = std::ptr::null_mut();
    if sys::napi_get_reference_value(env, func, &mut f) == sys::Status::napi_ok
        && !f.is_null()
        && sys::napi_create_string_utf8(env, event.as_ptr().cast(), event.len(), &mut arg)
            == sys::Status::napi_ok
        && sys::napi_get_undefined(env, &mut recv) == sys::Status::napi_ok
    {
        let mut result: sys::napi_value = std::ptr::null_mut();
        if sys::napi_call_function(env, recv, f, 1, &arg, &mut result) != sys::Status::napi_ok {
            let mut exc: sys::napi_value = std::ptr::null_mut();
            if sys::napi_get_and_clear_last_exception(env, &mut exc) == sys::Status::napi_ok
                && !exc.is_null()
            {
                if let Some(msg) = crate::napi_engine::delegate::napi_value_to_rust_string(env, exc)
                {
                    crate::store_last_js_error(msg);
                }
            }
        }
    }
    let _ = sys::napi_close_handle_scope(env, scope);
}

/// Forget the window, then send its listener `"closed"` and release it.
fn on_destroyed(hwnd: HWND) {
    let host = HOSTS.with(|h| h.borrow_mut().remove(&key(hwnd)));
    if HOSTS.with(|h| h.borrow().is_empty()) {
        let hook = KEY_HOOK.with(|k| k.replace(0));
        if hook != 0 {
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(hook as *mut c_void)) };
        }
    }
    let Some(Listener { env, func }) = host.and_then(|h| h.listener) else {
        return;
    };
    struct SendListener(sys::napi_env, sys::napi_ref);
    unsafe impl Send for SendListener {}
    let l = SendListener(env, func);
    // Without a dispatcher the reference leaks: destruction can run during process teardown.
    let _ = crate::ui_dispatcher::enqueue_on_ui_thread(move || {
        let l = l;
        unsafe {
            call_js(l.0, l.1, "closed");
            let _ = sys::napi_delete_reference(l.0, l.1);
        }
    });
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            if let Some(island) = island_of(hwnd) {
                let (w, h) = (loword(lparam.0 as usize), hiword(lparam.0 as usize));
                let _ = SetWindowPos(island, None, 0, 0, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        // Keyboard focus belongs to the island, not the empty host.
        WM_SETFOCUS => {
            if let Some(island) = island_of(hwnd) {
                let _ = SetFocus(Some(island));
                return LRESULT(0);
            }
        }
        WM_ACTIVATE => {
            let active = loword(wparam.0) != WA_INACTIVE as i32;
            emit(hwnd, if active { "activate" } else { "deactivate" });
        }
        // With a listener, closing is the listener's call.
        WM_CLOSE if has_listener(hwnd) => {
            emit(hwnd, "close");
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            return LRESULT(0);
        }
        WM_NCDESTROY => on_destroyed(hwnd),
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Offer island key messages to XAML first so Tab navigation and access keys work.
unsafe extern "system" fn get_message_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 == PM_REMOVE.0 as usize && lparam.0 != 0 {
        let msg = &mut *(lparam.0 as *mut MSG);
        if (WM_KEYFIRST..=WM_KEYLAST).contains(&msg.message) && !msg.hwnd.is_invalid() {
            let root = GetAncestor(msg.hwnd, GA_ROOT);
            // Cloned out: XAML key handlers run inside PreTranslateMessage and may touch HOSTS.
            let source = HOSTS.with(|h| h.borrow().get(&key(root)).map(|h| h.source.clone()));
            if source.is_some_and(|s| s.pre_translate(msg)) {
                msg.message = WM_NULL;
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn install_key_hook() {
    if KEY_HOOK.with(|k| k.get()) != 0 {
        return;
    }
    let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if let Ok(hook) =
        unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(get_message_hook), None, thread) }
    {
        KEY_HOOK.with(|k| k.set(hook.0 as isize));
    }
}

fn register_class_once() -> napi::Result<()> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| unsafe {
            let hinstance =
                GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                Err("RegisterClassExW returned 0".to_string())
            } else {
                Ok(())
            }
        })
        .clone()
        .map_err(napi::Error::from_reason)
}

fn err(msg: impl Into<String>) -> napi::Error {
    napi::Error::from_reason(msg.into())
}

/// A window handle from a number, bigint, pointer external or Buffer; `None` for null/undefined.
pub fn hwnd_from_js(env: &Env, v: &JsUnknown) -> napi::Result<Option<HWND>> {
    let raw: isize = match v.get_type()? {
        ValueType::Null | ValueType::Undefined => return Ok(None),
        ValueType::Number => {
            let n: f64 = unsafe { v.cast::<napi::JsNumber>() }.get_double()?;
            n as i64 as isize
        }
        ValueType::BigInt => {
            let (n, _) = unsafe { v.cast::<napi::JsBigInt>() }.get_i64()?;
            n as isize
        }
        ValueType::External => crate::napi_engine::value::ptr_from_external(env, v)
            .map(|p| p as isize)
            .ok_or_else(|| err("window handle external is not a pointer"))?,
        ValueType::Object if v.is_buffer()? => {
            let buf = unsafe { v.cast::<napi::JsBuffer>() }.into_value()?;
            let bytes: &[u8] = &buf;
            let n = std::mem::size_of::<isize>();
            if bytes.len() < n {
                return Err(err("window handle buffer is too short"));
            }
            isize::from_le_bytes(bytes[..n].try_into().unwrap())
        }
        _ => return Err(err("expected a window handle (number, bigint or Buffer)")),
    };
    Ok(if raw == 0 {
        None
    } else {
        Some(HWND(raw as *mut c_void))
    })
}

fn required_hwnd(env: &Env, v: &JsUnknown, what: &str) -> napi::Result<HWND> {
    hwnd_from_js(env, v)?.ok_or_else(|| err(format!("{what}: a window handle is required")))
}

fn opt_prop<T>(
    obj: &JsObject,
    name: &str,
    read: impl FnOnce(JsUnknown) -> napi::Result<T>,
) -> napi::Result<Option<T>> {
    if !obj.has_named_property(name)? {
        return Ok(None);
    }
    let v: JsUnknown = obj.get_named_property(name)?;
    match v.get_type()? {
        ValueType::Undefined | ValueType::Null => Ok(None),
        _ => read(v).map(Some),
    }
}

fn read_bool(v: JsUnknown) -> napi::Result<bool> {
    v.coerce_to_bool()?.get_value()
}

fn read_f64(v: JsUnknown) -> napi::Result<f64> {
    v.coerce_to_number()?.get_double()
}

fn read_string(v: JsUnknown) -> napi::Result<String> {
    v.coerce_to_string()?.into_utf8()?.into_owned()
}

fn set_dwm_u32(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: u32) {
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attr,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
}

/// Create a hidden host window for `source` (a `DesktopWindowXamlSource`); returns its HWND.
/// Options: `style` (`"dialog"` | `"popup"`), `owner`, `title`, `topmost`, `dark`.
pub fn create(env: &Env, source: &JsUnknown, options: Option<JsObject>) -> napi::Result<f64> {
    register_class_once()?;
    let source_ptr = match source.get_type()? {
        ValueType::Object | ValueType::Function => {
            try_get_external_handle(env, &unsafe { source.cast::<JsObject>() })
        }
        _ => None,
    }
    .filter(|p| !p.is_null())
    .ok_or_else(|| err("xamlHostCreate: source is not a DesktopWindowXamlSource"))?;
    let source_unknown = unsafe { IUnknown::from_raw_borrowed(&source_ptr) }
        .cloned()
        .ok_or_else(|| err("xamlHostCreate: source is null"))?;
    let native = SourceNative::from_source(&source_unknown).map_err(|e| {
        err(format!(
            "xamlHostCreate: source is not a DesktopWindowXamlSource: {e}"
        ))
    })?;

    let mut style_name = String::from("dialog");
    let (mut owner, mut title, mut topmost, mut dark) = (None, String::new(), false, false);
    if let Some(o) = &options {
        style_name = opt_prop(o, "style", read_string)?.unwrap_or(style_name);
        if o.has_named_property("owner")? {
            owner = hwnd_from_js(env, &o.get_named_property::<JsUnknown>("owner")?)?;
        }
        title = opt_prop(o, "title", read_string)?.unwrap_or_default();
        topmost = opt_prop(o, "topmost", read_bool)?.unwrap_or(false);
        dark = opt_prop(o, "dark", read_bool)?.unwrap_or(false);
    }
    let popup = match style_name.as_str() {
        "dialog" => false,
        "popup" => true,
        other => return Err(err(format!("xamlHostCreate: unknown style '{other}'"))),
    };

    let (style, mut ex_style) = if popup {
        (WS_POPUP | WS_CLIPCHILDREN, WS_EX_TOOLWINDOW)
    } else {
        (
            WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
            WS_EX_DLGMODALFRAME,
        )
    };
    if topmost {
        ex_style |= WS_EX_TOPMOST;
    }

    let hwnd = unsafe {
        let hinstance =
            GetModuleHandleW(None).map_err(|e| err(format!("GetModuleHandleW failed: {e}")))?;
        CreateWindowExW(
            ex_style,
            CLASS_NAME,
            &windows::core::HSTRING::from(title.as_str()),
            style,
            0,
            0,
            400,
            300,
            owner,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .map_err(|e| err(format!("CreateWindowExW failed: {e}")))?;

    if dark {
        set_dwm_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    }
    if popup {
        set_dwm_u32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, 2); // DWMWCP_ROUND
    }

    let island = match native.attach(hwnd) {
        Ok(island) => island,
        Err(e) => {
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(err(format!("DesktopWindowXamlSource attach failed: {e}")));
        }
    };
    HOSTS.with(|h| {
        h.borrow_mut().insert(
            key(hwnd),
            Host {
                island,
                source: native,
                listener: None,
            },
        )
    });
    install_key_hook();

    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let _ = unsafe {
        SetWindowPos(
            island,
            None,
            0,
            0,
            rc.right,
            rc.bottom,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    let _ = unsafe { ShowWindow(island, SW_SHOW) };
    Ok(hwnd.0 as isize as f64)
}

fn host_hwnd(env: &Env, v: &JsUnknown, what: &str) -> napi::Result<HWND> {
    let hwnd = required_hwnd(env, v, what)?;
    if HOSTS.with(|h| h.borrow().contains_key(&key(hwnd))) {
        Ok(hwnd)
    } else {
        Err(err(format!("{what}: not a live XAML host window")))
    }
}

/// Set the listener for `"activate"`, `"deactivate"`, `"close"`, `"closed"`; `null` removes it.
pub fn set_listener(env: &Env, hwnd: &JsUnknown, func: Option<JsFunction>) -> napi::Result<()> {
    let hwnd = host_hwnd(env, hwnd, "xamlHostSetListener")?;
    let listener = match func {
        Some(f) => {
            let mut r: sys::napi_ref = std::ptr::null_mut();
            let status = unsafe { sys::napi_create_reference(env.raw(), f.raw(), 1, &mut r) };
            if status != sys::Status::napi_ok {
                return Err(err("xamlHostSetListener: could not reference the function"));
            }
            Some(Listener {
                env: env.raw(),
                func: r,
            })
        }
        None => None,
    };
    let old = HOSTS.with(|h| {
        let mut h = h.borrow_mut();
        h.get_mut(&key(hwnd))
            .and_then(|host| std::mem::replace(&mut host.listener, listener))
    });
    if let Some(old) = old {
        unsafe { sys::napi_delete_reference(old.env, old.func) };
    }
    Ok(())
}

fn monitor_work_area(monitor: HMONITOR) -> RECT {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetMonitorInfoW(monitor, &mut info) };
    info.rcWork
}

/// Size the client area in DIPs; center over `over`, or on the pointer's monitor at `y` (0..1).
pub fn place(env: &Env, hwnd: &JsUnknown, options: JsObject) -> napi::Result<()> {
    let hwnd = host_hwnd(env, hwnd, "xamlHostPlace")?;
    let width = opt_prop(&options, "width", read_f64)?.unwrap_or(400.0);
    let height = opt_prop(&options, "height", read_f64)?.unwrap_or(300.0);
    let y_fraction = opt_prop(&options, "y", read_f64)?
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    let over = if options.has_named_property("over")? {
        hwnd_from_js(env, &options.get_named_property::<JsUnknown>("over")?)?
            .filter(|w| unsafe { IsWindow(Some(*w)) }.as_bool())
    } else {
        None
    };

    let monitor = match over {
        Some(w) => unsafe { MonitorFromWindow(w, MONITOR_DEFAULTTONEAREST) },
        None => {
            let mut pt = POINT::default();
            let _ = unsafe { GetCursorPos(&mut pt) };
            unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) }
        }
    };
    let (mut dpi, mut dpi_y) = (96u32, 96u32);
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) };
    let scale = dpi as f64 / 96.0;

    let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32);
    let ex_style = WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32);
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: (width * scale).round() as i32,
        bottom: (height * scale).round() as i32,
    };
    let _ = unsafe { AdjustWindowRectExForDpi(&mut frame, style, false, ex_style, dpi) };
    let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);

    let work = monitor_work_area(monitor);
    let area = match over {
        Some(w) => {
            let mut r = RECT::default();
            let _ = unsafe { GetWindowRect(w, &mut r) };
            r
        }
        None => work,
    };
    let mut x = area.left + (area.right - area.left - fw) / 2;
    let mut y = match over {
        Some(_) => area.top + (area.bottom - area.top - fh) / 2,
        None => area.top + ((area.bottom - area.top - fh) as f64 * y_fraction) as i32,
    };
    x = x.clamp(work.left, (work.right - fw).max(work.left));
    y = y.clamp(work.top, (work.bottom - fh).max(work.top));
    unsafe { SetWindowPos(hwnd, None, x, y, fw, fh, SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| err(format!("SetWindowPos failed: {e}")))
}

/// Show the window; with `activate` it takes the foreground and keyboard focus.
pub fn show(env: &Env, hwnd: &JsUnknown, activate: bool) -> napi::Result<bool> {
    let hwnd = host_hwnd(env, hwnd, "xamlHostShow")?;
    unsafe {
        if !activate {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            return Ok(true);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let foreground = SetForegroundWindow(hwnd).as_bool();
        if let Some(island) = island_of(hwnd) {
            let _ = SetFocus(Some(island));
        }
        Ok(foreground)
    }
}

pub fn hide(env: &Env, hwnd: &JsUnknown) -> napi::Result<()> {
    let hwnd = host_hwnd(env, hwnd, "xamlHostHide")?;
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    Ok(())
}

pub fn is_visible(env: &Env, hwnd: &JsUnknown) -> napi::Result<bool> {
    let hwnd = host_hwnd(env, hwnd, "xamlHostIsVisible")?;
    Ok(unsafe { IsWindowVisible(hwnd) }.as_bool())
}

/// Destroy the window; its listener still receives `"closed"`.
pub fn destroy(env: &Env, hwnd: &JsUnknown) -> napi::Result<()> {
    let Some(hwnd) = hwnd_from_js(env, hwnd)? else {
        return Ok(());
    };
    if HOSTS.with(|h| h.borrow().contains_key(&key(hwnd))) {
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    Ok(())
}

pub fn foreground_window() -> f64 {
    unsafe { GetForegroundWindow() }.0 as isize as f64
}

/// Bring `hwnd` to the foreground, if Windows lets this process do so.
pub fn activate_window(env: &Env, hwnd: &JsUnknown) -> napi::Result<bool> {
    let Some(hwnd) = hwnd_from_js(env, hwnd)? else {
        return Ok(false);
    };
    Ok(unsafe { IsWindow(Some(hwnd)).as_bool() && SetForegroundWindow(hwnd).as_bool() })
}

pub fn enable_window(env: &Env, hwnd: &JsUnknown, enabled: bool) -> napi::Result<()> {
    let hwnd = required_hwnd(env, hwnd, "enableWindow")?;
    let _ = unsafe { EnableWindow(hwnd, enabled) };
    Ok(())
}

pub fn window_process_id(env: &Env, hwnd: &JsUnknown) -> napi::Result<u32> {
    let Some(hwnd) = hwnd_from_js(env, hwnd)? else {
        return Ok(0);
    };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Ok(pid)
}
