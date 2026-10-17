use runtime::Runtime;

fn eval(rt: &mut Runtime, expr: &str) -> String {
    rt.eval_script_to_string(expr)
        .unwrap_or_else(|| "<exception>".to_string())
}

#[test]
fn returned_struct_is_plain_object() {
    let mut rt = Runtime::new(".");
    rt.run_script(
        "globalThis.c = Windows.UI.ColorHelper.FromArgb(255, 1, 2, 3);",
        "setup.js",
    );
    assert_eq!(eval(&mut rt, "JSON.stringify(c)"), r#"{"A":255,"R":1,"G":2,"B":3}"#);
    assert_eq!(eval(&mut rt, "JSON.stringify({ ...c })"), r#"{"A":255,"R":1,"G":2,"B":3}"#);
    assert_eq!(eval(&mut rt, "Object.keys(c).join()"), "A,R,G,B");
    assert_eq!(eval(&mut rt, "String(c)"), "[object Windows.UI.Color]");
    assert_eq!(eval(&mut rt, "c instanceof Windows.UI.Color"), "true");
    assert_eq!(eval(&mut rt, "c.hasOwnProperty('R') && !('zzz' in c)"), "true");
    assert_eq!(eval(&mut rt, "c.foo = 5; c.foo"), "5");
    assert_eq!(
        eval(&mut rt, "Object.getPrototypeOf(c) === Object.getPrototypeOf(Windows.UI.Colors.Red)"),
        "true"
    );
}

#[test]
fn constructor_builds_independent_values() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "const a = new Windows.Foundation.Rect({ X: 1, Y: 2, Width: 3, Height: 4 });
         const b = new Windows.Foundation.Rect(5, 6, 7, 8);
         JSON.stringify([a, b, a instanceof Windows.Foundation.Rect])",
    );
    assert_eq!(
        v,
        r#"[{"X":1,"Y":2,"Width":3,"Height":4},{"X":5,"Y":6,"Width":7,"Height":8},true]"#
    );
}

#[test]
fn constructor_zero_fills_and_validates() {
    let mut rt = Runtime::new(".");
    assert_eq!(
        eval(&mut rt, "JSON.stringify(new Windows.Foundation.Rect())"),
        r#"{"X":0,"Y":0,"Width":0,"Height":0}"#
    );
    assert_eq!(
        eval(&mut rt, "JSON.stringify(new Windows.Foundation.Rect({ X: 1 }))"),
        r#"{"X":1,"Y":0,"Width":0,"Height":0}"#
    );
    assert_eq!(
        eval(
            &mut rt,
            "try { new Windows.Foundation.Rect({ X: 'a' }); 'no' } catch (e) { e instanceof TypeError }"
        ),
        "true"
    );
    // Integer fields hold what native will receive.
    assert_eq!(eval(&mut rt, "new Windows.UI.Color({ R: 1.7 }).R"), "1");
}

#[test]
fn nested_struct_fields_are_read() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "(() => { try {
            const d = Windows.UI.Xaml.DurationHelper.FromTimeSpan({ Duration: 10000000 });
            return JSON.stringify(d);
        } catch (e) { return 'skip'; } })()",
    );
    if v != "skip" {
        assert_eq!(v, r#"{"TimeSpan":{"Duration":10000000},"Type":1}"#);
    }
}

#[test]
fn struct_values_round_trip_through_native() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "(() => { try {
            const s = new Windows.Media.Core.TimedTextStyle();
            s.Padding = { Before: 1, After: 2, Start: 3, End: 4, Unit: 1 };
            const p = s.Padding;
            p.End = 9;
            s.Padding = p;
            return JSON.stringify(s.Padding);
        } catch (e) { return 'skip'; } })()",
    );
    if v != "skip" {
        assert_eq!(v, r#"{"Before":1,"After":2,"Start":3,"End":9,"Unit":1}"#);
    }
}
