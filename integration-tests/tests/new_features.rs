use runtime::Runtime;

// ── helper ───────────────────────────────────────────────────────────────────

/// Evaluate `expr` in `rt`, assert the result is `"true"`.
/// Uses eval_script_to_string so JS exceptions propagate as test failures.
fn assert_js(rt: &mut Runtime, expr: &str, msg: &str) {
    match rt.eval_script_to_string(expr) {
        Some(ref v) if v.trim() == "true" => {}
        Some(v) => panic!("{msg}: expression evaluated to {v:?} (expected \"true\")"),
        None => panic!("{msg}: JS exception thrown"),
    }
}

/// Evaluate `expr` in `rt` and return the string representation.
fn eval(rt: &mut Runtime, expr: &str) -> String {
    rt.eval_script_to_string(expr)
        .unwrap_or_else(|| "<eval failed>".to_string())
}

// ── __time / performance.now ─────────────────────────────────────────────────

#[test]
fn time_returns_positive_milliseconds() {
    let mut rt = Runtime::new(".");
    assert_js(&mut rt, "__time() > 0", "expected __time() > 0");
}

#[test]
fn time_is_monotonically_non_decreasing() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        "(function(){ var a = __time(); var b = __time(); return b >= a; })()",
        "expected __time monotonic",
    );
}

#[test]
fn performance_now_is_positive() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        "performance.now() > 0",
        "expected performance.now() > 0",
    );
}

#[test]
fn performance_now_agrees_with_time() {
    let mut rt = Runtime::new(".");
    // Both are ms since process start; they should be within 10ms of each other.
    assert_js(&mut rt,
        "(function(){ var t = __time(); var p = performance.now(); return Math.abs(t - p) < 10; })()",
        "performance.now and __time diverge by more than 10ms");
}

// ── runtimeVersion ───────────────────────────────────────────────────────────

#[test]
fn runtime_version_is_set() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        "typeof __runtimeVersion === 'string' && __runtimeVersion.length > 0",
        "expected __runtimeVersion to be a non-empty string",
    );
}

// ── gc() ─────────────────────────────────────────────────────────────────────

#[test]
fn gc_does_not_throw() {
    let mut rt = Runtime::new(".");
    rt.run_script("gc();", "test.js");
}

// ── URL / URLSearchParams ─────────────────────────────────────────────────────

#[test]
fn url_parse_round_trip() {
    let mut rt = Runtime::new(".");
    assert_js(&mut rt,
        "new URL('https://user:pw@example.com:8080/path?q=1#h').href === 'https://user:pw@example.com:8080/path?q=1#h'",
        "URL href round-trip failed");
}

#[test]
fn url_components_are_correct() {
    let mut rt = Runtime::new(".");
    rt.run_script(
        "var _u = new URL('https://example.com:8080/path?q=1#hash');",
        "setup.js",
    );
    assert_js(&mut rt, "_u.protocol === 'https:'", "protocol");
    assert_js(&mut rt, "_u.hostname === 'example.com'", "hostname");
    assert_js(&mut rt, "_u.port     === '8080'", "port");
    assert_js(&mut rt, "_u.pathname === '/path'", "pathname");
    assert_js(&mut rt, "_u.search   === '?q=1'", "search");
    assert_js(&mut rt, "_u.hash     === '#hash'", "hash");
    assert_js(
        &mut rt,
        "_u.origin   === 'https://example.com:8080'",
        "origin",
    );
}

#[test]
fn url_search_params_basic() {
    let mut rt = Runtime::new(".");
    rt.run_script(
        "var _p = new URLSearchParams('a=1&b=hello%20world');",
        "setup.js",
    );
    assert_js(&mut rt, "_p.get('a') === '1'", "URLSearchParams a");
    assert_js(
        &mut rt,
        "_p.get('b') === 'hello world'",
        "URLSearchParams b decode",
    );
    assert_js(&mut rt, "_p.has('a') === true", "URLSearchParams has a");
    assert_js(&mut rt, "_p.has('z') === false", "URLSearchParams no z");
}

#[test]
fn url_relative_resolution() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        "new URL('/other', 'https://example.com/page').href === 'https://example.com/other'",
        "relative URL resolution",
    );
}

