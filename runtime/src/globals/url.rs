use std::cell::RefCell;
use std::ffi::c_void;
use url::Url;
use v8::FunctionTemplate;

thread_local! {
    static URL_CTOR: RefCell<Option<v8::Global<v8::Function>>> = const { RefCell::new(None) };
}

/// Called from `Runtime::drop` while the isolate is alive — the stored
/// `v8::Global` must not outlive it.
pub(crate) fn clear_thread_url_ctor() {
    URL_CTOR.with(|c| *c.borrow_mut() = None);
}

#[repr(C)]
struct UrlData {
    tag: DataTag,
    url: Url,
}

/// First field of both data types, so an accessor can check which one an object carries.
#[repr(u32)]
#[derive(PartialEq, Eq, Clone, Copy)]
enum DataTag {
    Url = 0x5552_4c31,
    SearchParams = 0x5553_5031,
}

// URLSearchParams stores pairs as a Vec to preserve insertion order and allow duplicates.
#[repr(C)]
struct SearchParamsData {
    tag: DataTag,
    pairs: Vec<(String, String)>,
}

impl SearchParamsData {
    fn from_str(input: &str) -> Self {
        let s = input.trim_start_matches('?');
        let pairs = form_urlencoded::parse(s.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        Self { tag: DataTag::SearchParams, pairs }
    }

    fn to_string_value(&self) -> String {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish()
    }

    fn append(&mut self, k: &str, v: &str) {
        self.pairs.push((k.to_owned(), v.to_owned()));
    }

    fn remove_all(&mut self, k: &str) {
        self.pairs.retain(|(pk, _)| pk != k);
    }

    fn remove_pair(&mut self, k: &str, v: &str) {
        self.pairs.retain(|(pk, pv)| pk != k || pv != v);
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.as_str())
    }

    fn get_all(&self, k: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(pk, _)| pk == k)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn has_key(&self, k: &str) -> bool {
        self.pairs.iter().any(|(pk, _)| pk == k)
    }

    fn has_pair(&self, k: &str, v: &str) -> bool {
        self.pairs.iter().any(|(pk, pv)| pk == k && pv == v)
    }

    fn set(&mut self, k: &str, v: &str) {
        let pos = self.pairs.iter().position(|(pk, _)| pk == k);
        match pos {
            Some(i) => {
                self.pairs[i].1 = v.to_owned();
                self.pairs.retain_at_position(i, k);
            }
            None => self.pairs.push((k.to_owned(), v.to_owned())),
        }
    }

    fn sort(&mut self) {
        self.pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
    }
}

// Helper: retain only the first occurrence at `keep_idx` for key `k`, remove rest.
trait RetainAtPosition {
    fn retain_at_position(&mut self, keep_idx: usize, k: &str);
}
impl RetainAtPosition for Vec<(String, String)> {
    fn retain_at_position(&mut self, keep_idx: usize, k: &str) {
        let mut seen = false;
        self.retain(|(pk, _)| {
            if pk == k {
                if seen {
                    return false;
                }
                seen = true;
            }
            true
        });
        let _ = keep_idx; // keep_idx is the index before retain; after retain it's still valid
    }
}

/// The data pointer of a URL or URLSearchParams object, or None (with a TypeError thrown) for any
/// other receiver. Both templates have exactly one internal field; WinRT wrappers have two.
fn tagged_data_ptr(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    tag: DataTag,
) -> Option<*mut c_void> {
    let ptr = (obj.internal_field_count() == 1)
        .then(|| obj.get_internal_field(scope, 0))
        .flatten()
        .and_then(|field| v8::Local::<v8::External>::try_from(field).ok())
        .map(|ext| ext.value())
        .filter(|ptr| !ptr.is_null() && unsafe { *(*ptr as *const DataTag) } == tag);
    if ptr.is_none() {
        throw_type_error(scope, "Illegal invocation");
    }
    ptr
}

unsafe fn url_data_ptr(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<*mut UrlData> {
    tagged_data_ptr(scope, obj, DataTag::Url).map(|ptr| ptr as *mut UrlData)
}

unsafe fn sp_data_ptr(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<*mut SearchParamsData> {
    tagged_data_ptr(scope, obj, DataTag::SearchParams).map(|ptr| ptr as *mut SearchParamsData)
}

fn to_v8_str<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
    v8::String::new(scope, s)
        .unwrap_or_else(|| v8::String::empty(scope))
        .into()
}

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    if let Some(m) = v8::String::new(scope, msg) {
        let e = v8::Exception::type_error(scope, m);
        scope.throw_exception(e);
    }
}

