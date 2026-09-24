use crate::Runtime;

/// Ensure logging WinRT `Windows.UI.Color` values does not crash the runtime.
#[test]
fn test_logging_windows_ui_color_does_not_panic() {
    let mut runtime = Runtime::new(".");

    // Instance-returning struct via factory helper
    runtime.run_script(
        "console.log('red', Windows.UI.ColorHelper.FromArgb(255, 255, 0, 0));",
        "color_fromargb_test.js",
    );

    // Static color property (predefined struct value)
    runtime.run_script(
        "console.log('green', Windows.UI.Colors.Green);",
        "color_static_green_test.js",
    );

    if let Some(err) = crate::get_last_js_error() {
        panic!("JS error during color logging test: {}", err);
    }
}

/// `Compositor.CreateColorBrush(Color)` is the `[Overload("CreateColorBrushWithColor")]` sibling
/// of the 0-arg `CreateColorBrush()`. Calling the public name with a color must reach the 1-arg
/// overload, not silently drop the argument (transparent black); the overload name keeps working.
#[test]
fn test_public_overload_name_dispatches_on_arity() {
    // Compositor needs a DispatcherQueue, which needs an STA thread; libtest reuses pool threads
    // other tests may already have joined to the MTA, so run on a fresh one.
    std::thread::spawn(overload_arity_body).join().unwrap();
}

fn overload_arity_body() {
    use windows::Win32::System::WinRT::{
        CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
    };
    // init_ui_dispatcher only ever creates one process-wide queue, so give this thread its own.
    let _controller = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })
    }
    .expect("CreateDispatcherQueueController");
    let mut runtime = Runtime::new(".");
    runtime.run_script(
        r#"
        const c = new Windows.UI.Composition.Compositor();
        const want = { A: 255, R: 40, G: 120, B: 220 };
        const same = (x) => x.A === want.A && x.R === want.R && x.G === want.G && x.B === want.B;
        const viaPublic = c.CreateColorBrush(want).Color;
        if (!same(viaPublic)) throw new Error('CreateColorBrush(color) → ' + JSON.stringify(viaPublic));
        const viaAlias = c.CreateColorBrushWithColor(want).Color;
        if (!same(viaAlias)) throw new Error('CreateColorBrushWithColor(color) → ' + JSON.stringify(viaAlias));
        const zero = c.CreateColorBrush().Color;
        if (zero.A !== 0) throw new Error('CreateColorBrush() → ' + JSON.stringify(zero));
        "#,
        "overload_arity_test.js",
    );
    if let Some(err) = crate::get_last_js_error() {
        panic!("JS error during overload dispatch test: {}", err);
    }
}
