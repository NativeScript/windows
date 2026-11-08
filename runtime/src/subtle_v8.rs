use crate::subtle::{dispatch, Js, Key};
use serde_json::Value;

type Args<'s> = v8::FunctionCallbackArguments<'s>;
type Rv<'s> = v8::ReturnValue<'s, v8::Value>;

fn key_slot<'s>(scope: &v8::PinScope<'s, '_, ()>) -> v8::Local<'s, v8::Private> {
    let name = v8::String::new(scope, "ns#cryptoKey").unwrap();
    v8::Private::for_api(scope, Some(name))
}

struct V8<'a, 's, 'i> {
    scope: &'a mut v8::PinScope<'s, 'i>,
    proto: v8::Local<'s, v8::Value>,
}

impl<'a, 's, 'i> V8<'a, 's, 'i> {
    fn str(&mut self, s: &str) -> v8::Local<'s, v8::String> {
        v8::String::new(self.scope, s).unwrap()
    }
}

impl<'a, 's, 'i> Js for V8<'a, 's, 'i> {
    type V = v8::Local<'s, v8::Value>;

    fn undefined(&mut self) -> Self::V {
        v8::undefined(self.scope).into()
    }

    fn is_nullish(&mut self, v: Self::V) -> bool {
        v.is_null_or_undefined()
    }

    fn string(&mut self, v: Self::V) -> Option<String> {
        v.is_string().then(|| v.to_rust_string_lossy(self.scope))
    }

    fn number(&mut self, v: Self::V) -> Option<f64> {
        if v.is_number() {
            v.number_value(self.scope)
        } else {
            None
        }
    }

    fn boolean(&mut self, v: Self::V) -> bool {
        v.boolean_value(self.scope)
    }

    fn bytes(&mut self, v: Self::V) -> Option<Vec<u8>> {
        if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(v) {
            let mut out = vec![0; view.byte_length()];
            view.copy_contents(&mut out);
            return Some(out);
        }
        let buffer = v8::Local::<v8::ArrayBuffer>::try_from(v).ok()?;
        Some(match buffer.data() {
            Some(p) => unsafe {
                std::slice::from_raw_parts(p.as_ptr() as *const u8, buffer.byte_length()).to_vec()
            },
            None => Vec::new(),
        })
    }

    fn get(&mut self, v: Self::V, name: &str) -> Self::V {
        let Ok(object) = v8::Local::<v8::Object>::try_from(v) else {
            return self.undefined();
        };
        let key = self.str(name);
        object
            .get(self.scope, key.into())
            .unwrap_or_else(|| v8::undefined(self.scope).into())
    }

    fn array(&mut self, v: Self::V) -> Option<Vec<Self::V>> {
        let array = v8::Local::<v8::Array>::try_from(v).ok()?;
        (0..array.length())
            .map(|i| array.get_index(self.scope, i))
            .collect()
    }

    fn stringify(&mut self, v: Self::V) -> Option<String> {
        v.is_object()
            .then(|| v8::json::stringify(self.scope, v))
            .flatten()
            .map(|s| s.to_rust_string_lossy(self.scope))
    }

    fn key(&mut self, v: Self::V) -> Option<Key> {
        let object = v8::Local::<v8::Object>::try_from(v).ok()?;
        let slot = key_slot(self.scope);
        let stored = object.get_private(self.scope, slot)?;
        let bytes = self.bytes(stored)?;
        Key::decode(&bytes)
    }

    fn array_buffer(&mut self, bytes: Vec<u8>) -> Self::V {
        let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
        v8::ArrayBuffer::with_backing_store(self.scope, &store).into()
    }

    fn bool_value(&mut self, b: bool) -> Self::V {
        v8::Boolean::new(self.scope, b).into()
    }

    fn json(&mut self, value: &Value) -> Self::V {
        let text = self.str(&value.to_string());
        v8::json::parse(self.scope, text).unwrap_or_else(|| v8::undefined(self.scope).into())
    }

    fn crypto_key(&mut self, key: Key) -> Self::V {
        let object = v8::Object::new(self.scope);
        object.set_prototype(self.scope, self.proto);
        let usages: Vec<v8::Local<v8::Value>> =
            key.usages.iter().map(|u| self.str(u).into()).collect();
        let usages = v8::Array::new_with_elements(self.scope, &usages);
        let algorithm = self.json(&key.algorithm.to_json());
        let kind = self.str(key.kind()).into();
        let extractable = self.bool_value(key.extractable);
        for (name, value) in [
            ("type", kind),
            ("extractable", extractable),
            ("algorithm", algorithm),
            ("usages", usages.into()),
        ] {
            let name = self.str(name);
            object.define_own_property(self.scope, name.into(), value, v8::PropertyAttribute::READ_ONLY);
        }
        let stored = self.array_buffer(key.encode());
        let slot = key_slot(self.scope);
        object.set_private(self.scope, slot, stored);
        object.into()
    }
}

fn throw<'s>(scope: &mut v8::PinScope<'s, '_>, name: &str, message: &str) {
    let text = v8::String::new(scope, message).unwrap();
    let exception = if name == "TypeError" {
        v8::Exception::type_error(scope, text)
    } else {
        let context = scope.get_current_context();
        let key = v8::String::new(scope, "DOMException").unwrap();
        let name = v8::String::new(scope, name).unwrap();
        context
            .global(scope)
            .get(scope, key.into())
            .and_then(|c| v8::Local::<v8::Function>::try_from(c).ok())
            .and_then(|c| c.new_instance(scope, &[text.into(), name.into()]))
            .map(Into::into)
            .unwrap_or_else(|| v8::Exception::error(scope, text))
    };
    scope.throw_exception(exception);
}

/// subtle(op, args, CryptoKey.prototype)
pub fn subtle<'s>(scope: &mut v8::PinScope<'s, '_>, args: Args<'s>, mut rv: Rv<'s>) {
    let op = args.get(0).to_rust_string_lossy(scope);
    let values: Vec<v8::Local<v8::Value>> = match v8::Local::<v8::Array>::try_from(args.get(1)) {
        Ok(a) => (0..a.length()).filter_map(|i| a.get_index(scope, i)).collect(),
        Err(_) => Vec::new(),
    };
    let result = dispatch(&mut V8 { scope, proto: args.get(2) }, &op, &values);
    match result {
        Ok(v) => rv.set(v),
        Err(e) => throw(scope, e.name, &e.message),
    }
}