fn arg_str(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    idx: i32,
) -> Option<String> {
    args.get(idx)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
}

fn parse_url(input: &str, base: Option<&str>) -> Result<Url, url::ParseError> {
    match base {
        Some(b) => {
            let base_url = Url::parse(b)?;
            base_url.join(input)
        }
        None => Url::parse(input),
    }
}

fn whatwg_host(url: &Url) -> String {
    match url.port() {
        Some(p) => format!("{}:{}", url.host_str().unwrap_or(""), p),
        None => url.host_str().unwrap_or("").to_owned(),
    }
}

fn whatwg_protocol(url: &Url) -> String {
    format!("{}:", url.scheme())
}

fn whatwg_search(url: &Url) -> String {
    url.query().map_or_else(String::new, |q| format!("?{q}"))
}

fn whatwg_hash(url: &Url) -> String {
    url.fragment().map_or_else(String::new, |f| format!("#{f}"))
}

fn whatwg_port(url: &Url) -> String {
    url.port().map_or_else(String::new, |p| p.to_string())
}

fn url_constructor(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // Without `new`, `this` is the receiver, which has no internal field to hold the data.
    if !args.is_construct_call() {
        throw_type_error(scope, "Failed to construct 'URL': Please use the 'new' operator");
        return;
    }
    let Some(input) = arg_str(scope, &args, 0) else {
        throw_type_error(scope, "Failed to construct 'URL': 1 argument required");
        return;
    };
    let base = if args.length() > 1 && !args.get(1).is_undefined() && !args.get(1).is_null() {
        arg_str(scope, &args, 1)
    } else {
        None
    };
    match parse_url(&input, base.as_deref()) {
        Ok(url) => {
            let data = Box::into_raw(Box::new(UrlData { tag: DataTag::Url, url }));
            args.this()
                .set_internal_field(0, v8::External::new(scope, data as _).into());
        }
        Err(_) => {
            throw_type_error(
                scope,
                &format!("Failed to construct 'URL': Invalid URL: {input}"),
            );
        }
    }
}

fn url_href_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.as_str().to_owned() };
    rv.set(to_v8_str(scope, &s));
}
fn url_protocol_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { whatwg_protocol(&(*d).url) };
    rv.set(to_v8_str(scope, &s));
}
fn url_username_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.username().to_owned() };
    rv.set(to_v8_str(scope, &s));
}
fn url_password_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.password().unwrap_or("").to_owned() };
    rv.set(to_v8_str(scope, &s));
}
fn url_host_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { whatwg_host(&(*d).url) };
    rv.set(to_v8_str(scope, &s));
}
fn url_hostname_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.host_str().unwrap_or("").to_owned() };
    rv.set(to_v8_str(scope, &s));
}
fn url_port_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { whatwg_port(&(*d).url) };
    rv.set(to_v8_str(scope, &s));
}
fn url_pathname_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.path().to_owned() };
    rv.set(to_v8_str(scope, &s));
}
fn url_search_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { whatwg_search(&(*d).url) };
    rv.set(to_v8_str(scope, &s));
}
fn url_hash_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { whatwg_hash(&(*d).url) };
    rv.set(to_v8_str(scope, &s));
}
fn url_origin_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.origin().ascii_serialization() };
    rv.set(to_v8_str(scope, &s));
}

fn url_href_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    match Url::parse(&val) {
        Ok(new_url) => {
            let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
            unsafe { (*d).url = new_url };
        }
        Err(_) => throw_type_error(
            scope,
            &format!("Failed to set 'href' on 'URL': Invalid URL: {val}"),
        ),
    }
}

fn url_protocol_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let scheme = val.trim_end_matches(':');
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let _ = unsafe { (*d).url.set_scheme(scheme) };
}

fn url_username_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let _ = unsafe { (*d).url.set_username(&val) };
}

fn url_password_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let pass = if val.is_empty() {
        None
    } else {
        Some(val.as_str())
    };
    let _ = unsafe { (*d).url.set_password(pass) };
}

