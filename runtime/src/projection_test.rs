use crate::Runtime;

fn run(name: &str, script: &str) {
    let name = name.to_string();
    let temp = std::env::temp_dir().to_string_lossy().trim_end_matches('\\').to_string();
    let script = script.replace("__TEMP_DIR__", &format!("{temp:?}"));
    std::thread::spawn(move || {
        let mut runtime = Runtime::new(".");
        runtime.run_script(&script, &format!("{name}.js"));
        if let Some(err) = crate::get_last_js_error() {
            panic!("JS error in {name}: {err}");
        }
    })
    .join()
    .unwrap();
}

/// `CoreApplication.Properties` is declared as `IPropertySet`, which has no members of its own;
/// the wrapper must come from the runtime class so the IMap surface is callable.
#[test]
fn test_interface_typed_return_wraps_runtime_class() {
    run(
        "property_set_return",
        r#"
        const props = Windows.ApplicationModel.Core.CoreApplication.Properties;
        if (typeof props.HasKey !== 'function') throw new Error('HasKey missing on ' + String(props));
        props.Insert('ns-test', 'v😀');
        if (!props.HasKey('ns-test')) throw new Error('HasKey after Insert');
        if (props.Lookup('ns-test') !== 'v😀') throw new Error('Lookup ' + props.Lookup('ns-test'));
        props.Remove('ns-test');
        if (props.HasKey('ns-test')) throw new Error('HasKey after Remove');
        "#,
    );
}

/// `StorageFile.CopyAsync`/`MoveAsync` overloads all carry `[Overload]` names (`CopyOverload*`);
/// the public name must still be callable and dispatch on arity, as must `RenameAsync(name)` and
/// `DeleteAsync()`, whose public names bind to the overload with an extra option argument.
#[test]
fn test_public_name_reaches_overload_named_methods() {
    run(
        "storage_overloads",
        r#"
        const S = Windows.Storage;
        function wait(op) {
            NSWinRT.wait(op, { timeoutMs: 10000 });
            if (Number(op.Status) !== 1) throw new Error('op failed: ' + JSON.stringify(op.ErrorCode));
            return op.GetResults();
        }
        const temp = wait(S.StorageFolder.GetFolderFromPathAsync(__TEMP_DIR__));
        const dir = wait(temp.CreateFolderAsync('ns-overloads-' + Date.now(), S.CreationCollisionOption.GenerateUniqueName));
        try {
            const sub = wait(dir.CreateFolderAsync('sub'));
            const file = wait(dir.CreateFileAsync('a.txt'));
            const copy = wait(file.CopyAsync(dir, 'b.txt', S.NameCollisionOption.FailIfExists));
            if (copy.Name !== 'b.txt') throw new Error('CopyAsync(3) → ' + copy.Name);
            wait(copy.MoveAsync(sub));
            if (copy.Path !== sub.Path + '\\b.txt') throw new Error('MoveAsync(1) → ' + copy.Path);
            wait(file.RenameAsync('c.txt'));
            if (file.Name !== 'c.txt') throw new Error('RenameAsync(1) → ' + file.Name);
            wait(file.DeleteAsync());
            if (wait(dir.TryGetItemAsync('c.txt')) !== null) throw new Error('DeleteAsync() left the file');
        } finally {
            wait(dir.DeleteAsync(S.StorageDeleteOption.PermanentDelete));
        }
        "#,
    );
}
