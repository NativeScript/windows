//! ES module loader behaviour that bundler output (Vite/rolldown's `bundle.mjs` + chunks)
//! depends on: entry lookup, app-rooted specifiers, `import.meta`, error propagation out of
//! module evaluation (which rejects a promise under top-level await instead of throwing), and
//! dynamic `import()` settling after TLA.

use std::path::PathBuf;

use crate::Runtime;

/// A fresh temp dir holding `files` (relative path → source).
fn app_dir(test: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ns-esm-test-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, src) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, src).unwrap();
    }
    dir
}

fn run_entry(runtime: &mut Runtime, dir: &PathBuf, entry: &str) {
    let path = dir.join(entry);
    let src = std::fs::read_to_string(&path).unwrap();
    runtime.run_module(&src, &path.to_string_lossy());
}

fn eval(runtime: &mut Runtime, expr: &str) -> String {
    runtime
        .eval_script_to_string(expr)
        .unwrap_or_else(|| "<no result>".to_string())
}

#[test]
fn chunks_import_meta_and_app_rooted_specifiers() {
    let dir = app_dir(
        "graph",
        &[
            (
                "bundle.mjs",
                "import { v } from './vendor.mjs?v=123';\n\
                 import { w } from '~/lib/w.mjs';\n\
                 import { r } from '/lib/r.mjs';\n\
                 globalThis.__esm = [v, w, r, import.meta.url.startsWith('file:///'), import.meta.url.endsWith('/bundle.mjs'), typeof import.meta.dirname].join(',');",
            ),
            ("vendor.mjs", "export const v = 'vendor';"),
            ("lib/w.mjs", "export const w = 'tilde';"),
            ("lib/r.mjs", "export const r = 'root';"),
        ],
    );
    let mut runtime = Runtime::new(".");
    let _ = crate::get_last_js_error();
    run_entry(&mut runtime, &dir, "bundle.mjs");
    assert_eq!(crate::get_last_js_error(), None);
    assert_eq!(eval(&mut runtime, "globalThis.__esm"), "vendor,tilde,root,true,true,string");
}

#[test]
fn top_level_throw_is_reported_not_swallowed() {
    let dir = app_dir(
        "throw",
        &[("bundle.mjs", "globalThis.__before = 1;\nthrow new Error('boom at top level');")],
    );
    let mut runtime = Runtime::new(".");
    let _ = crate::get_last_js_error();
    run_entry(&mut runtime, &dir, "bundle.mjs");
    let err = crate::get_last_js_error().expect("module evaluation error must be reported");
    assert!(err.contains("boom at top level"), "{err}");
}

#[test]
fn missing_import_reports_cannot_find_module() {
    let dir = app_dir("missing", &[("bundle.mjs", "import './nope.mjs';\nglobalThis.__ran = true;")]);
    let mut runtime = Runtime::new(".");
    let _ = crate::get_last_js_error();
    run_entry(&mut runtime, &dir, "bundle.mjs");
    let err = crate::get_last_js_error().expect("unresolvable import must be reported");
    assert!(err.contains("Cannot find module './nope.mjs'"), "{err}");
    assert_eq!(eval(&mut runtime, "String(globalThis.__ran)"), "undefined");
}

#[test]
fn http_import_explains_hmr_is_unsupported() {
    let dir = app_dir(
        "http",
        &[("bundle.mjs", "import 'http://127.0.0.1:5173/ns/core/index.js';")],
    );
    let mut runtime = Runtime::new(".");
    let _ = crate::get_last_js_error();
    run_entry(&mut runtime, &dir, "bundle.mjs");
    let err = crate::get_last_js_error().expect("http import must be reported");
    assert!(err.contains("from disk only"), "{err}");
}

#[test]
fn dynamic_import_rejects_with_the_real_error() {
    let dir = app_dir(
        "dyn-reject",
        &[
            ("bundle.mjs", "globalThis.__dir = import.meta.dirname;"),
            ("bad.mjs", "throw new TypeError('bad module');"),
        ],
    );
    let mut runtime = Runtime::new(".");
    run_entry(&mut runtime, &dir, "bundle.mjs");
    eval(
        &mut runtime,
        "import('./bad.mjs'.replace('.', globalThis.__dir)).then(() => globalThis.__r = 'resolved', (e) => globalThis.__r = e.name + ':' + e.message)",
    );
    assert_eq!(eval(&mut runtime, "globalThis.__r"), "TypeError:bad module");
}