fn url_host_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    // val may contain "hostname:port"; set_host handles this
    let _ = unsafe {
        (*d).url
            .set_host(if val.is_empty() { None } else { Some(&val) })
    };
}

fn url_hostname_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let _ = unsafe {
        (*d).url
            .set_host(if val.is_empty() { None } else { Some(&val) })
    };
}

fn url_port_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let port: Option<u16> = val.parse().ok();
    let _ = unsafe { (*d).url.set_port(port) };
}

fn url_pathname_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    unsafe { (*d).url.set_path(&val) };
}

fn url_search_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let q = val.trim_start_matches('?');
    unsafe {
        (*d).url
            .set_query(if q.is_empty() { None } else { Some(q) })
    };
}

fn url_hash_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let f = val.trim_start_matches('#');
    unsafe {
        (*d).url
            .set_fragment(if f.is_empty() { None } else { Some(f) })
    };
}

fn url_to_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { url_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).url.as_str().to_owned() };
    rv.set(to_v8_str(scope, &s));
}

fn url_can_parse(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = arg_str(scope, &args, 0).unwrap_or_default();
    let base = if args.length() > 1 && !args.get(1).is_undefined() {
        arg_str(scope, &args, 1)
    } else {
        None
    };
    rv.set_bool(parse_url(&input, base.as_deref()).is_ok());
}

fn url_parse_static(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = arg_str(scope, &args, 0).unwrap_or_default();
    let base = if args.length() > 1 && !args.get(1).is_undefined() {
        arg_str(scope, &args, 1)
    } else {
        None
    };

    if parse_url(&input, base.as_deref()).is_err() {
        rv.set(v8::null(scope).into());
        return;
    }

    let new_obj = URL_CTOR
        .with(|c| {
            c.borrow().as_ref().map(|g| {
                let ctor = v8::Local::new(scope, g);
                let inp_v: v8::Local<v8::Value> = v8::String::new(scope, &input).unwrap().into();
                if let Some(b) = &base {
                    let base_v: v8::Local<v8::Value> = v8::String::new(scope, b).unwrap().into();
                    ctor.new_instance(scope, &[inp_v, base_v])
                } else {
                    ctor.new_instance(scope, &[inp_v])
                }
            })
        })
        .flatten();

    rv.set(
        new_obj
            .map(Into::into)
            .unwrap_or_else(|| v8::null(scope).into()),
    );
}

fn sp_constructor(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if !args.is_construct_call() {
        throw_type_error(
            scope,
            "Failed to construct 'URLSearchParams': Please use the 'new' operator",
        );
        return;
    }
    let params = if args.length() == 0 || args.get(0).is_undefined() || args.get(0).is_null() {
        SearchParamsData::from_str("")
    } else {
        let init = args.get(0);
        if init.is_string() {
            let s = init
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_default();
            SearchParamsData::from_str(&s)
        } else if init.is_array() {
            let arr = v8::Local::<v8::Array>::try_from(init).unwrap();
            let mut p = SearchParamsData { tag: DataTag::SearchParams, pairs: Vec::new() };
            for i in 0..arr.length() {
                let Some(item) = arr.get_index(scope, i) else {
                    continue;
                };
                if item.is_array() {
                    let pair = v8::Local::<v8::Array>::try_from(item).unwrap();
                    let k = pair
                        .get_index(scope, 0)
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    let v = pair
                        .get_index(scope, 1)
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    p.append(&k, &v);
                }
            }
            p
        } else if init.is_object() {
            let mut p = SearchParamsData { tag: DataTag::SearchParams, pairs: Vec::new() };
            if let Some(json) = v8::json::stringify(scope, init) {
                let s = json.to_rust_string_lossy(scope);
                if let Ok(map) =
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&s)
                {
                    for (k, v) in map {
                        let vs = match &v {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Null => "null".into(),
                            other => other.to_string(),
                        };
                        p.append(&k, &vs);
                    }
                }
            }
            p
        } else {
            let s = init
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_default();
            SearchParamsData::from_str(&s)
        }
    };

    let data = Box::into_raw(Box::new(params));
    args.this()
        .set_internal_field(0, v8::External::new(scope, data as _).into());
}