// ── requestAnimationFrame ─────────────────────────────────────────────────────

#[test]
fn raf_fires_callback_with_positive_timestamp() {
    let mut rt = Runtime::new(".");
    // What an app host does after creating the runtime; the frame pump finds the isolate here.
    rt.register_delegate_isolate_ptr();
    // Register the callback.
    rt.run_script(
        r#"
        var _rafFired = false;
        var _rafTs    = -1;
        requestAnimationFrame(function(ts) {
            _rafFired = true;
            _rafTs    = ts;
        });
    "#,
        "setup.js",
    );

    // One frame: the host's pump (runtime_pump_timers) runs requested callbacks.
    assert!(runtime::animation_frames::pump(), "no frame was requested");

    assert_js(&mut rt, "_rafFired === true", "rAF callback not fired");
    assert_js(&mut rt, "_rafTs    >= 0", "rAF timestamp negative");
}

#[test]
fn raf_callback_receives_increasing_timestamps() {
    let mut rt = Runtime::new(".");
    // What an app host does after creating the runtime; the frame pump finds the isolate here.
    rt.register_delegate_isolate_ptr();
    rt.run_script(
        r#"
        var _ts1 = -1, _ts2 = -1;
        requestAnimationFrame(function(t1) {
            _ts1 = t1;
            requestAnimationFrame(function(t2) { _ts2 = t2; });
        });
    "#,
        "setup.js",
    );
    runtime::animation_frames::pump();
    // Requested during the first frame: runs in the second, not the first.
    assert_js(&mut rt, "_ts2 === -1", "nested rAF ran in the same frame");
    runtime::animation_frames::pump();
    assert_js(&mut rt, "_ts1 >= 0", "first rAF timestamp invalid");
    assert_js(&mut rt, "_ts2 >= _ts1", "second rAF not >= first");
}

#[test]
fn raf_does_not_run_without_a_frame() {
    let mut rt = Runtime::new(".");
    // What an app host does after creating the runtime; the frame pump finds the isolate here.
    rt.register_delegate_isolate_ptr();
    rt.run_script(
        r#"
        var _early = false;
        requestAnimationFrame(function() { _early = true; });
        Promise.resolve().then(function() {});
    "#,
        "setup.js",
    );
    // Microtasks drained, no pump yet: the UI thread was never held for a frame.
    assert_js(&mut rt, "_early === false", "rAF ran before the frame");
    runtime::animation_frames::pump();
    assert_js(&mut rt, "_early === true", "rAF did not run on the frame");
}

#[test]
fn cancel_raf_prevents_callback() {
    let mut rt = Runtime::new(".");
    // What an app host does after creating the runtime; the frame pump finds the isolate here.
    rt.register_delegate_isolate_ptr();
    rt.run_script(
        r#"
        var _called = false;
        var _id = requestAnimationFrame(function() { _called = true; });
        cancelAnimationFrame(_id);
    "#,
        "setup.js",
    );
    runtime::animation_frames::pump();
    assert_js(
        &mut rt,
        "_called === false",
        "cancelAnimationFrame did not cancel",
    );
}

// ── setTimeout / setInterval ─────────────────────────────────────────────────
// These rely on DispatcherTimer which requires a WinRT UI context.
// We mark them as ignored in headless CI and only verify the API exists.

#[test]
fn settimeout_exists_and_returns_id() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        "typeof __ns__setTimeout === 'function' || typeof setTimeout === 'function'",
        "setTimeout not a function",
    );
    assert_js(
        &mut rt,
        "typeof __ns__clearTimeout === 'function' || typeof clearTimeout === 'function'",
        "clearTimeout not a function",
    );
    assert_js(
        &mut rt,
        "typeof __ns__setInterval === 'function' || typeof setInterval === 'function'",
        "setInterval not a function",
    );
    assert_js(
        &mut rt,
        "typeof __ns__clearInterval === 'function' || typeof clearInterval === 'function'",
        "clearInterval not a function",
    );
}

// ── Win32 FFI ─────────────────────────────────────────────────────────────────

