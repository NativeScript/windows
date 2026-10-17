use runtime::Runtime;

fn eval(rt: &mut Runtime, expr: &str) -> String {
    rt.eval_script_to_string(expr)
        .unwrap_or_else(|| "<exception>".to_string())
}

#[test]
fn resolved_members_are_cached() {
    let mut rt = Runtime::new(".");
    // A miss here means every access builds a new constructor (and a new V8 template).
    assert_eq!(eval(&mut rt, "Windows.Foundation.Uri === Windows.Foundation.Uri"), "true");
    assert_eq!(eval(&mut rt, "Windows.UI.Xaml === Windows.UI.Xaml"), "true");
    assert_eq!(eval(&mut rt, "'Uri' in Windows.Foundation"), "true");
    assert_eq!(eval(&mut rt, "Object.keys(Windows.Foundation).length"), "0");
    assert_eq!(eval(&mut rt, "Windows.UI.Xaml.Visibility.Collapsed"), "1");
    assert_eq!(eval(&mut rt, "new Windows.Foundation.Uri('http://x.y/').Host"), "x.y");
}

#[test]
fn resolved_members_are_read_only() {
    let mut rt = Runtime::new(".");
    assert_eq!(
        eval(&mut rt, "Windows.Foundation.Uri; Windows.Foundation.Uri = 5; typeof Windows.Foundation.Uri"),
        "function"
    );
    assert_eq!(
        eval(
            &mut rt,
            "(function () { 'use strict'; try { Windows.Foundation.Uri = 5; return false } catch (e) { return e instanceof TypeError } })()"
        ),
        "true"
    );
    assert_eq!(eval(&mut rt, "Windows.Foundation.myShim = 42; Windows.Foundation.myShim"), "42");
}
