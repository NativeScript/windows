//! WinRT value structs (classic engine): plain JS snapshots built from a per-isolate,
//! per-type template, so all values of a type share one map and the constructor's prototype.

use crate::helpers::strip_generic_suffix;
use crate::property_call::{align_up, sig_size_align_pub};
use crate::value::{read_value_from_ptr, NativeType};
use metadata::declarations::declaration::{Declaration, DeclarationKind};
use metadata::declarations::struct_declaration::StructDeclaration;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;
use parking_lot::RwLock;
use std::cell::{OnceCell, RefCell};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::Arc;
use windows::core::HSTRING;

const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;

enum FieldKind {
    /// Primitive, HSTRING, or enum (as Int32).
    Value(NativeType),
    Struct(Rc<StructType>),
    /// Not marshaled (e.g. `Guid`, `IReference<T>`); exposed as `null`.
    Opaque,
}

struct Field {
    /// Internalized.
    key: v8::Global<v8::String>,
    name: String,
    offset: usize,
    kind: FieldKind,
}

/// Layout and templates for one struct type. The cache keeps it alive for the constructor's raw pointer.
pub(crate) struct StructType {
    name: String,
    fields: Vec<Field>,
    tmpl: OnceCell<v8::Global<v8::FunctionTemplate>>,
    instance_tmpl: OnceCell<v8::Global<v8::ObjectTemplate>>,
}

/// Per-isolate struct types by full name (isolate slot).
#[derive(Default)]
pub(crate) struct StructCache {
    types: RefCell<ahash::AHashMap<String, Rc<StructType>>>,
}

fn cache(scope: &mut v8::PinScope<'_, '_>) -> &'static StructCache {
    if scope.get_slot::<StructCache>().is_none() {
        scope.set_slot(StructCache::default());
    }
    // Slot lives as long as the isolate; detach the borrow from `scope`.
    let cache = scope.get_slot::<StructCache>().unwrap();
    unsafe { &*(cache as *const StructCache) }
}

fn struct_type_for(
    scope: &mut v8::PinScope<'_, '_>,
    declaration: &Arc<RwLock<dyn Declaration>>,
) -> Option<Rc<StructType>> {
    let lock = declaration.read();
    let cache = cache(scope);
    if let Some(ty) = cache.types.borrow().get(lock.full_name()) {
        return Some(Rc::clone(ty));
    }
    let sd = lock.as_any().downcast_ref::<StructDeclaration>()?;
    let full_name = lock.full_name().to_string();
    let simple_name = lock.name().to_string();

    let mut fields = Vec::with_capacity(sd.fields().len());
    let mut offset = 0usize;
    for f in sd.fields().iter() {
        let Some(m) = f.base().metadata() else {
            continue;
        };
        let sig = Signature::to_string(m, &f.type_());
        // Same layout as the serializer.
        let (size, align) = sig_size_align_pub(&sig);
        offset = align_up(offset, align);

        let kind = if sig.contains('.') {
            let nested = MetadataReader::find_by_name(strip_generic_suffix(&sig));
            let nested_kind = nested.as_ref().map(|d| d.read().kind());
            match (nested, nested_kind) {
                (Some(d), Some(DeclarationKind::Struct)) => struct_type_for(scope, &d)
                    .map(FieldKind::Struct)
                    .unwrap_or(FieldKind::Opaque),
                (_, Some(DeclarationKind::Enum)) => FieldKind::Value(NativeType::I32),
                _ => FieldKind::Opaque,
            }
        } else {
            match NativeType::try_from(sig.as_str()) {
                Ok(NativeType::Pointer | NativeType::Buffer | NativeType::Function)
                | Ok(NativeType::Struct(_) | NativeType::Void)
                | Err(_) => FieldKind::Opaque,
                Ok(nt) => FieldKind::Value(nt),
            }
        };

        let key =
            v8::String::new_from_utf8(scope, f.name().as_bytes(), v8::NewStringType::Internalized)?;
        fields.push(Field {
            key: v8::Global::new(scope, key),
            name: f.name().to_string(),
            offset,
            kind,
        });
        offset += size;
    }
    drop(lock);

    let ty = Rc::new(StructType {
        name: simple_name,
        fields,
        tmpl: OnceCell::new(),
        instance_tmpl: OnceCell::new(),
    });
    let data = v8::External::new(scope, Rc::as_ptr(&ty) as *mut c_void);
    let tmpl = v8::FunctionTemplate::builder(construct)
        .data(data.into())
        .build(scope);
    if let Some(class_name) = v8::String::new(scope, &ty.name) {
        tmpl.set_class_name(class_name);
    }
    let instance_tmpl = tmpl.instance_template(scope);
    for field in ty.fields.iter() {
        let key = v8::Local::new(scope, &field.key);
        let placeholder: v8::Local<v8::Data> = match field.kind {
            FieldKind::Value(NativeType::String) => v8::String::empty(scope).into(),
            FieldKind::Value(NativeType::Bool) => v8::Boolean::new(scope, false).into(),
            FieldKind::Value(_) => v8::Integer::new(scope, 0).into(),
            FieldKind::Struct(_) | FieldKind::Opaque => v8::null(scope).into(),
        };
        instance_tmpl.set(key.into(), placeholder);
    }
    let tag = v8::Symbol::get_to_string_tag(scope);
    if let Some(tag_value) = v8::String::new(scope, &full_name) {
        tmpl.prototype_template(scope).set_with_attr(
            tag.into(),
            tag_value.into(),
            v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
        );
    }
    let _ = ty.tmpl.set(v8::Global::new(scope, tmpl));
    let _ = ty.instance_tmpl.set(v8::Global::new(scope, instance_tmpl));

    cache.types.borrow_mut().insert(full_name, Rc::clone(&ty));
    Some(ty)
}

