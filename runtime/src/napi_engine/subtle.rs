//! `crypto.subtle` for napi engines: `__nsSubtle(op, args, CryptoKey.prototype)`.

use crate::napi_engine::value::buffer_data;
use crate::subtle::{dispatch, Js as Engine, Key};
use napi::{
    sys, CallContext, Env, JsFunction, JsObject, JsUnknown, NapiRaw, NapiValue, Property, PropertyAttributes,
    Status, ValueType,
};
use serde_json::Value;

struct Napi<'e> {
    env: &'e Env,
    proto: sys::napi_value,
}

impl Napi<'_> {
    fn value(&self, raw: sys::napi_value) -> JsUnknown {
        unsafe { JsUnknown::from_raw_unchecked(self.env.raw(), raw) }
    }

    fn object(&self, raw: sys::napi_value) -> Option<JsObject> {
        matches!(self.value(raw).get_type(), Ok(ValueType::Object | ValueType::Function))
            .then(|| unsafe { JsObject::from_raw_unchecked(self.env.raw(), raw) })
    }

    fn call_global(&self, object: &str, method: &str, arg: sys::napi_value) -> Option<JsUnknown> {
        let target: JsObject = self.env.get_global().ok()?.get_named_property(object).ok()?;
        let f: JsFunction = target.get_named_property(method).ok()?;
        let out = f.call(Some(&target), &[self.value(arg)]);
        if out.is_err() {
            let mut exception = std::ptr::null_mut();
            unsafe { sys::napi_get_and_clear_last_exception(self.env.raw(), &mut exception) };
        }
        out.ok()
    }
}