fn sp_append(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let k = arg_str(scope, &args, 0).unwrap_or_default();
    let v = arg_str(scope, &args, 1).unwrap_or_default();
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    unsafe { (*d).append(&k, &v) };
}

fn sp_delete(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let k = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    if args.length() > 1 && !args.get(1).is_undefined() {
        let v = arg_str(scope, &args, 1).unwrap_or_default();
        unsafe { (*d).remove_pair(&k, &v) };
    } else {
        unsafe { (*d).remove_all(&k) };
    }
}

fn sp_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let k = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    match unsafe { (*d).get(&k) } {
        Some(s) => rv.set(to_v8_str(scope, s)),
        None => rv.set(v8::null(scope).into()),
    }
}

fn sp_get_all(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let k = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let all: Vec<String> = unsafe { (*d).get_all(&k) }
        .iter()
        .map(|s| s.to_string())
        .collect();
    let arr = v8::Array::new(scope, all.len() as i32);
    for (i, s) in all.iter().enumerate() {
        let v = to_v8_str(scope, s);
        arr.set_index(scope, i as u32, v);
    }
    rv.set(arr.into());
}

fn sp_has(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let k = arg_str(scope, &args, 0).unwrap_or_default();
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let result = if args.length() > 1 && !args.get(1).is_undefined() {
        let v = arg_str(scope, &args, 1).unwrap_or_default();
        unsafe { (*d).has_pair(&k, &v) }
    } else {
        unsafe { (*d).has_key(&k) }
    };
    rv.set_bool(result);
}

fn sp_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let k = arg_str(scope, &args, 0).unwrap_or_default();
    let v = arg_str(scope, &args, 1).unwrap_or_default();
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    unsafe { (*d).set(&k, &v) };
}

fn sp_sort(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    unsafe { (*d).sort() };
}

fn sp_to_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let s = unsafe { (*d).to_string_value() };
    rv.set(to_v8_str(scope, &s));
}

fn sp_size_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    rv.set_int32(unsafe { (*d).pairs.len() } as i32);
}

fn sp_for_each(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if args.length() < 1 || !args.get(0).is_function() {
        return;
    }
    let cb = v8::Local::<v8::Function>::try_from(args.get(0)).unwrap();
    let this_a = if args.length() > 1 {
        args.get(1)
    } else {
        v8::undefined(scope).into()
    };
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let entries: Vec<(String, String)> = unsafe { (*d).pairs.clone() };
    let sp = args.this();
    for (k, v) in &entries {
        let vv = to_v8_str(scope, v);
        let kv = to_v8_str(scope, k);
        let spv: v8::Local<v8::Value> = sp.into();
        let _ = cb.call(scope, this_a, &[vv, kv, spv]);
    }
}

fn sp_keys(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let keys: Vec<String> = unsafe { (*d).pairs.iter().map(|(k, _)| k.clone()).collect() };
    let arr = v8::Array::new(scope, keys.len() as i32);
    for (i, k) in keys.iter().enumerate() {
        let v = to_v8_str(scope, k);
        arr.set_index(scope, i as u32, v);
    }
    rv.set(arr.into());
}

fn sp_values(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let vals: Vec<String> = unsafe { (*d).pairs.iter().map(|(_, v)| v.clone()).collect() };
    let arr = v8::Array::new(scope, vals.len() as i32);
    for (i, val) in vals.iter().enumerate() {
        let v = to_v8_str(scope, val);
        arr.set_index(scope, i as u32, v);
    }
    rv.set(arr.into());
}

fn sp_entries(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(d) = (unsafe { sp_data_ptr(scope, args.this()) }) else { return };
    let pairs: Vec<(String, String)> = unsafe { (*d).pairs.clone() };
    let arr = v8::Array::new(scope, pairs.len() as i32);
    for (i, (k, v)) in pairs.iter().enumerate() {
        let pair = v8::Array::new(scope, 2);
        let kv = to_v8_str(scope, k);
        pair.set_index(scope, 0, kv);
        let vv = to_v8_str(scope, v);
        pair.set_index(scope, 1, vv);
        let pv: v8::Local<v8::Value> = pair.into();
        arr.set_index(scope, i as u32, pv);
    }
    rv.set(arr.into());
}