#[test]
fn dynamic_import_waits_for_top_level_await() {
    let dir = app_dir(
        "dyn-tla",
        &[
            ("bundle.mjs", "globalThis.__dir = import.meta.dirname;"),
            ("tla.mjs", "await Promise.resolve();\nexport const late = 'after-await';"),
        ],
    );
    let mut runtime = Runtime::new(".");
    run_entry(&mut runtime, &dir, "bundle.mjs");
    eval(
        &mut runtime,
        "import('./tla.mjs'.replace('.', globalThis.__dir)).then((m) => globalThis.__r = m.late, (e) => globalThis.__r = 'rejected:' + e)",
    );
    assert_eq!(eval(&mut runtime, "globalThis.__r"), "after-await");
}

#[test]
fn entry_passed_by_file_name_is_found_in_app_dir() {
    // Hosts pass `Path.GetFileName(entry)`; the runtime looks under <app_root>/app.
    let root = app_dir(
        "by-name",
        &[
            ("app/bundle.mjs", "import { v } from './vendor.mjs';\nglobalThis.__byName = v;"),
            ("app/vendor.mjs", "export const v = 'found';"),
        ],
    );
    let mut runtime = Runtime::new(&root.to_string_lossy());
    let _ = crate::get_last_js_error();
    let src = std::fs::read_to_string(root.join("app/bundle.mjs")).unwrap();
    runtime.run_module(&src, "bundle.mjs");
    assert_eq!(crate::get_last_js_error(), None);
    assert_eq!(eval(&mut runtime, "globalThis.__byName"), "found");
}

#[test]
fn text_encoder_decoder_utf8() {
    let mut runtime = Runtime::new(".");
    let got = eval(
        &mut runtime,
        r#"(function () {
            var bytes = new TextEncoder().encode('h\u20ac\ud83d\ude00');
            var d = new TextDecoder();
            var streamed = d.decode(bytes.subarray(0, 3), { stream: true }) + d.decode(bytes.subarray(3));
            var bom = new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41]));
            var fatal;
            try { new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xc3])); fatal = 'no-throw'; }
            catch (e) { fatal = e.name; }
            return [Array.prototype.join.call(bytes, ' '), streamed === 'h\u20ac\ud83d\ude00', bom, fatal,
                    new TextDecoder().decode(new Uint8Array([0xff])) === '\ufffd'].join('|');
        })()"#,
    );
    assert_eq!(got, "104 226 130 172 240 159 152 128|true|A|TypeError|true");
}

#[test]
fn ts_decorator_helpers_are_global_before_modules_run() {
    let dir = app_dir(
        "decorate",
        &[(
            "bundle.mjs",
            "function tag(t, k, d) { d.value = () => 'decorated'; return d; }\n\
             class C { m() { return 'plain'; } }\n\
             __decorate([tag], C.prototype, 'm', null);\n\
             globalThis.__deco = [typeof __extends, typeof __param, new C().m()].join(',');",
        )],
    );
    let mut runtime = Runtime::new(".");
    let _ = crate::get_last_js_error();
    run_entry(&mut runtime, &dir, "bundle.mjs");
    assert_eq!(crate::get_last_js_error(), None);
    assert_eq!(eval(&mut runtime, "globalThis.__deco"), "function,function,decorated");
}

#[test]
fn large_modules_get_a_bytecode_cache_that_later_runs_consume() {
    let mut src = String::from("let total = 0;\n");
    for i in 0..3000 {
        src.push_str(&format!("export function f{i}(x) {{ return x + {i}; }}\ntotal += f{i}(1);\n"));
    }
    src.push_str("globalThis.__cacheTotal = total;\n");
    assert!(src.len() >= 65536);
    let dir = app_dir("codecache", &[("bundle.mjs", &src)]);

    // LOG_DIR is process-wide and set once; use whatever the process already has.
    crate::set_log_dir(dir.join("LocalState").to_string_lossy().into_owned());
    let cache_dir = std::path::PathBuf::from(crate::log_dir_for_tests().expect("log dir")).join("v8-codecache");
    let cache_files = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(&cache_dir)
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().contains("bundle.mjs.")).collect())
            .unwrap_or_default()
    };
    for p in cache_files() {
        let _ = std::fs::remove_file(p);
    }

    let expected = (0..3000).map(|i| 1 + i).sum::<i64>().to_string();
    for run in 0..2 {
        let dir = dir.clone();
        let got = std::thread::spawn(move || {
            let mut runtime = Runtime::new(".");
            run_entry(&mut runtime, &dir, "bundle.mjs");
            eval(&mut runtime, "String(globalThis.__cacheTotal)")
        })
        .join()
        .unwrap();
        assert_eq!(got, expected, "run {run}");
        assert_eq!(cache_files().len(), 1, "run {run}: exactly one cache file for the bundle");
    }
}
