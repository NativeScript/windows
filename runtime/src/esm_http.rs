//! HTTP ES-module loading. The native half of the dev-server module contract the iOS/Android
//! runtimes implement and `@nativescript/vite`'s HMR client drives through `ns:module`:
//!
//! - modules served over `http(s)://` are fetched during the graph walk and keyed by a canonical
//!   URL (`canonicalize_http_url_key`);
//! - `configureLoader` installs an import map (imports + scopes), volatile URL patterns and the
//!   canonicalization vocabulary (which query params are cache busters, which paths are dev
//!   endpoints, which paths keep their query as identity). Policy stays with the client;
//! - `invalidateModules` evicts registry keys and marks them so the next fetch carries a one-shot
//!   `__ns_dev_nonce`, defeating any HTTP cache between us and the dev server.
//!
//! Mechanism mirrors ios/NativeScript/runtime/HttpLoader.mm + ModuleInternalCallbacks.mm.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

#[derive(Default, Clone)]
struct Canonicalization {
    strip_params: Vec<String>,
    dev_path_prefixes: Vec<String>,
    preserve_query_for: Vec<String>,
}

#[derive(Default)]
struct ImportMap {
    imports: HashMap<String, String>,
    /// Most specific (longest) prefix first.
    scopes: Vec<(String, HashMap<String, String>)>,
}

#[derive(Default)]
struct LoaderVocabulary {
    import_map: ImportMap,
    volatile_patterns: Vec<String>,
    canonicalization: Option<Canonicalization>,
}

thread_local! {
    // Per isolate (one isolate per thread), like the iOS runtime's per-isolate vocabulary.
    static VOCABULARY: RefCell<LoaderVocabulary> = RefCell::new(LoaderVocabulary::default());
}

#[cfg(any(feature = "classic", test))]
pub(crate) fn clear_thread_vocabulary() {
    VOCABULARY.with(|v| *v.borrow_mut() = LoaderVocabulary::default());
}

pub(crate) fn is_http(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// `http:/host` (one slash, left by path joins) back to `http://host`, so the collapsed form keys
/// and resolves as the URL it means.
pub(crate) fn repair_collapsed_scheme(url: &str) -> String {
    for scheme in ["http:/", "https:/"] {
        if url.starts_with(scheme) && !url.starts_with(&format!("{scheme}/")) {
            return format!("{scheme}/{}", &url[scheme.len()..]);
        }
    }
    url.to_string()
}

/// Stable registry key for an HTTP(S) module URL. Always drops the fragment. For dev endpoints
/// (per the configured vocabulary) drops cache-buster params and sorts the rest; any other URL
/// keeps its query verbatim, since there it can be identity. Unconfigured, only the fragment goes.
pub(crate) fn canonicalize_http_url_key(url: &str) -> String {
    let mut url = repair_collapsed_scheme(url);
    for wrapper in ["file://http:/", "file://https:/"] {
        if url.starts_with(wrapper) {
            url = repair_collapsed_scheme(&url["file://".len()..]);
        }
    }
    if !is_http(&url) {
        return url;
    }
    let no_hash = url.split('#').next().unwrap_or("").to_string();
    let Some(scheme_end) = no_hash.find("://") else {
        return no_hash;
    };
    let Some(path_start) = no_hash[scheme_end + 3..].find('/').map(|i| i + scheme_end + 3) else {
        return no_hash;
    };
    let (origin_and_path, query) = match no_hash[path_start..].find('?') {
        Some(q) => (&no_hash[..path_start + q], &no_hash[path_start + q + 1..]),
        None => (no_hash.as_str(), ""),
    };
    let canon = VOCABULARY.with(|v| v.borrow().canonicalization.clone());
    let Some(canon) = canon else {
        return no_hash;
    };
    let path = &origin_and_path[path_start..];
    if canon.preserve_query_for.iter().any(|p| !p.is_empty() && path.contains(p.as_str())) {
        return no_hash;
    }
    if !canon.dev_path_prefixes.iter().any(|p| !p.is_empty() && path.starts_with(p.as_str())) {
        return no_hash;
    }
    let mut kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("");
            !canon.strip_params.iter().any(|s| s == name)
        })
        .collect();
    if kept.is_empty() {
        return origin_and_path.to_string();
    }
    kept.sort_unstable();
    format!("{origin_and_path}?{}", kept.join("&"))
}

