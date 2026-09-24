//! ES modules for napi engines without a module loader reachable through their shim (QuickJS,
//! Hermes, JSC; Hermes can't parse `import`/`export` at all). Each module is transformed with oxc's
//! `ModuleRunnerTransform` (Vite's ssrTransform) into an async function whose imports, exports,
//! `import.meta` and `import()` go through runtime calls, and a JS module runner (`RUNNER`) keeps
//! the registry, links imports, resolves cycles and settles dynamic imports. Exports are getters,
//! hoisted above the imports, so live bindings hold through import cycles.
//!
//! Resolution, source loading (files, the sealed app.nsbundle, HTTP dev-server modules, the
//! `ns:module` builtin) and error text come from `crate::esm_loader`, as for the V8 engines, so a
//! graph resolves the same way on every engine. The engine supplies only its script evaluator.
//! Top-level await is supported (every module body is async); `import()` from a classic script is
//! not, since that syntax never reaches the transform.

use std::cell::Cell;

use napi::{sys, CallContext, Env, JsObject, JsUnknown, NapiRaw, NapiValue, Status, ValueType};

use crate::esm_loader;

/// Evaluate `code` as a classic script named `filename` and return its completion value. On
/// failure the engine's exception is left pending.
pub type EvalScript = fn(env: &Env, code: &str, filename: &str) -> Result<sys::napi_value, ()>;

thread_local! {
    static EVAL: Cell<Option<EvalScript>> = const { Cell::new(None) };
}

/// The runner's module body parameters, in the order `RUNNER` passes them.
const WRAPPER_HEAD: &str = "(async function (__vite_ssr_exports__, __vite_ssr_import_meta__, \
__vite_ssr_import__, __vite_ssr_dynamic_import__, __vite_ssr_exportAll__, __vite_ssr_exportName__) \
{\"use strict\";\n";
const WRAPPER_TAIL: &str = "\n})";