#[test]
fn win32_call_kernel32_get_tick_count() {
    let mut rt = Runtime::new(".");
    // GetTickCount64 lives in kernel32.dll, returns u64 (ms since boot).
    let result = eval(
        &mut rt,
        r#"
        NSWinRT.win32.call('kernel32.dll', 'GetTickCount64', 'u64')
    "#,
    );
    let v: u64 = result.trim().parse().unwrap_or(0);
    assert!(v > 0, "GetTickCount64 returned 0 or failed: {result}");
}

#[test]
fn win32_bind_pattern() {
    let mut rt = Runtime::new(".");
    let result = eval(
        &mut rt,
        r#"
        var kernel32 = NSWinRT.win32.bind('kernel32.dll', 'u64');
        var t = kernel32.GetTickCount64();
        typeof t === 'number' && t > 0
    "#,
    );
    assert_eq!(result, "true", "win32.bind().GetTickCount64() failed");
}

#[test]
fn win32_exports_returns_array() {
    let mut rt = Runtime::new(".");
    assert_js(&mut rt,
        "(function(){ var ex = JSON.parse(__nsWin32Exports('kernel32.dll')); return Array.isArray(ex) && ex.length > 0; })()",
        "kernel32.dll exports should be a non-empty array");
}

#[test]
fn win32_exports_contains_known_function() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        "JSON.parse(__nsWin32Exports('kernel32.dll')).indexOf('GetTickCount64') >= 0",
        "GetTickCount64 not in kernel32.dll exports",
    );
}

#[test]
fn win32_import_installs_global() {
    let mut rt = Runtime::new(".");
    // After import(), DLL exports become plain JS globals — call GetTickCount64() directly.
    let result = eval(
        &mut rt,
        r#"
        NSWinRT.win32.import('kernel32.dll', 'u64');
        var t = GetTickCount64();
        typeof t === 'number' && t > 0
    "#,
    );
    assert_eq!(
        result, "true",
        "win32.import() + direct GetTickCount64() call failed"
    );
}

// ── .NET BCL (skipped when bridge DLL is not published) ──────────────────────

fn dotnet_bridge_available() -> bool {
    std::path::PathBuf::from(".")
        .join("dotnet-bridge")
        .join("publish")
        .join("DotNetBridge.dll")
        .exists()
}

#[test]
fn dotnet_stopwatch_start_new_via_natural_namespace() {
    if !dotnet_bridge_available() {
        eprintln!("SKIP: dotnet-bridge not published — run `dotnet publish` in dotnet-bridge/");
        return;
    }
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var sw = System.Diagnostics.Stopwatch.StartNew();
            return typeof sw === 'object' && sw.__handle != null;
        })()
    "#,
        "Stopwatch.StartNew() should return a DotNetObject",
    );
}

