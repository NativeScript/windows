//! A plain Win32 window, for hosting a `Windows.UI.Composition` visual tree from a console host.
//!
//! `Windows.UI.Xaml` needs an actual XAML-initialized thread to activate (see
//! `windows-napi/test/composable-test.js`): not available headless. `Windows.UI.Composition`
//! has no such requirement (already proven headless in that same test file: `Compositor`,
//! `SpriteVisual`, `CompositionColorBrush` all construct and wire up fine) but a compositor needs
//! a real HWND to render onto. This module supplies just that HWND via `ICompositorDesktopInterop`,
//! with no XAML, no package identity, nothing else new. `runtime::pump_messages()` (already used by
//! `enableAutoPump()`/`toPromise` for WinRT async completions) pumps its messages for free: it
//! peeks messages for the whole calling thread, not a specific window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;

use napi::{Env, JsObject, JsUnknown};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, GetClientRect, IsWindow, LoadCursorW,
    RegisterClassExW, SetForegroundWindow, SetWindowTextW, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    IDC_ARROW, SW_SHOW, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN,
    WM_SIZE, WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
};

use crate::error::{generic_error, AnyError};
use crate::napi_engine::value::{external_from_ptr, napi_parse_pointer, ptr_from_external};

const CLASS_NAME: PCWSTR = windows::core::w!("NsWinRTCompositionWindow");

/// One input/lifecycle event, queued by `wnd_proc` and drained by [`poll_window_events`].
/// Coordinates are client-area pixels. The same space a `DesktopWindowTarget`'s root visual
/// uses, so JS can place visuals at pointer positions without conversion.
#[derive(Clone, Copy)]
enum WindowEvent {
    PointerDown { x: i32, y: i32, button: u32 },
    PointerUp { x: i32, y: i32 },
    PointerMove { x: i32, y: i32 },
    Resize { width: i32, height: i32 },
    Close,
}

thread_local! {
    // Keyed by HWND. Window messages are dispatched on the thread that created the window (the
    // JS thread, inside `pump_messages`), so a thread-local needs no locking. Events are polled
    // rather than delivered as JS callbacks: `wnd_proc` runs nested inside PeekMessage/
    // DispatchMessage, and calling back into JS from there is re-entrancy every engine handles
    // differently.
    static EVENTS: RefCell<HashMap<isize, Vec<WindowEvent>>> = RefCell::new(HashMap::new());
}

fn push_event(hwnd: HWND, event: WindowEvent) {
    EVENTS.with(|m| {
        let mut m = m.borrow_mut();
        let Some(queue) = m.get_mut(&(hwnd.0 as isize)) else {
            return;
        };
        // Coalesce moves: a poll wants the latest pointer position, not every intermediate one.
        if let (WindowEvent::PointerMove { .. }, Some(WindowEvent::PointerMove { .. })) =
            (&event, queue.last())
        {
            queue.pop();
        }
        queue.push(event);
    });
}

fn point_from_lparam(lparam: LPARAM) -> (i32, i32) {
    let v = lparam.0 as u32;
    ((v & 0xFFFF) as u16 as i16 as i32, (v >> 16) as u16 as i16 as i32)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            let (x, y) = point_from_lparam(lparam);
            let button = if msg == WM_LBUTTONDOWN { 0 } else { 2 };
            push_event(hwnd, WindowEvent::PointerDown { x, y, button });
        }
        WM_LBUTTONUP => {
            let (x, y) = point_from_lparam(lparam);
            push_event(hwnd, WindowEvent::PointerUp { x, y });
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_from_lparam(lparam);
            push_event(hwnd, WindowEvent::PointerMove { x, y });
        }
        WM_SIZE => {
            let (width, height) = point_from_lparam(lparam);
            push_event(hwnd, WindowEvent::Resize { width, height });
        }
        // DefWindowProc's WM_CLOSE already destroys the window (title-bar X just works); record
        // the destroy so JS can end its loop instead of polling a dead handle.
        WM_DESTROY => push_event(hwnd, WindowEvent::Close),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn register_class_once() -> Result<(), AnyError> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = RESULT.get_or_init(|| unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => return Err(format!("GetModuleHandleW failed: {e}")),
        };
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            hCursor: cursor,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            Err("RegisterClassExW returned 0".to_string())
        } else {
            Ok(())
        }
    });
    result.clone().map_err(generic_error)
}

/// Create and show a top-level window. Returns the `HWND`, boxed as an opaque `External`. The
/// same "pointer travels as an External" convention used elsewhere in this layer (see
/// `external_from_ptr`), so it round-trips through JS without exposing raw pointer arithmetic.
pub fn create_window(env: &Env, title: &str, width: i32, height: i32) -> napi::Result<JsUnknown> {
    register_class_once().map_err(|e| napi::Error::from_reason(e.to_string()))?;
    let title_w = windows::core::HSTRING::from(title);
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| {
            napi::Error::from_reason(format!("GetModuleHandleW failed: {e}"))
        })?;
        CreateWindowExW(
            // No GDI redirection bitmap: without this the window keeps its own (white) backing
            // surface on top of anything DirectComposition renders, so a Composition visual
            // tree attached via CreateDesktopWindowTarget never becomes visible.
            WS_EX_NOREDIRECTIONBITMAP,
            CLASS_NAME,
            &title_w,
            WS_OVERLAPPEDWINDOW,
            // Fixed position rather than CW_USEDEFAULT: Windows cascades CW_USEDEFAULT windows
            // further down/right with each one created *by this process this session*. Fine
            // for one window, but a demo host that creates many across repeated runs eventually
            // cascades new ones off-screen with nothing on-screen to show it happened.
            100,
            100,
            width,
            height,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .map_err(|e| napi::Error::from_reason(format!("CreateWindowExW failed: {e}")))?;
    EVENTS.with(|m| m.borrow_mut().insert(hwnd.0 as isize, Vec::new()));
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // A window created by a background/non-foreground process (a script launched from a
        // task runner, CI, or, as hit while building this demo, a background shell tool) does
        // not reliably receive foreground z-order on its own; Windows' focus-stealing prevention
        // leaves it behind whatever the actual foreground app is, with nothing on screen to show
        // it happened (the window is otherwise created and rendering into correctly). Force it.
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
    }
    external_from_ptr(env, hwnd.0)
        .map_err(|e| napi::Error::from_reason(format!("failed to box HWND: {e}")))
}