fn lookup_in(entries: &HashMap<String, String>, spec: &str) -> Option<String> {
    if let Some(v) = entries.get(spec) {
        return Some(v.clone());
    }
    // Longest trailing-slash prefix maps a subtree.
    entries
        .iter()
        .filter(|(k, _)| k.ends_with('/') && spec.len() > k.len() && spec.starts_with(k.as_str()))
        .max_by_key(|(k, _)| k.len())
        .map(|(k, v)| format!("{v}{}", &spec[k.len()..]))
}

/// Import-map resolution: the most specific scope matching the referrer's registry key first, then
/// less specific ones, then the top-level imports.
pub(crate) fn lookup_import_map(spec: &str, referrer_key: &str) -> Option<String> {
    VOCABULARY.with(|v| {
        let v = v.borrow();
        for (prefix, entries) in &v.import_map.scopes {
            if referrer_key.starts_with(prefix.as_str()) {
                if let Some(m) = lookup_in(entries, spec) {
                    return Some(m);
                }
            }
        }
        lookup_in(&v.import_map.imports, spec)
    })
}

pub(crate) fn is_volatile(url: &str) -> bool {
    VOCABULARY.with(|v| v.borrow().volatile_patterns.iter().any(|p| url.contains(p.as_str())))
}

fn string_map(value: &serde_json::Value, what: &str) -> Result<HashMap<String, String>, String> {
    let obj = value.as_object().ok_or_else(|| format!("{what} must be an object"))?;
    obj.iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|s| (k.clone(), s.to_string()))
                .ok_or_else(|| format!("{what}: the value for '{k}' must be a string"))
        })
        .collect()
}

fn string_list(value: &serde_json::Value, what: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{what} must be an array of strings"))?
        .iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("{what} must be an array of strings")))
        .collect()
}

/// `ns:module` `configureLoader(config)`, given the config as JSON. Validates every section before
/// applying any, so a rejected call leaves the live vocabulary untouched. A section that is present
/// replaces its state wholesale; an absent one is left alone.
pub(crate) fn configure_loader(json: &str) -> Result<(), String> {
    let config: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("configureLoader: invalid config: {e}"))?;
    let obj = config.as_object().ok_or("configureLoader expects a config object")?;
    for key in obj.keys() {
        if !matches!(key.as_str(), "importMap" | "volatilePatterns" | "canonicalization") {
            return Err(format!("configureLoader: unknown option '{key}'"));
        }
    }

    let import_map = match obj.get("importMap") {
        None => None,
        Some(map) => {
            let map_obj = map.as_object().ok_or("configureLoader: importMap must be an object")?;
            let mut parsed = ImportMap::default();
            for (section, value) in map_obj {
                match section.as_str() {
                    "imports" => parsed.imports = string_map(value, "configureLoader: importMap.imports")?,
                    "scopes" => {
                        let scopes = value.as_object().ok_or("configureLoader: importMap.scopes must be an object")?;
                        for (prefix, entries) in scopes {
                            if prefix.is_empty() {
                                return Err("configureLoader: a scope key must not be empty".into());
                            }
                            parsed.scopes.push((prefix.clone(), string_map(entries, &format!("configureLoader: scope '{prefix}'"))?));
                        }
                    }
                    other => {
                        return Err(format!(
                            "configureLoader: unsupported import-map section '{other}'; only \"imports\" and \"scopes\" are supported"
                        ))
                    }
                }
            }
            parsed
                .scopes
                .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| b.0.cmp(&a.0)));
            Some(parsed)
        }
    };
    let volatile = obj
        .get("volatilePatterns")
        .map(|v| string_list(v, "configureLoader: volatilePatterns"))
        .transpose()?;
    let canonicalization = match obj.get("canonicalization") {
        None => None,
        Some(c) => {
            let c = c.as_object().ok_or("configureLoader: canonicalization must be an object")?;
            let list = |key: &str| -> Result<Vec<String>, String> {
                c.get(key)
                    .map(|v| string_list(v, &format!("configureLoader: canonicalization.{key}")))
                    .transpose()
                    .map(Option::unwrap_or_default)
            };
            Some(Canonicalization {
                strip_params: list("stripParams")?,
                dev_path_prefixes: list("forPathPrefixes")?,
                preserve_query_for: list("preserveQueryFor")?,
            })
        }
    };

    VOCABULARY.with(|v| {
        let mut v = v.borrow_mut();
        if let Some(m) = import_map {
            v.import_map = m;
        }
        if let Some(p) = volatile {
            v.volatile_patterns = p;
        }
        if let Some(c) = canonicalization {
            v.canonicalization = Some(c);
        }
    });
    Ok(())
}

