mod interop;
mod sampler;

use crate::interop::create_dispatcher_queue_controller_for_current_thread;
use std::env;
use std::ffi::CString;
use std::fs;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_SINGLETHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

fn script_to_run() -> (String, String) {
    let app_root = env::var("PLAYGROUND_APP_ROOT").unwrap_or_else(|_| String::new());

    if let Ok(script_path) = env::var("PLAYGROUND_SCRIPT_PATH") {
        let script = fs::read_to_string(&script_path).unwrap_or_else(|e| {
            panic!(
                "Failed to read PLAYGROUND_SCRIPT_PATH '{}': {}",
                script_path, e
            )
        });
        let resolved_root = if app_root.is_empty() {
            std::path::Path::new(&script_path)
                .parent()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            app_root
        };
        return (resolved_root, script);
    }

    let script = r#"
        console.log('--- NativeScript on Windows (console demo) ---');
        console.log('performance.now() =', performance.now());
        console.log('typeof Windows =', typeof Windows);

        const uri = new Windows.Foundation.Uri("http://www.bing.com/");
        console.log('AbsoluteUri:', uri.AbsoluteUri);

        console.dir(Windows.UI.Popups.Placement);
console.log('Default', Windows.UI.Popups.Placement.Default, Windows.UI.Popups.Placement.Default === 0);
console.log('Right', Windows.UI.Popups.Placement.Right, Windows.UI.Popups.Placement.Right === 4);
const json = new Windows.Data.Json.JsonObject();
const method = new Windows.Web.Http.HttpMethod('GET');
console.log(method);

    var newGuid = Windows.Foundation.GuidHelper.CreateNewGuid();
    console.log(newGuid);
    "#;

    (app_root, script.to_string())
}

fn run_js_app() {
    // Initialize WinRT in STA mode. UI APIs like MessageDialog require an
    // apartment-threaded context, and RO_INIT_SINGLETHREADED creates a proper
    // ASTA so the shell can show dialogs from a console app.
    unsafe { RoInitialize(RO_INIT_SINGLETHREADED).expect("RoInitialize failed") };

    // A DispatcherQueue on this thread is what WinRT async APIs (e.g. ShowAsync)
    // hand their completion callbacks back to. Held alive for the program's life.
    let _controller = create_dispatcher_queue_controller_for_current_thread()
        .expect("DispatcherQueueController creation failed");

    // The runtime returns native WinRT objects directly — including
    // IAsyncOperation. JS code wraps it in a Promise itself.
    let (app_root, script) = script_to_run();
    let app_root_cstr = CString::new(app_root).unwrap();
    let rt = nativescript::runtime_init(app_root_cstr.as_ptr());
    let cscript = CString::new(script).unwrap();
    let sampler = sampler::start_if_requested();
    nativescript::runtime_runscript(rt, cscript.as_ptr(), std::ptr::null());
    if let Some(sampler) = sampler {
        sampler.finish();
    }
    nativescript::runtime_deinit(rt);

    unsafe { RoUninitialize() };
}

fn main() {
    nativescript::runtime_install_ctrlc_handler(0);
    run_js_app();
}