pub(crate) fn install_url_globals(scope: &mut v8::ContextScope<v8::HandleScope<v8::Context>>) {
    let global = scope.get_current_context().global(scope);

    macro_rules! proto_method {
        ($proto:expr, $name:expr, $cb:expr) => {{
            let ft = FunctionTemplate::new(scope, $cb);
            $proto.set(v8::String::new(scope, $name).unwrap().into(), ft.into());
        }};
    }
    macro_rules! rw {
        ($proto:expr, $name:expr, $g:expr, $s:expr) => {{
            let g = FunctionTemplate::new(scope, $g);
            let s = FunctionTemplate::new(scope, $s);
            $proto.set_accessor_property(
                v8::String::new(scope, $name).unwrap().into(),
                Some(g),
                Some(s),
                v8::PropertyAttribute::NONE,
            );
        }};
    }
    macro_rules! ro {
        ($proto:expr, $name:expr, $g:expr) => {{
            let g = FunctionTemplate::new(scope, $g);
            $proto.set_accessor_property(
                v8::String::new(scope, $name).unwrap().into(),
                Some(g),
                None,
                v8::PropertyAttribute::NONE,
            );
        }};
    }

    let sp_tmpl = FunctionTemplate::new(scope, sp_constructor);
    sp_tmpl.set_class_name(v8::String::new(scope, "URLSearchParams").unwrap());
    sp_tmpl.instance_template(scope).set_internal_field_count(1);

    let sp_proto = sp_tmpl.prototype_template(scope);
    proto_method!(sp_proto, "append", sp_append);
    proto_method!(sp_proto, "delete", sp_delete);
    proto_method!(sp_proto, "get", sp_get);
    proto_method!(sp_proto, "getAll", sp_get_all);
    proto_method!(sp_proto, "has", sp_has);
    proto_method!(sp_proto, "set", sp_set);
    proto_method!(sp_proto, "sort", sp_sort);
    proto_method!(sp_proto, "toString", sp_to_string);
    proto_method!(sp_proto, "forEach", sp_for_each);
    proto_method!(sp_proto, "keys", sp_keys);
    proto_method!(sp_proto, "values", sp_values);
    proto_method!(sp_proto, "entries", sp_entries);
    ro!(sp_proto, "size", sp_size_get);

    let sp_fn = sp_tmpl.get_function(scope).unwrap();
    global.set(
        scope,
        v8::String::new(scope, "URLSearchParams").unwrap().into(),
        sp_fn.into(),
    );

    let url_tmpl = FunctionTemplate::new(scope, url_constructor);
    url_tmpl.set_class_name(v8::String::new(scope, "URL").unwrap());
    url_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let url_proto = url_tmpl.prototype_template(scope);
    proto_method!(url_proto, "toString", url_to_string);
    proto_method!(url_proto, "toJSON", url_to_string);

    rw!(url_proto, "href", url_href_get, url_href_set);
    ro!(url_proto, "origin", url_origin_get);
    rw!(url_proto, "protocol", url_protocol_get, url_protocol_set);
    rw!(url_proto, "username", url_username_get, url_username_set);
    rw!(url_proto, "password", url_password_get, url_password_set);
    rw!(url_proto, "host", url_host_get, url_host_set);
    rw!(url_proto, "hostname", url_hostname_get, url_hostname_set);
    rw!(url_proto, "port", url_port_get, url_port_set);
    rw!(url_proto, "pathname", url_pathname_get, url_pathname_set);
    rw!(url_proto, "search", url_search_get, url_search_set);
    rw!(url_proto, "hash", url_hash_get, url_hash_set);

    let url_fn = url_tmpl.get_function(scope).unwrap();
    URL_CTOR.with(|c| *c.borrow_mut() = Some(v8::Global::new(scope, url_fn)));

    let can_parse_f = FunctionTemplate::new(scope, url_can_parse)
        .get_function(scope)
        .unwrap();
    url_fn.set(
        scope,
        v8::String::new(scope, "canParse").unwrap().into(),
        can_parse_f.into(),
    );
    let parse_f = FunctionTemplate::new(scope, url_parse_static)
        .get_function(scope)
        .unwrap();
    url_fn.set(
        scope,
        v8::String::new(scope, "parse").unwrap().into(),
        parse_f.into(),
    );

    global.set(
        scope,
        v8::String::new(scope, "URL").unwrap().into(),
        url_fn.into(),
    );
}