/// A transformed module: the wrapper function's source and the specifiers it imports statically.
pub struct Transformed {
    pub code: String,
    pub deps: Vec<String>,
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset.min(source.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rfind('\n').map_or(before.len(), |i| before.len() - i - 1) + 1;
    (line, col)
}

/// Transform an ES module into the runner's async wrapper. `Err` carries a SyntaxError message
/// with the first error's location.
pub fn transform(source: &str, key: &str) -> Result<Transformed, String> {
    use oxc::allocator::Allocator;
    use oxc::codegen::Codegen;
    use oxc::parser::Parser;
    use oxc::semantic::SemanticBuilder;
    use oxc::span::SourceType;
    use oxc_transformer_plugins::ModuleRunnerTransform;

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    if let Some(error) = parsed.diagnostics.first() {
        let at = error
            .labels
            .as_ref()
            .first()
            .map(|label| line_col(source, label.offset() as usize));
        return Err(match at {
            Some((line, col)) => format!("{} ({key}:{line}:{col})", error.message),
            None => format!("{} ({key})", error.message),
        });
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let (deps, _dynamic) = ModuleRunnerTransform::new().transform(&allocator, &mut program, scoping);
    let body = Codegen::new().build(&program).code;
    let mut deps: Vec<String> = deps.into_iter().collect();
    deps.sort();
    Ok(Transformed { code: format!("{WRAPPER_HEAD}{body}{WRAPPER_TAIL}"), deps })
}

fn arg_string(ctx: &CallContext, index: usize) -> napi::Result<Option<String>> {
    if ctx.length <= index {
        return Ok(None);
    }
    let v = ctx.get::<JsUnknown>(index)?;
    if matches!(v.get_type()?, ValueType::Undefined | ValueType::Null) {
        return Ok(None);
    }
    Ok(Some(v.coerce_to_string()?.into_utf8()?.as_str()?.to_owned()))
}

fn arg_strings(ctx: &CallContext, index: usize) -> napi::Result<Vec<String>> {
    let mut out = Vec::new();
    if ctx.length <= index {
        return Ok(out);
    }
    let list = ctx.get::<JsUnknown>(index)?;
    if !list.is_array()? {
        return Ok(out);
    }
    let list: JsObject = unsafe { list.cast() };
    for i in 0..list.get_array_length()? {
        let item = list.get_element::<JsUnknown>(i)?;
        out.push(item.coerce_to_string()?.into_utf8()?.as_str()?.to_owned());
    }
    Ok(out)
}

/// Throw a real `SyntaxError` (napi8 has no constructor for one) and return the pending marker.
fn throw_syntax_error(env: &Env, message: &str) -> napi::Error {
    let thrown = (|| -> napi::Result<()> {
        let ctor: napi::JsFunction = env.get_global()?.get_named_property("SyntaxError")?;
        let error = ctor.new_instance(&[env.create_string(message)?])?;
        env.throw(error)
    })();
    match thrown {
        Ok(()) => napi::Error::new(Status::PendingException, String::new()),
        Err(_) => napi::Error::from_reason(message.to_string()),
    }
}

/// String coercion of the pending exception, cleared.
fn take_exception(env: &Env) -> String {
    let mut exc: sys::napi_value = std::ptr::null_mut();
    if unsafe { sys::napi_get_and_clear_last_exception(env.raw(), &mut exc) } != sys::Status::napi_ok
        || exc.is_null()
    {
        return "<no exception>".into();
    }
    unsafe { JsUnknown::from_raw_unchecked(env.raw(), exc) }
        .coerce_to_string()
        .and_then(|s| s.into_utf8())
        .and_then(|s| Ok(s.as_str()?.to_owned()))
        .unwrap_or_else(|_| "<unprintable exception>".into())
}

/// `__nsEsmLoad(key, spec, referrer) → { fn, deps }`: read, transform and evaluate a module's
/// wrapper. Throws the not-found reason, a SyntaxError, or the engine's own evaluation error.
fn load(ctx: CallContext) -> napi::Result<JsObject> {
    let env = &ctx.env;
    let key = arg_string(&ctx, 0)?.unwrap_or_default();
    let Some(source) = esm_loader::read_source(&key) else {
        let spec = arg_string(&ctx, 1)?.unwrap_or_else(|| key.clone());
        let referrer = arg_string(&ctx, 2)?;
        return Err(napi::Error::from_reason(esm_loader::not_found_message(
            &spec,
            referrer.as_deref(),
            &key,
        )));
    };
    let transformed = transform(&source, &key).map_err(|e| throw_syntax_error(env, &e))?;
    let Some(eval) = EVAL.with(Cell::get) else {
        return Err(napi::Error::from_reason("module runner is not installed"));
    };
    let function = eval(env, &transformed.code, &key)
        .map_err(|()| napi::Error::new(Status::PendingException, String::new()))?;
    let mut deps = env.create_array_with_length(transformed.deps.len())?;
    for (i, dep) in transformed.deps.iter().enumerate() {
        deps.set_element(i as u32, env.create_string(dep)?)?;
    }
    let mut out = env.create_object()?;
    out.set_named_property("fn", unsafe { JsUnknown::from_raw_unchecked(env.raw(), function) })?;
    out.set_named_property("deps", deps)?;
    Ok(out)
}

/// Install the runner's natives and the runner itself, then `ns:module`'s natives over it. Call
/// after `host_abi::initialize_runtime` and before the shared prelude (which builds
/// `__nsModuleBuiltin` from `__nsModuleConfigureLoader`).
pub fn install(env: &Env, eval: EvalScript) -> napi::Result<()> {
    EVAL.with(|e| e.set(Some(eval)));
    let mut global = env.get_global()?;

    let load_fn = env.create_function_from_closure("__nsEsmLoad", load)?;
    global.set_named_property("__nsEsmLoad", load_fn)?;

    let resolve = env.create_function_from_closure("__nsEsmResolve", |ctx: CallContext| {
        let spec = arg_string(&ctx, 0)?.unwrap_or_default();
        let referrer = arg_string(&ctx, 1)?;
        ctx.env.create_string(&esm_loader::resolve(&spec, referrer.as_deref()))
    })?;
    global.set_named_property("__nsEsmResolve", resolve)?;

    let prefetch = env.create_function_from_closure("__nsEsmPrefetch", |ctx: CallContext| {
        esm_loader::prefetch(&arg_strings(&ctx, 0)?);
        ctx.env.get_undefined()
    })?;
    global.set_named_property("__nsEsmPrefetch", prefetch)?;

    let volatile = env.create_function_from_closure("__nsEsmIsVolatile", |ctx: CallContext| {
        let key = arg_string(&ctx, 0)?.unwrap_or_default();
        ctx.env.get_boolean(esm_loader::is_volatile(&key))
    })?;
    global.set_named_property("__nsEsmIsVolatile", volatile)?;

    let registry_key = env.create_function_from_closure("__nsEsmRegistryKey", |ctx: CallContext| {
        let url = arg_string(&ctx, 0)?.unwrap_or_default();
        ctx.env.create_string(&esm_loader::registry_key_for(&url))
    })?;
    global.set_named_property("__nsEsmRegistryKey", registry_key)?;

    // Registry keys the runner dropped: forget their load failures and prefetched bodies, and
    // cache-bust the next fetch of each HTTP one.
    let forget = env.create_function_from_closure("__nsEsmForget", |ctx: CallContext| {
        let keys = arg_strings(&ctx, 0)?;
        for key in &keys {
            esm_loader::forget(key);
        }
        let http: Vec<String> = keys.into_iter().filter(|k| esm_loader::is_http(k)).collect();
        esm_loader::mark_keys_for_cache_bust(&http);
        ctx.env.get_undefined()
    })?;
    global.set_named_property("__nsEsmForget", forget)?;

    let report = env.create_function_from_closure("__nsEsmReportError", |ctx: CallContext| {
        let report = arg_string(&ctx, 0)?.unwrap_or_default();
        crate::debug_output(&format!("[NativeScript] Uncaught error evaluating module: {report}\n"));
        crate::store_last_js_error(report);
        ctx.env.get_undefined()
    })?;
    global.set_named_property("__nsEsmReportError", report)?;

    let configure = env.create_function_from_closure("__nsModuleConfigureLoader", |ctx: CallContext| {
        let json = arg_string(&ctx, 0)?.unwrap_or_default();
        esm_loader::configure_loader(&json).map_err(napi::Error::from_reason)?;
        ctx.env.get_undefined()
    })?;
    global.set_named_property("__nsModuleConfigureLoader", configure)?;

    eval(env, RUNNER, "<module-runner>").map_err(|()| {
        napi::Error::from_reason(format!("module runner failed to install: {}", take_exception(env)))
    })?;
    Ok(())
}

/// Run the entry module the host passes to `runtime_runscript` (`filename`, absolute or relative
/// to the app root). Failures are reported through `__nsEsmReportError` when the graph settles.
pub fn run_entry(env: &Env, filename: &str, app_root: &str) -> napi::Result<()> {
    let key = esm_loader::entry_key(filename, app_root);
    let global = env.get_global()?;
    let runner: JsObject = global.get_named_property("__nsEsmRunner")?;
    let run_entry: napi::JsFunction = runner.get_named_property("runEntry")?;
    run_entry.call(Some(&runner), &[env.create_string(&key)?])?;
    Ok(())
}

/// The module runner. Engine-conservative like the prelude (function expressions, no syntax the
/// transform would have to lower), since it's evaluated as a classic script on every engine.
pub const RUNNER: &str = r#"
(function (g) {
  'use strict';
  if (typeof g.__nsEsmLoad !== 'function' || g.__nsEsmRunner) { return; }

  var load = g.__nsEsmLoad;
  var resolve = g.__nsEsmResolve;
  var prefetch = g.__nsEsmPrefetch;
  var isVolatile = g.__nsEsmIsVolatile;
  var registryKey = g.__nsEsmRegistryKey;
  var forget = g.__nsEsmForget;
  var report = g.__nsEsmReportError;

  // Registry key -> { key, exports, promise, evaluated, stack }. `stack` is the static import
  // chain the module was first reached through, which is how a cycle is recognized.
  var registry = new Map();

  function newNamespace() {
    var ns = Object.create(null);
    Object.defineProperty(ns, Symbol.toStringTag, { value: 'Module' });
    return ns;
  }

  // `import.meta`: `url` (a file:/// URL, as bundlers' `new URL(x, import.meta.url)` expect),
  // `filename` and `dirname`. A served module's identity is its URL alone.
  function importMeta(key) {
    if (/^https?:\/\//.test(key) || key === 'ns:module') { return { url: key }; }
    var path = key.indexOf('\\\\?\\') === 0 ? key.slice(4) : key;
    var sep = Math.max(path.lastIndexOf('\\'), path.lastIndexOf('/'));
    return {
      url: 'file:///' + path.replace(/\\/g, '/'),
      filename: path,
      dirname: sep < 0 ? '' : path.slice(0, sep)
    };
  }

  function defineExport(exports, name, getter) {
    Object.defineProperty(exports, name, { enumerable: true, configurable: true, get: getter });
  }

  // `export * from`: every name but `default` and the ones the module declares itself.
  function exportAll(exports, source) {
    for (var name in source) {
      if (name !== 'default' && name !== '__esModule' && !(name in exports)) {
        defineExport(exports, name, (function (n) { return function () { return source[n]; }; })(name));
      }
    }
  }

  function describe(e) {
    var text = String(e);
    var stack = e && e.stack ? String(e.stack) : '';
    if (!stack) { return text; }
    return stack.indexOf(text) >= 0 ? stack : text + '\n' + stack;
  }

  function evict(key) {
    var removed = registry.delete(key);
    forget([key]);
    return removed;
  }

  // Load and start evaluating `key`. A load failure throws and leaves nothing registered, so a
  // later import retries it.
  function instantiate(key, spec, referrer, parentStack) {
    var loaded = load(key, spec, referrer);
    var record = { key: key, exports: newNamespace(), promise: null, evaluated: false, stack: parentStack.concat([key]) };
    registry.set(key, record);

    // Fetch the dependencies served over HTTP concurrently before the body awaits them in turn.
    var deps = [];
    for (var i = 0; i < loaded.deps.length; i++) {
      var dep = resolve(loaded.deps[i], key);
      if (!registry.has(dep) && deps.indexOf(dep) < 0) { deps.push(dep); }
    }
    if (deps.length > 1) { prefetch(deps); }

    var exports = record.exports;
    record.promise = loaded.fn.call(
      undefined,
      exports,
      importMeta(key),
      function (s) { return importFrom(s, record); },
      function (s) { return dynamicImport(s, key); },
      function (source) { exportAll(exports, source); },
      function (name, getter) { defineExport(exports, name, getter); }
    ).then(function () {
      record.evaluated = true;
      return exports;
    });
    return record;
  }

  // Static import from `importer`'s body.
  function importFrom(spec, importer) {
    var key = resolve(spec, importer.key);
    var record = registry.get(key);
    if (record) {
      // Inside an import cycle, take the partially evaluated exports: their getters fill in once
      // the module's body runs.
      if (record.evaluated || importer.stack.indexOf(key) >= 0) { return record.exports; }
      return record.promise;
    }
    try {
      return instantiate(key, spec, importer.key, importer.stack).promise;
    } catch (e) {
      return Promise.reject(e);
    }
  }

  // `import()`: settles with the namespace once the graph (including any top-level await) has
  // evaluated, or rejects with the real error.
  function dynamicImport(spec, referrer) {
    try {
      var key = resolve(spec, referrer);
      // Volatile URLs (configureLoader) are re-fetched on every import.
      if (isVolatile(key)) { evict(key); }
      var record = registry.get(key) || instantiate(key, spec, referrer, []);
      return record.promise;
    } catch (e) {
      return Promise.reject(e);
    }
  }

  function runEntry(key) {
    var promise;
    try {
      promise = (registry.get(key) || instantiate(key, key, null, [])).promise;
    } catch (e) {
      promise = Promise.reject(e);
    }
    return promise.then(null, function (e) { report(describe(e)); });
  }

  // `ns:module`'s `invalidateModules(urls)`: registry eviction plus a one-shot cache-bust nonce on
  // each evicted URL's next fetch. Importers already linked keep the old exports; the HMR client
  // re-imports the graph above them.
  g.__nsModuleInvalidate = function (urls) {
    if (!Array.isArray(urls)) { throw new TypeError('invalidateModules expects an array of URL strings'); }
    var keys = [];
    for (var i = 0; i < urls.length; i++) {
      if (typeof urls[i] !== 'string') { throw new TypeError('invalidateModules: urls[' + i + '] must be a string'); }
      var key = registryKey(urls[i]);
      if (key && keys.indexOf(key) < 0) { keys.push(key); }
    }
    var removed = 0;
    for (var j = 0; j < keys.length; j++) {
      if (registry.delete(keys[j])) { removed++; }
    }
    forget(keys);
    return removed;
  };

  // `ns:module`'s `getLoadedModuleUrls()`: the URL-keyed (served) modules currently registered.
  g.__nsModuleLoadedUrls = function () {
    var urls = [];
    registry.forEach(function (record, key) {
      if (key.indexOf('://') >= 0) { urls.push(key); }
    });
    return urls.sort();
  };

  Object.defineProperty(g, '__nsEsmRunner', {
    value: Object.freeze({ runEntry: runEntry, importModule: dynamicImport }),
    configurable: true
  });
})(globalThis);
'module-runner-ok'
"#;

#[cfg(test)]
mod tests {
    use super::transform;

    #[test]
    fn exports_are_getters_hoisted_above_awaited_imports() {
        let out = transform(
            "import d, { a } from './dep.mjs';\nexport let n = a;\nexport default d;\nexport * from './star.mjs';",
            "C:/app/main.mjs",
        )
        .unwrap();
        assert_eq!(out.deps, vec!["./dep.mjs".to_string(), "./star.mjs".to_string()]);
        assert!(out.code.starts_with("(async function (__vite_ssr_exports__"), "{}", out.code);
        let getter = out.code.find("Object.defineProperty(__vite_ssr_exports__, \"n\"").unwrap();
        let import = out.code.find("await __vite_ssr_import__(\"./dep.mjs\"").unwrap();
        assert!(getter < import, "{}", out.code);
        assert!(out.code.contains("__vite_ssr_exportAll__("), "{}", out.code);
    }

    #[test]
    fn import_meta_and_dynamic_import_go_through_the_runner() {
        let out = transform("const u = import.meta.url; const m = () => import('./lazy.mjs');", "k").unwrap();
        assert!(out.code.contains("__vite_ssr_import_meta__.url"), "{}", out.code);
        assert!(out.code.contains("__vite_ssr_dynamic_import__(\"./lazy.mjs\")"), "{}", out.code);
        assert!(out.deps.is_empty());
    }

    #[test]
    fn syntax_errors_name_the_module_and_location() {
        let err = transform("export const ok = 1;\nexport const = 2;", "C:/app/bad.mjs").err().unwrap();
        assert!(err.contains("(C:/app/bad.mjs:2:"), "{err}");
    }
}