#[test]
fn winrt_jsonarray_indexof_out_wrapper() {
    let mut rt = Runtime::new(".");

    // Exercise IndexOf with omitted out arg, undefined, and explicit out wrapper.
    // JsonArray is non-UI, so this runs in headless CI.
    let script = r#"
        (function(){
            try {
                var collection = null;
                var item = null;

                if (typeof Windows !== 'undefined' && Windows.Data && Windows.Data.Json && typeof Windows.Data.Json.JsonArray === 'function' && typeof Windows.Data.Json.JsonValue === 'function') {
                    try {
                        collection = new Windows.Data.Json.JsonArray();
                        item = Windows.Data.Json.JsonValue.CreateStringValue('Test');
                        if (collection && typeof collection.Append === 'function' && typeof collection.IndexOf === 'function') {
                            collection.Append(item);
                        } else {
                            collection = null; item = null;
                        }
                    } catch (e) {
                        collection = null; item = null;
                    }
                }

                if (!collection) {
                    return JSON.stringify({available:false});
                }

                var r1 = collection.IndexOf(item);
                var found1 = false, idx1 = -1;
                if (Array.isArray(r1)) { found1 = !!r1[0]; idx1 = r1[1]; } else { found1 = !!r1; }

                var r2 = collection.IndexOf(item, undefined);
                var found2 = false, idx2 = -1;
                if (Array.isArray(r2)) { found2 = !!r2[0]; idx2 = r2[1]; } else { found2 = !!r2; }

                var out = NSWinRT.interop.out('Int32');
                var r3 = collection.IndexOf(item, out);
                var found3 = Array.isArray(r3) ? !!r3[0] : !!r3;
                var outv = out && out.value;

                return JSON.stringify({
                    available: true,
                    found1: !!found1, idx1: (typeof idx1 === 'number' ? idx1 : null),
                    found2: !!found2, idx2: (typeof idx2 === 'number' ? idx2 : null),
                    found3: !!found3, r3IsArray: Array.isArray(r3),
                    outvType: (typeof outv), outv: outv
                });
            } catch (e) {
                return JSON.stringify({ error: String(e) });
            }
        })()
    "#;

    let result = eval(&mut rt, script);
    if result.contains("\"available\":false") {
        eprintln!("SKIP: JsonArray not available: {}", result);
        return;
    }
    if result.contains("marshalled for a different thread")
        || result.contains("ActivateInstance failed")
        || result.contains("apartment")
    {
        eprintln!(
            "SKIP: JsonArray activation failed due to apartment state: {}",
            result
        );
        return;
    }

    assert!(
        result.contains("\"available\":true"),
        "JsonArray APIs not available: {}",
        result
    );
    assert!(
        result.contains("\"found1\":true"),
        "IndexOf omitted-out failed: {}",
        result
    );
    assert!(
        result.contains("\"found2\":true"),
        "IndexOf undefined-out failed: {}",
        result
    );
    assert!(
        result.contains("\"found3\":true"),
        "IndexOf wrapper-out failed: {}",
        result
    );
    assert!(
        result.contains("\"r3IsArray\":false"),
        "Explicit out wrapper should not return a tuple: {}",
        result
    );
    assert!(
        result.contains("\"outvType\":\"number\""),
        "Out wrapper type not number: {}",
        result
    );
    assert!(
        result.contains("\"outv\":0"),
        "Out wrapper value should be index 0: {}",
        result
    );
}

#[test]
fn dotnet_stopwatch_elapsed_is_numeric() {
    if !dotnet_bridge_available() {
        return;
    }
    let mut rt = Runtime::new(".");
    // sw.Stop() — natural method call (no .call('Stop'))
    // sw.Elapsed — natural property access (no .get('Elapsed'))
    assert_js(
        &mut rt,
        r#"
        (function(){
            var sw = System.Diagnostics.Stopwatch.StartNew();
            sw.Stop();
            var elapsed = sw.Elapsed;
            return elapsed != null;
        })()
    "#,
        "Stopwatch elapsed should not be null",
    );
}

#[test]
fn dotnet_stringbuilder_constructor() {
    if !dotnet_bridge_available() {
        return;
    }
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var sb = new System.Text.StringBuilder(64);
            return typeof sb === 'object' && sb.__handle != null;
        })()
    "#,
        "new System.Text.StringBuilder() should return a DotNetObject",
    );
}

#[test]
fn dotnet_environment_machine_name_direct_property() {
    if !dotnet_bridge_available() {
        return;
    }
    let mut rt = Runtime::new(".");
    // System.Environment.MachineName — static property accessed directly (no get_MachineName())
    assert_js(
        &mut rt,
        r#"
        (function(){
            var name = System.Environment.MachineName;
            return typeof name === 'string' && name.length > 0;
        })()
    "#,
        "System.Environment.MachineName should be a non-empty string",
    );
}

#[test]
fn dotnet_environment_get_machine_name() {
    if !dotnet_bridge_available() {
        return;
    }
    let mut rt = Runtime::new(".");
    // Legacy get_ prefix still works for backwards compatibility.
    assert_js(
        &mut rt,
        r#"
        (function(){
            var name = System.Environment.get_MachineName();
            return typeof name === 'string' && name.length > 0;
        })()
    "#,
        "System.Environment.get_MachineName() should still work",
    );
}