fn instantiate<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    ty: &StructType,
) -> Option<v8::Local<'a, v8::Object>> {
    let tmpl = v8::Local::new(scope, ty.instance_tmpl.get()?);
    tmpl.new_instance(scope)
}

/// The JS constructor for a struct type (`Windows.Foundation.Rect`).
pub(crate) fn constructor<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    declaration: &Arc<RwLock<dyn Declaration>>,
) -> v8::Local<'a, v8::Value> {
    struct_type_for(scope, declaration)
        .and_then(|ty| {
            let tmpl = v8::Local::new(scope, ty.tmpl.get()?);
            tmpl.get_function(scope)
        })
        .map(Into::into)
        .unwrap_or_else(|| v8::undefined(scope).into())
}

/// Copies a native struct into a JS value. `raw` is borrowed (HSTRINGs are not released); null gives zeros.
pub(crate) fn from_raw<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    declaration: &Arc<RwLock<dyn Declaration>>,
    raw: *const c_void,
) -> Option<v8::Local<'a, v8::Object>> {
    let ty = struct_type_for(scope, declaration)?;
    read_struct(scope, &ty, raw as *const u8)
}

/// Serializes `obj` into `scratch` (must outlive the call) and returns its address.
pub(crate) fn serialize_into(
    scope: &mut v8::PinScope<'_, '_>,
    declaration: &Arc<RwLock<dyn Declaration>>,
    obj: v8::Local<v8::Object>,
    scratch: &mut Vec<Vec<u8>>,
) -> *mut c_void {
    let mut buf = Vec::new();
    {
        let lock = declaration.read();
        if let Some(sd) = lock.as_any().downcast_ref::<StructDeclaration>() {
            crate::property_call::append_struct_object_bytes(&mut buf, scope, obj, sd);
        }
    }
    if buf.is_empty() {
        buf.push(0);
    }
    let ptr = buf.as_mut_ptr() as *mut c_void;
    scratch.push(buf);
    ptr
}

pub(crate) fn type_name_declaration() -> Option<Arc<RwLock<dyn Declaration>>> {
    MetadataReader::find_by_name("Windows.UI.Xaml.Interop.TypeName")
}

fn read_struct<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    ty: &StructType,
    raw: *const u8,
) -> Option<v8::Local<'a, v8::Object>> {
    let obj = instantiate(scope, ty)?;
    if raw.is_null() {
        return Some(obj);
    }
    for field in ty.fields.iter() {
        let ptr = unsafe { raw.add(field.offset) };
        let value: v8::Local<v8::Value> = match &field.kind {
            FieldKind::Value(NativeType::String) => unsafe {
                let raw_hstring = std::ptr::read_unaligned(ptr as *const usize);
                let hstring = ManuallyDrop::new(std::mem::transmute::<usize, HSTRING>(raw_hstring));
                crate::value::two_byte_string(scope, &hstring).into()
            },
            FieldKind::Value(nt) => unsafe {
                read_value_from_ptr(ptr as *const c_void, scope, nt.clone())
            },
            FieldKind::Struct(nested) => match read_struct(scope, nested, ptr) {
                Some(v) => v.into(),
                None => continue,
            },
            FieldKind::Opaque => continue,
        };
        let key = v8::Local::new(scope, &field.key);
        obj.set(scope, key.into(), value);
    }
    Some(obj)
}