impl Engine for Napi<'_> {
    type V = sys::napi_value;

    fn undefined(&mut self) -> Self::V {
        self.env.get_undefined().map(|u| unsafe { u.raw() }).unwrap_or(std::ptr::null_mut())
    }

    fn is_nullish(&mut self, v: Self::V) -> bool {
        matches!(self.value(v).get_type(), Ok(ValueType::Undefined | ValueType::Null))
    }

    fn string(&mut self, v: Self::V) -> Option<String> {
        let value = self.value(v);
        if value.get_type().ok()? != ValueType::String {
            return None;
        }
        Some(value.coerce_to_string().ok()?.into_utf8().ok()?.as_str().ok()?.to_owned())
    }

    fn number(&mut self, v: Self::V) -> Option<f64> {
        let value = self.value(v);
        if value.get_type().ok()? != ValueType::Number {
            return None;
        }
        value.coerce_to_number().ok()?.get_double().ok()
    }

    fn boolean(&mut self, v: Self::V) -> bool {
        self.value(v).coerce_to_bool().and_then(|b| b.get_value()).unwrap_or(false)
    }

    fn bytes(&mut self, v: Self::V) -> Option<Vec<u8>> {
        let (data, len) = buffer_data(self.env, &self.value(v))?;
        Some(if len == 0 || data.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data as *const u8, len).to_vec() }
        })
    }

    fn get(&mut self, v: Self::V, name: &str) -> Self::V {
        self.object(v)
            .and_then(|o| o.get_named_property::<JsUnknown>(name).ok())
            .map(|value| unsafe { value.raw() })
            .unwrap_or_else(|| self.undefined())
    }

    fn array(&mut self, v: Self::V) -> Option<Vec<Self::V>> {
        let array = self.object(v)?;
        if !array.is_array().ok()? {
            return None;
        }
        (0..array.get_array_length().ok()?)
            .map(|i| array.get_element::<JsUnknown>(i).ok().map(|e| unsafe { e.raw() }))
            .collect()
    }

    fn stringify(&mut self, v: Self::V) -> Option<String> {
        self.object(v)?;
        let text = self.call_global("JSON", "stringify", v)?;
        self.string(unsafe { text.raw() })
    }

    fn key(&mut self, v: Self::V) -> Option<Key> {
        let object = self.object(v)?;
        self.env.unwrap::<Key>(&object).ok().map(|key| key.clone())
    }

    fn array_buffer(&mut self, bytes: Vec<u8>) -> Self::V {
        self.env
            .create_arraybuffer_with_data(bytes)
            .map(|b| unsafe { b.into_raw().raw() })
            .unwrap_or_else(|_| self.undefined())
    }

    fn bool_value(&mut self, b: bool) -> Self::V {
        self.env.get_boolean(b).map(|b| unsafe { b.raw() }).unwrap_or_else(|_| self.undefined())
    }

    fn json(&mut self, value: &Value) -> Self::V {
        let Ok(text) = self.env.create_string(&value.to_string()) else {
            return self.undefined();
        };
        self.call_global("JSON", "parse", unsafe { text.raw() })
            .map(|v| unsafe { v.raw() })
            .unwrap_or_else(|| self.undefined())
    }

    fn crypto_key(&mut self, key: Key) -> Self::V {
        let algorithm = self.json(&key.algorithm.to_json());
        let created = self.call_global("Object", "create", self.proto);
        let mut object = match created.and_then(|o| self.object(unsafe { o.raw() })) {
            Some(o) => o,
            None => match self.env.create_object() {
                Ok(o) => o,
                Err(_) => return self.undefined(),
            },
        };
        let built = (|| -> napi::Result<()> {
            let mut usages = self.env.create_array_with_length(key.usages.len())?;
            for (i, usage) in key.usages.iter().enumerate() {
                usages.set_element(i as u32, self.env.create_string(usage)?)?;
            }
            let kind = self.env.create_string(key.kind())?;
            let extractable = self.env.get_boolean(key.extractable)?;
            let algorithm = self.value(algorithm);
            object.define_properties(&[
                Property::new("type")?.with_value(&kind).with_property_attributes(PropertyAttributes::Enumerable),
                Property::new("extractable")?
                    .with_value(&extractable)
                    .with_property_attributes(PropertyAttributes::Enumerable),
                Property::new("algorithm")?
                    .with_value(&algorithm)
                    .with_property_attributes(PropertyAttributes::Enumerable),
                Property::new("usages")?.with_value(&usages).with_property_attributes(PropertyAttributes::Enumerable),
            ])?;
            self.env.wrap(&mut object, key)
        })();
        if built.is_err() {
            return self.undefined();
        }
        unsafe { object.raw() }
    }
}

fn throw(env: &Env, name: &str, message: &str) -> napi::Error {
    let thrown = (|| -> napi::Result<()> {
        let global = env.get_global()?;
        let text = env.create_string(message)?;
        let error = if name == "TypeError" {
            let ctor: JsFunction = global.get_named_property("TypeError")?;
            ctor.new_instance(&[text])?
        } else {
            let ctor: JsFunction = global.get_named_property("DOMException")?;
            ctor.new_instance(&[text.into_unknown(), env.create_string(name)?.into_unknown()])?
        };
        env.throw(error)
    })();
    match thrown {
        Ok(()) => napi::Error::new(Status::PendingException, String::new()),
        Err(_) => napi::Error::from_reason(format!("{name}: {message}")),
    }
}

pub(crate) fn call(ctx: CallContext) -> napi::Result<JsUnknown> {
    let op = ctx.get::<JsUnknown>(0)?.coerce_to_string()?.into_utf8()?.as_str()?.to_owned();
    let args = unsafe { ctx.get::<JsUnknown>(1)?.raw() };
    let proto = unsafe { ctx.get::<JsUnknown>(2)?.raw() };
    let mut engine = Napi { env: ctx.env, proto };
    let args = engine.array(args).unwrap_or_default();
    match dispatch(&mut engine, &op, &args) {
        Ok(v) => Ok(unsafe { JsUnknown::from_raw_unchecked(ctx.env.raw(), v) }),
        Err(e) => Err(throw(ctx.env, e.name, &e.message)),
    }
}