// Keys whose NEXT fetch must carry a unique `__ns_dev_nonce`, set by invalidateModules. Global
// (fetches can run on helper threads); holds canonical keys, never canonicalizes itself.
fn bust_marks() -> &'static Mutex<HashSet<String>> {
    static MARKS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    MARKS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn mark_keys_for_cache_bust(keys: &[String]) {
    if let Ok(mut m) = bust_marks().lock() {
        m.extend(keys.iter().cloned());
    }
}

/// Remote-module gate: debug hosts (`set_debug_build`) always allow; release builds need
/// `security.allowRemoteModules` in the app's package.json, then an allowlist match
/// (`security.remoteModuleAllowlist` URL prefixes; empty allows every URL).
pub(crate) fn remote_url_allowed(url: &str) -> bool {
    if DEBUG_BUILD.load(std::sync::atomic::Ordering::Relaxed) || cfg!(test) {
        return true;
    }
    let (allowed, allowlist) = SECURITY.get().cloned().unwrap_or((false, Vec::new()));
    allowed && (allowlist.is_empty() || allowlist.iter().any(|p| url.starts_with(p.as_str())))
}

static SECURITY: OnceLock<(bool, Vec<String>)> = OnceLock::new();

static DEBUG_BUILD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this as a debug (devtools) host, where dev-server modules are always allowed. The
/// counterpart of the iOS runtime's `RuntimeConfig.IsDebug`. Set by the host before scripts run.
pub fn set_debug_build(debug: bool) {
    DEBUG_BUILD.store(debug, std::sync::atomic::Ordering::Relaxed);
}

/// Read the release-build security settings from the app's package.json, once.
pub(crate) fn init_security_from_app_dir(app_dir: &std::path::Path) {
    let _ = SECURITY.get_or_init(|| {
        let text = crate::source_protect::read_text(&app_dir.join("package.json").to_string_lossy())
            .or_else(|| std::fs::read_to_string(app_dir.join("package.json")).ok());
        let json: serde_json::Value = text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let security = &json["security"];
        let allowed = security["allowRemoteModules"].as_bool().unwrap_or(false);
        let list = security["remoteModuleAllowlist"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        (allowed, list)
    });
}

pub(crate) enum ModuleKind {
    JavaScript,
    Json,
}

pub(crate) struct FetchedModule {
    pub kind: ModuleKind,
    pub body: String,
}

fn mime_essence(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn fetch_once(url: &str, key: &str) -> Result<crate::winhttp::Response, String> {
    let bust = bust_marks().lock().map(|m| m.contains(key)).unwrap_or(false);
    let wire_url = if bust {
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sep = if url.contains('?') { '&' } else { '?' };
        format!("{url}{sep}__ns_dev_nonce={}-{n}", std::process::id())
    } else {
        url.to_string()
    };
    let response = crate::winhttp::get(
        &wire_url,
        "Cache-Control: no-cache, no-store, max-age=0\r\nPragma: no-cache\r\n",
    )?;
    if bust {
        if let Ok(mut m) = bust_marks().lock() {
            m.remove(key);
        }
    }
    Ok(response)
}

/// Fetch one module over HTTP (one retry on transport error) and classify the response the way
/// the iOS runtime does: 2xx JavaScript (an empty body is the empty module) or JSON; anything else
/// is an import error naming the URL.
pub(crate) fn fetch_module(url: &str, key: &str) -> Result<FetchedModule, String> {
    if !remote_url_allowed(url) {
        return Err(format!(
            "HTTP import blocked: remote module loading is not allowed for {url} (set security.allowRemoteModules in the app's package.json)"
        ));
    }
    let response = fetch_once(url, key).or_else(|_| fetch_once(url, key))
        .map_err(|e| format!("HTTP import failed: {url} ({e})"))?;
    let status = response.status;
    if status == 204 || status == 205 {
        return Err(format!("HTTP import failed: {url} (status={status}, no content)"));
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP import failed: {url} (status={status})"));
    }
    let essence = mime_essence(&response.content_type);
    let body = String::from_utf8_lossy(&response.body).into_owned();
    if essence == "application/json" || essence.ends_with("+json") {
        if body.is_empty() {
            return Err(format!("Expected a JSON module but '{url}' responded with an empty body"));
        }
        return Ok(FetchedModule { kind: ModuleKind::Json, body });
    }
    let is_js = matches!(
        essence.as_str(),
        "text/javascript" | "application/javascript" | "application/x-javascript" | "text/ecmascript" | "application/ecmascript" | "module"
    );
    if !is_js {
        if essence.is_empty() {
            return Err(format!("Expected a JavaScript module but '{url}' responded with no MIME type"));
        }
        return Err(format!("Expected a JavaScript module but '{url}' responded with MIME type '{essence}'"));
    }
    Ok(FetchedModule {
        kind: ModuleKind::JavaScript,
        body: if body.is_empty() { "export {};\n".to_string() } else { body },
    })
}

/// Fetch several modules concurrently (bounded), preserving input order.
pub(crate) fn fetch_modules(items: &[(String, String)]) -> Vec<Result<FetchedModule, String>> {
    const MAX_PARALLEL: usize = 8;
    if items.len() <= 1 {
        return items.iter().map(|(u, k)| fetch_module(u, k)).collect();
    }
    let mut out: Vec<Option<Result<FetchedModule, String>>> = (0..items.len()).map(|_| None).collect();
    for (chunk_index, chunk) in items.chunks(MAX_PARALLEL).enumerate() {
        std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(u, k)| s.spawn(move || fetch_module(u, k)))
                .collect();
            for (i, h) in handles.into_iter().enumerate() {
                out[chunk_index * MAX_PARALLEL + i] =
                    Some(h.join().unwrap_or_else(|_| Err("HTTP fetch thread panicked".into())));
            }
        });
    }
    out.into_iter().map(|r| r.unwrap()).collect()
}