/// `new Rect({ X, Y, Width, Height })`, `new Rect(x, y, w, h)` or `new Rect()`; missing fields are zero.
fn construct<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s, v8::Value>,
) {
    let Ok(data) = v8::Local::<v8::External>::try_from(args.data()) else {
        return;
    };
    let ty = unsafe { &*(data.value() as *const StructType) };

    let this = if args.new_target().is_undefined() {
        match instantiate(scope, ty) {
            Some(obj) => obj,
            None => return,
        }
    } else {
        args.this()
    };

    // One object argument: named fields; otherwise positional.
    let first = args.get(0);
    let named = if args.length() == 1 && first.is_object() {
        first.to_object(scope)
    } else {
        None
    };
    let filled = fill_fields(scope, ty, this, |scope, i, key| match named {
        Some(obj) => obj.get(scope, key.into()),
        None => Some(args.get(i as i32)),
    });
    if filled.is_some() {
        rv.set(this.into());
    }
}

/// Coerces and stores each field; `None` means a TypeError was thrown.
fn fill_fields<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    ty: &StructType,
    target: v8::Local<'a, v8::Object>,
    mut source: impl FnMut(
        &mut v8::PinScope<'a, '_>,
        usize,
        v8::Local<'a, v8::String>,
    ) -> Option<v8::Local<'a, v8::Value>>,
) -> Option<()> {
    for (i, field) in ty.fields.iter().enumerate() {
        let key = v8::Local::new(scope, &field.key);
        let input = source(scope, i, key)?;
        let value = coerce_field(scope, ty, field, input)?;
        target.set(scope, key.into(), value)?;
    }
    Some(())
}

fn coerce_field<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    owner: &StructType,
    field: &Field,
    input: v8::Local<'a, v8::Value>,
) -> Option<v8::Local<'a, v8::Value>> {
    let missing = input.is_null_or_undefined();
    match &field.kind {
        FieldKind::Value(NativeType::Bool) => {
            Some(v8::Boolean::new(scope, !missing && input.boolean_value(scope)).into())
        }
        FieldKind::Value(NativeType::String) => {
            if missing {
                Some(v8::String::empty(scope).into())
            } else if input.is_string() {
                Some(input)
            } else {
                type_error(scope, owner, field, "a string")
            }
        }
        // BigInt stays BigInt only beyond the safe-integer range, as the reader does.
        FieldKind::Value(nt @ (NativeType::I64 | NativeType::U64)) if input.is_big_int() => {
            let bi = v8::Local::<v8::BigInt>::try_from(input).ok()?;
            let n = match nt {
                NativeType::U64 => bi.u64_value().0 as f64,
                _ => bi.i64_value().0 as f64,
            };
            if n.abs() <= MAX_SAFE_INTEGER {
                Some(v8::Number::new(scope, n).into())
            } else {
                Some(input)
            }
        }
        FieldKind::Value(nt) => {
            if missing {
                return Some(v8::Integer::new(scope, 0).into());
            }
            if !input.is_number() {
                return type_error(scope, owner, field, "a number");
            }
            let n = input.number_value(scope).unwrap_or(0.0);
            // Same casts as the serializer.
            let n = match nt {
                NativeType::F32 => n as f32 as f64,
                NativeType::I32 => n as i32 as f64,
                NativeType::U32 => n as u32 as f64,
                NativeType::I16 => n as i16 as f64,
                NativeType::U16 => n as u16 as f64,
                NativeType::I8 => n as i8 as f64,
                NativeType::U8 => n as u8 as f64,
                NativeType::I64 => n as i64 as f64,
                NativeType::U64 => n as u64 as f64,
                _ => n,
            };
            Some(v8::Number::new(scope, n).into())
        }
        FieldKind::Struct(nested) => {
            let obj = instantiate(scope, nested)?;
            if missing {
                return Some(obj.into());
            }
            let Some(src) = input.to_object(scope).filter(|_| input.is_object()) else {
                return type_error(scope, owner, field, "an object");
            };
            fill_fields(scope, nested, obj, |scope, _, key| src.get(scope, key.into()))?;
            Some(obj.into())
        }
        FieldKind::Opaque => Some(if missing { v8::null(scope).into() } else { input }),
    }
}

fn type_error<'a, T>(
    scope: &mut v8::PinScope<'a, '_>,
    owner: &StructType,
    field: &Field,
    expected: &str,
) -> Option<T> {
    let message = format!("{}.{} expects {}", owner.name, field.name, expected);
    if let Some(message) = v8::String::new(scope, &message) {
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
    }
    None
}
