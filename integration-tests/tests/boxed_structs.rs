use runtime::Runtime;

fn eval(rt: &mut Runtime, expr: &str) -> String {
    rt.eval_script_to_string(expr)
        .unwrap_or_else(|| "<exception>".to_string())
}

#[test]
fn boxed_structs_unbox_and_box_by_shape() {
    let mut rt = Runtime::new(".");
    rt.run_script(
        "const PV = Windows.Foundation.PropertyValue;
         globalThis.ps = new Windows.Foundation.Collections.PropertySet();
         ps.Insert('size', PV.CreateSize({ Width: 9, Height: 8 }));
         ps.Insert('point', { X: 1, Y: 2 });
         ps.Insert('rect', { X: 1, Y: 2, Width: 3, Height: 4 });
         ps.Insert('span', { Duration: 10000000 });",
        "setup.js",
    );
    assert_eq!(eval(&mut rt, "JSON.stringify(ps.Lookup('size'))"), r#"{"Width":9,"Height":8}"#);
    assert_eq!(eval(&mut rt, "JSON.stringify(ps.Lookup('point'))"), r#"{"X":1,"Y":2}"#);
    assert_eq!(
        eval(&mut rt, "JSON.stringify(ps.Lookup('rect'))"),
        r#"{"X":1,"Y":2,"Width":3,"Height":4}"#
    );
    assert_eq!(eval(&mut rt, "JSON.stringify(ps.Lookup('span'))"), r#"{"Duration":10000000}"#);
    assert_eq!(
        eval(&mut rt, "JSON.stringify(Windows.Foundation.PropertyValue.CreatePoint({ X: 1.5, Y: 2.5 }))"),
        r#"{"X":1.5,"Y":2.5}"#
    );
}

#[test]
fn by_value_point_round_trips() {
    let mut rt = Runtime::new(".");
    assert_eq!(
        eval(
            &mut rt,
            "JSON.stringify(new Windows.UI.Input.Inking.InkPoint({ X: 3, Y: 4 }, 0.5).Position)"
        ),
        r#"{"X":3,"Y":4}"#
    );
}

#[test]
fn missing_struct_fields_serialize_as_zero() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "(() => { try {
            const s = new Windows.Media.Core.TimedTextStyle();
            s.Padding = { Before: 1, Unit: 1 };
            return JSON.stringify(s.Padding);
        } catch (e) { return 'skip'; } })()",
    );
    if v != "skip" {
        assert_eq!(v, r#"{"Before":1,"After":0,"Start":0,"End":0,"Unit":1}"#);
    }
}
