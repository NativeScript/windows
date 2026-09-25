//! CommonJS natives for webpack `target: 'node'` bundles: NativeScript apps are built expecting
//! `require`/`module`/`exports`/`__dirname`/`__filename` as globals (Node normally provides these
//! via its module wrapper; this runtime is not Node). This module provides the two natives that
//! back that shim — `__nsResolveModulePath` and `__nsReadTextFile` — plus a read-only
//! `__nsAppRoot` string (the classic rusty_v8 runtime supplies the equivalent JS shim from
//! `global_fns::HELPER_SOURCE`). The JS-side `require`/`module`/`exports` shim itself lives in
//! `packages/common/src/prelude.rs` so it runs unchanged on every napi engine.
//!
//! Without this, every chunk a webpack "node" build produces (`runtime.js`/`vendor.js`/the app
//! bundle) throws `ReferenceError: require/module/exports is not defined` on evaluation — silently,
//! since a top-level throw here is caught by the host and never reaches the JS error/console
//! surfaces a real app would show. See [[project_engine_framework_parity]] for how this was found.

use std::path::{Path, PathBuf};

use napi::{CallContext, Env, JsUnknown, ValueType};

use crate::napi_engine::value::as_unknown;

fn arg_string(ctx: &CallContext, index: usize) -> napi::Result<Option<String>> {
    if ctx.length <= index {
        return Ok(None);
    }
    let v = ctx.get::<JsUnknown>(index)?;
    if matches!(v.get_type(), Ok(ValueType::Null) | Ok(ValueType::Undefined)) {
        return Ok(None);
    }
    let s = v.coerce_to_string()?.into_utf8()?.as_str()?.to_owned();
    Ok(Some(s))
}

/// Install `__nsAppRoot` (read-only string), `__nsReadTextFile`, `__nsResolveModulePath` —
/// install-if-missing, matching the rest of `install_globals`.
pub fn install_module_natives(env: &Env, app_root: &str) -> napi::Result<()> {
    let mut global = env.get_global()?;

    let has_app_root = matches!(
        global
            .get_named_property::<JsUnknown>("__nsAppRoot")
            .and_then(|v| v.get_type()),
        Ok(ValueType::String)
    );
    if !has_app_root {
        global.set_named_property("__nsAppRoot", env.create_string(app_root)?)?;
    }

    let read_text_file =
        env.create_function_from_closure("__nsReadTextFile", |ctx: CallContext| {
            let env = &ctx.env;
            let Some(path) = arg_string(&ctx, 0)? else {
                return Err(napi::Error::from_reason(
                    "__nsReadTextFile: path is null, undefined, or missing",
                ));
            };
            if path.is_empty() {
                return Err(napi::Error::from_reason("__nsReadTextFile: path is empty"));
            }
            if let Some(content) = crate::source_protect::read_text(&path) {
                return Ok(env.create_string(&content)?);
            }
            match std::fs::read_to_string(Path::new(&path)) {
                Ok(content) => Ok(env.create_string(&content)?),
                Err(err) => Err(napi::Error::from_reason(format!(
                    "Failed to read module file: {err}"
                ))),
            }
        })?;
    global.set_named_property("__nsReadTextFile", read_text_file)?;

    let resolve_module_path =
        env.create_function_from_closure("__nsResolveModulePath", |ctx: CallContext| {
            let env = &ctx.env;
            let Some(specifier) = arg_string(&ctx, 0)? else {
                return Err(napi::Error::from_reason(
                    "__nsResolveModulePath: module specifier is null, undefined, or missing",
                ));
            };
            if specifier.is_empty() {
                return Err(napi::Error::from_reason(
                    "__nsResolveModulePath: module specifier is empty",
                ));
            }
            let parent_path = arg_string(&ctx, 1)?;
            let app_root = arg_string(&ctx, 2)?.unwrap_or_default();

            let mut candidate = if specifier.starts_with("./") || specifier.starts_with("../") {
                let parent = parent_path
                    .map(|v| crate::normalize_js_path(&v))
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
                // A packed (virtual, no-plaintext-on-disk) referrer never satisfies `.is_file()`,
                // so also check the sealed bundle's table — otherwise a relative require() from a
                // bundle-only file would wrongly treat the referrer as the base dir, not its parent.
                let base = if parent.is_file()
                    || crate::source_protect::contains(&parent.to_string_lossy())
                {
                    parent.parent().map(Path::to_path_buf).unwrap_or(parent)
                } else {
                    parent
                };
                base.join(&specifier)
            } else {
                let direct = crate::normalize_js_path(&specifier);
                if direct.is_absolute() {
                    direct
                } else {
                    let app_base = if app_root.is_empty() {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    } else {
                        let lower = PathBuf::from(&app_root).join("app");
                        if lower.exists() {
                            lower
                        } else {
                            PathBuf::from(&app_root).join("App")
                        }
                    };
                    app_base.join(direct)
                }
            };

            candidate = crate::try_resolve_with_known_extensions(candidate);
            let resolved = candidate.canonicalize().unwrap_or(candidate);
            match resolved.to_str() {
                Some(p) => Ok(as_unknown(env, env.create_string(p)?)),
                None => Ok(as_unknown(env, env.get_null()?)),
            }
        })?;
    global.set_named_property("__nsResolveModulePath", resolve_module_path)?;

    Ok(())
}