// Managed-subclass proxy system (NSWinRT.proxy.createManagedSubclass / Bridge.Proxy.cs's dynamic
// dev-only fallback proxy). dotnet-bridge-tests exercises the C# IL-emission side directly via
// Bridge.DispatchBin, bypassing the JS engine entirely — these run the full real path instead:
// JS -> __nsDotNetCreateJsSubclass -> CreateJsSubclass (dynamic proxy emitted, IL overrides only
// the requested members) -> instance call reflects on the real derived type -> virtual dispatch
// hits the emitted override -> callback fires back into this exact JS closure.
#[test]
fn dotnet_managed_subclass_overridden_method_dispatches_to_js() {
    if !dotnet_bridge_available() {
        return;
    }
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var called = false;
            var obj = NSWinRT.proxy.createManagedSubclass('', 'System.Object', {
                ToString: function () { called = true; return 'js-tostring'; }
            });
            if (!obj || typeof obj !== 'object' || obj.__handle == null) return false;
            var result = obj.ToString();
            return called === true && result === 'js-tostring';
        })()
    "#,
        "overridden ToString() on a managed subclass should dispatch to the JS override",
    );
}

#[test]
fn dotnet_managed_subclass_unoverridden_virtual_falls_back_to_base() {
    if !dotnet_bridge_available() {
        return;
    }
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var called = false;
            // Override GetHashCode only; ToString is left untouched and must run the real
            // System.Object implementation, never asking JS.
            var obj = NSWinRT.proxy.createManagedSubclass('', 'System.Object', {
                GetHashCode: function () { called = true; return 42; }
            });
            if (!obj || typeof obj !== 'object' || obj.__handle == null) return false;
            var result = obj.ToString();
            return called === false && typeof result === 'string' && result.indexOf('System.Object') !== -1;
        })()
    "#,
        "un-overridden virtual on a managed subclass should run the real base implementation",
    );
}

#[test]
fn event_reads_null_before_assignment() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            return ps.MapChanged === null;
        })()
    "#,
        "unset WinRT event should read back as null (not undefined)",
    );
}

#[test]
fn event_reads_assigned_handler() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            var h = function(sender, args){};
            ps.MapChanged = h;
            return ps.MapChanged === h;
        })()
    "#,
        "WinRT event should read back the exact handler that was assigned",
    );
}

#[test]
fn event_reassignment_reflects_latest_handler() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            var first = function(){};
            var second = function(){};
            ps.MapChanged = first;
            ps.MapChanged = second;
            return ps.MapChanged === second && ps.MapChanged !== first;
        })()
    "#,
        "re-assigning a WinRT event should read back the latest handler",
    );
}

#[test]
fn event_fires_handler_on_insert() {
    let mut rt = Runtime::new(".");
    // Delegates reach JS via DELEGATE_ISOLATE_PTR, which the embedding host
    // registers after construction — mirror that here.
    rt.register_delegate_isolate_ptr();
    // PropertySet raises MapChanged synchronously on Insert.
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            var count = 0;
            ps.MapChanged = function(sender, args){ count++; };
            ps.Insert('k1', 1);
            return count === 1;
        })()
    "#,
        "MapChanged handler should fire once per Insert",
    );
}

#[test]
fn event_reassignment_replaces_subscription() {
    let mut rt = Runtime::new(".");
    rt.register_delegate_isolate_ptr();
    // Re-assigning must remove the old subscription (token continuity): after
    // swapping handlers, an Insert fires only the new one, exactly once.
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            var firstCount = 0, secondCount = 0;
            ps.MapChanged = function(){ firstCount++; };
            ps.MapChanged = function(){ secondCount++; };
            ps.Insert('k1', 1);
            return firstCount === 0 && secondCount === 1;
        })()
    "#,
        "re-assignment should unsubscribe the previous handler",
    );
}

#[test]
fn event_null_assignment_unsubscribes() {
    let mut rt = Runtime::new(".");
    rt.register_delegate_isolate_ptr();
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            var count = 0;
            ps.MapChanged = function(){ count++; };
            ps.Insert('k1', 1);
            ps.MapChanged = null;
            ps.Insert('k2', 2);
            return count === 1 && ps.MapChanged === null;
        })()
    "#,
        "assigning null should unsubscribe and read back as null",
    );
}

// Sideloaded winmd metadata (third-party WinRT components like WebView2).

#[test]
fn register_winmd_js_api() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            if (typeof __nsRegisterWinmd !== 'function') return false;
            var ok = __nsRegisterWinmd('C:\\Windows\\System32\\WinMetadata\\Windows.Globalization.winmd') === true;
            var threw = false;
            try { __nsRegisterWinmd('C:\\does\\not\\exist.winmd'); } catch (e) { threw = true; }
            return ok && threw;
        })()
    "#,
        "__nsRegisterWinmd should load real winmds and throw on missing files",
    );
}

