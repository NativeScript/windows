//! JS arrays passed where WinRT expects `IIterable<T>` / `IVectorView<T>` / `IVector<T>`.

use runtime::Runtime;

fn eval(rt: &mut Runtime, expr: &str) -> String {
    rt.eval_script_to_string(expr)
        .unwrap_or_else(|| "<eval failed>".to_string())
}

#[test]
fn string_array_to_iterable_in_constructor() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "(() => { const f = new Windows.Globalization.NumberFormatting.DecimalFormatter(['fr-FR', 'de-DE'], 'FR'); \
         return f.Languages.Size + ':' + f.Languages.GetAt(0) + ':' + f.Languages.GetAt(1); })()",
    );
    assert_eq!(v.trim(), "2:fr-FR:de-DE", "the languages array arrives as IIterable<String>");
}

#[test]
fn string_array_formats_with_its_language() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "['en-US', 'de-DE'].map((l) => new Windows.Globalization.NumberFormatting.DecimalFormatter([l], 'US').FormatDouble(1234.5)).join('|')",
    );
    assert_eq!(v.trim(), "1234.50|1234,50", "each formatter uses the language from its array argument");
}

#[test]
fn native_collection_is_queried_for_the_closed_iid() {
    // Languages is an IVectorView<String>; the parameter wants IIterable<String>, which the runtime
    // has to QI for by the closed generic's IID.
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "(() => { const N = Windows.Globalization.NumberFormatting;          const source = new N.DecimalFormatter(['de-DE', 'fr-FR'], 'DE');          const copy = new N.DecimalFormatter(source.Languages, 'DE');          return copy.Languages.Size + ':' + copy.Languages.GetAt(1); })()",
    );
    assert_eq!(v.trim(), "2:fr-FR", "a native IVectorView passed as IIterable");
}

#[test]
fn string_array_to_calendar_languages() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "new Windows.Globalization.Calendar(['en-US']).Languages.GetAt(0)",
    );
    assert_eq!(v.trim(), "en-US", "Calendar(IIterable<String>)");
}

#[test]
fn object_array_to_iterable_of_classes() {
    let mut rt = Runtime::new(".");
    let v = eval(
        &mut rt,
        "(() => { const I = Windows.UI.Input.Inking; \
         const points = [new I.InkPoint({ X: 1, Y: 2 }, 0.5), new I.InkPoint({ X: 3, Y: 4 }, 0.5), new I.InkPoint({ X: 5, Y: 6 }, 0.5)]; \
         const stroke = new I.InkStrokeBuilder().CreateStrokeFromInkPoints(points, { M11: 1, M12: 0, M21: 0, M22: 1, M31: 0, M32: 0 }); \
         const got = stroke.GetInkPoints(); \
         return got.Size + ':' + got.GetAt(2).Position.X; })()",
    );
    assert_eq!(v.trim(), "3:5", "IIterable<InkPoint> built from an array of InkPoint");
}