fn hwnd_arg(env: &Env, hwnd_external: &JsUnknown, what: &str) -> napi::Result<HWND> {
    ptr_from_external(env, hwnd_external)
        .map(HWND)
        .ok_or_else(|| napi::Error::from_reason(format!("{what}: hwnd argument is not a window handle")))
}

/// Drain the events queued for a window from [`create_window`] since the last poll, as
/// `{ type, … }` objects: `pointerdown {x, y, button}`, `pointerup {x, y}`, `pointermove {x, y}`
/// (coalesced to the latest position), `resize {width, height}` and `close`. Messages are only
/// dispatched while `pumpMessages` runs, so poll right after pumping.
pub fn poll_window_events(env: &Env, hwnd_external: &JsUnknown) -> napi::Result<JsObject> {
    let hwnd = hwnd_arg(env, hwnd_external, "pollWindowEvents")?;
    let events = EVENTS.with(|m| {
        let mut m = m.borrow_mut();
        let drained = m.get_mut(&(hwnd.0 as isize)).map(std::mem::take).unwrap_or_default();
        if drained.iter().any(|e| matches!(e, WindowEvent::Close)) {
            m.remove(&(hwnd.0 as isize));
        }
        drained
    });
    let mut arr = env.create_array_with_length(events.len())?;
    for (i, event) in events.iter().enumerate() {
        let mut obj = env.create_object()?;
        let mut set = |k: &str, v: i32| -> napi::Result<()> { obj.set_named_property(k, env.create_int32(v)?) };
        let ty = match *event {
            WindowEvent::PointerDown { x, y, button } => {
                set("x", x)?;
                set("y", y)?;
                set("button", button as i32)?;
                "pointerdown"
            }
            WindowEvent::PointerUp { x, y } => {
                set("x", x)?;
                set("y", y)?;
                "pointerup"
            }
            WindowEvent::PointerMove { x, y } => {
                set("x", x)?;
                set("y", y)?;
                "pointermove"
            }
            WindowEvent::Resize { width, height } => {
                set("width", width)?;
                set("height", height)?;
                "resize"
            }
            WindowEvent::Close => "close",
        };
        obj.set_named_property("type", env.create_string(ty)?)?;
        arr.set_element(i as u32, obj)?;
    }
    Ok(arr)
}

/// Client-area size of a window from [`create_window`] as `{ width, height }`, or `null` once
/// the window has been closed.
pub fn window_client_size(env: &Env, hwnd_external: &JsUnknown) -> napi::Result<JsUnknown> {
    let hwnd = hwnd_arg(env, hwnd_external, "getWindowSize")?;
    let mut rect = RECT::default();
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() || unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return Ok(env.get_null()?.into_unknown());
    }
    let mut obj = env.create_object()?;
    obj.set_named_property("width", env.create_int32(rect.right - rect.left)?)?;
    obj.set_named_property("height", env.create_int32(rect.bottom - rect.top)?)?;
    Ok(obj.into_unknown())
}

pub fn set_window_title(env: &Env, hwnd_external: &JsUnknown, title: &str) -> napi::Result<()> {
    let hwnd = hwnd_arg(env, hwnd_external, "setWindowTitle")?;
    unsafe { SetWindowTextW(hwnd, &windows::core::HSTRING::from(title)) }
        .map_err(|e| napi::Error::from_reason(format!("SetWindowTextW failed: {e}")))
}

/// Attach a `Windows.UI.Composition.Compositor` instance to a window created by [`create_window`]
/// and return the resulting `Windows.UI.Composition.Desktop.DesktopWindowTarget` as a normal
/// WinRT proxy: set its `.Root` to a visual, same as any other Composition property, to render
/// into the window.
pub fn attach_compositor_to_window(
    env: &Env,
    compositor: &JsUnknown,
    hwnd_external: &JsUnknown,
) -> napi::Result<JsObject> {
    let compositor_ptr = unsafe {
        napi_parse_pointer(env, compositor)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?
            .pointer
    };
    if compositor_ptr.is_null() {
        return Err(napi::Error::from_reason(
            "attachCompositorToWindow: compositor argument is not a WinRT object",
        ));
    }
    let hwnd = hwnd_arg(env, hwnd_external, "attachCompositorToWindow")?;

    let compositor_unknown: windows::core::IUnknown = unsafe {
        let borrowed = std::mem::ManuallyDrop::new(windows::core::IUnknown::from_raw(
            compositor_ptr as *mut c_void,
        ));
        (*borrowed).clone()
    };
    let interop: ICompositorDesktopInterop = compositor_unknown
        .cast()
        .map_err(|e| napi::Error::from_reason(format!("compositor is not desktop-capable: {e}")))?;
    let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false) }
        .map_err(|e| napi::Error::from_reason(format!("CreateDesktopWindowTarget failed: {e}")))?;
    let raw = target.as_raw();
    std::mem::forget(target); // ownership moves into the wrapped proxy below

    crate::napi_engine::ns_proxy::try_wrap_inspectable_pointer(env, raw)
        .ok_or_else(|| napi::Error::from_reason("failed to wrap DesktopWindowTarget"))
}