#[test]
fn event_supports_in_operator() {
    let mut rt = Runtime::new(".");
    assert_js(
        &mut rt,
        r#"
        (function(){
            var ps = new Windows.Foundation.Collections.PropertySet();
            return ('MapChanged' in ps) && !('NotARealEvent' in ps);
        })()
    "#,
        "'EventName' in instance should be true for declared WinRT events",
    );
}

/// Runs `expr` against the published .NET bridge and asserts it evaluates to `true`; skips when
/// the bridge isn't published.
fn assert_dotnet_js(expr: &str, msg: &str) {
    if !dotnet_bridge_available() {
        eprintln!("SKIP: dotnet-bridge not published, run `dotnet publish` in dotnet-bridge/");
        return;
    }
    let mut rt = Runtime::new(".");
    assert_js(&mut rt, expr, msg);
}

#[test]
fn dotnet_overloads_pick_the_best_fitting_parameter_types() {
    assert_dotnet_js(
        r#"
        System.Math.Abs(-0.5) === 0.5
            && System.Math.Abs(-128) === 128
            && System.Math.Max(3.5, 2) === 3.5
            && new System.Text.StringBuilder('q').ToString() === 'q'
    "#,
        "overloads should resolve by argument type, not first convertible",
    );
}

#[test]
fn dotnet_collections_are_objects_that_act_like_arrays() {
    assert_dotnet_js(
        r#"
        (function(){
            var list = new System.Collections.ArrayList();
            list.Add(1); list.Add('x');
            list[0] = 7;
            var seen = [];
            for (var v of list) seen.push(v);
            return list.Count === 2 && list.length === 2 && list[1] === 'x' && list[5] === null
                && list.map(String).join('|') === '7|x' && seen.join('|') === '7|x'
                && JSON.stringify(list) === '[7,"x"]'
                && Array.isArray(System.IO.Path.GetInvalidPathChars());
        })()
    "#,
        "a constructed collection should keep its .NET methods and be array-like",
    );
}

#[test]
fn dotnet_date_and_time_values_are_objects() {
    assert_dotnet_js(
        r#"
        (function(){
            var span = System.TimeSpan.FromSeconds(2);
            var year = System.Convert.ToDateTime(new Date(Date.UTC(2031, 4, 6, 12))).ToUniversalTime().Year;
            return span.TotalMilliseconds === 2000 && String(span) === '00:00:02'
                && System.DateTime.Now.Year > 2000 && year === 2031;
        })()
    "#,
        "TimeSpan/DateTime results should expose their members, JS Dates should convert",
    );
}

#[test]
fn dotnet_callback_arguments_are_dotnet_objects() {
    assert_dotnet_js(
        r#"
        System.Text.RegularExpressions.Regex.Replace('a1b2', '\d', function (m) { return '[' + m.Value + ']'; }) === 'a[1]b[2]'
    "#,
        "a JS function passed as a delegate should receive .NET object proxies",
    );
}

#[test]
fn dotnet_params_and_array_arguments() {
    assert_dotnet_js(
        r#"
        System.String.Join(',', 'a', 'b', 'c') === 'a,b,c'
            && System.String.Join('-', ['a', 'b']) === 'a-b'
            && System.String.Format('{0}+{1}={2}', 1, 2, 3) === '1+2=3'
    "#,
        "params arrays and JS array arguments should marshal",
    );
}

#[test]
fn dotnet_plain_js_fields_are_kept() {
    assert_dotnet_js(
        r#"
        (function(){
            var sb = new System.Text.StringBuilder();
            sb.tag = 3;
            sb.Capacity = 99;
            System.Text.tag = 'ns';
            return sb.tag === 3 && Object.keys(sb).join() === 'tag' && sb.Capacity >= 99
                && System.Text.tag === 'ns' && ('' + sb).indexOf('DotNetObject') >= 0;
        })()
    "#,
        "plain JS fields on .NET objects and namespaces should be kept",
    );
}