/// Module source for a JSON response: its value as the default export.
pub(crate) fn json_module_source(body: &str) -> String {
    let literal = serde_json::to_string(body).unwrap_or_else(|_| "\"null\"".into());
    format!("export default JSON.parse({literal});\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_keys_follow_the_configured_vocabulary() {
        clear_thread_vocabulary();
        // Unconfigured: only the fragment is dropped.
        assert_eq!(canonicalize_http_url_key("http://h:5173/ns/m/a.js?t=1#x"), "http://h:5173/ns/m/a.js?t=1");
        assert_eq!(canonicalize_http_url_key("http:/h:5173/ns/m/a.js"), "http://h:5173/ns/m/a.js");
        configure_loader(
            r#"{"canonicalization":{"stripParams":["t","v"],"forPathPrefixes":["/ns/"],"preserveQueryFor":["/@ng/component"]}}"#,
        )
        .unwrap();
        assert_eq!(canonicalize_http_url_key("http://h/ns/m/a.js?z=2&t=9&a=1"), "http://h/ns/m/a.js?a=1&z=2");
        assert_eq!(canonicalize_http_url_key("http://h/ns/m/a.js?t=9"), "http://h/ns/m/a.js");
        assert_eq!(canonicalize_http_url_key("http://h/other/a.js?t=9"), "http://h/other/a.js?t=9");
        assert_eq!(canonicalize_http_url_key("http://h/ns/@ng/component?t=9"), "http://h/ns/@ng/component?t=9");
        assert_eq!(canonicalize_http_url_key("file://http://h/ns/m/a.js?t=1"), "http://h/ns/m/a.js");
        clear_thread_vocabulary();
    }

    #[test]
    fn import_map_scopes_then_imports_with_prefix_entries() {
        clear_thread_vocabulary();
        configure_loader(
            r#"{"importMap":{"imports":{"@nativescript/core":"http://h/ns/core","@nativescript/core/":"http://h/ns/core/"},
                "scopes":{"http://h/ns/m/vendor/":{"@nativescript/core":"http://h/ns/vendor-core"}}}}"#,
        )
        .unwrap();
        assert_eq!(lookup_import_map("@nativescript/core", "http://h/ns/m/app.js").as_deref(), Some("http://h/ns/core"));
        assert_eq!(lookup_import_map("@nativescript/core/ui/frame", "").as_deref(), Some("http://h/ns/core/ui/frame"));
        assert_eq!(lookup_import_map("@nativescript/core", "http://h/ns/m/vendor/x.js").as_deref(), Some("http://h/ns/vendor-core"));
        assert_eq!(lookup_import_map("lodash", ""), None);
        // A rejected config leaves the previous map in place.
        assert!(configure_loader(r#"{"importMap":{"imports":{"a":1}}}"#).is_err());
        assert!(lookup_import_map("@nativescript/core", "").is_some());
        assert!(configure_loader(r#"{"bogus":true}"#).is_err());
        clear_thread_vocabulary();
    }
}
