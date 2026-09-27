//! A JS array passed where WinRT expects `IIterable<T>`, `IVectorView<T>` or `IVector<T>`.
//!
//! The parameter's interface is a closed generic, so its IID is computed from its name (the open
//! generic's IID matches nothing). An array becomes a native vector answering the IIDs of all three
//! with the same `T`, and an `IIterator<T>` from `First`. `T` is a reference type, whose elements
//! are held already QI'd to it, or `String`. The object is agile: a callee may keep and use it from
//! any thread, so the items sit behind a lock.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::DeclarationKind;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::generic_instance_id_builder::GenericInstanceIdBuilder;
use metadata::meta_data_reader::MetadataReader;
use windows::core::{Interface, GUID, HRESULT, HSTRING, IUnknown};

const E_BOUNDS: HRESULT = HRESULT(0x8000000Bu32 as i32);
const E_NOINTERFACE: HRESULT = HRESULT(0x80004002u32 as i32);
const E_POINTER: HRESULT = HRESULT(0x80004003u32 as i32);
const S_OK: HRESULT = HRESULT(0);
const IID_IINSPECTABLE: GUID = GUID::from_u128(0xAF86E2E0_B12D_4c6a_9C5A_D7AA65101E90);
const IID_IAGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);

const COLLECTIONS: &str = "Windows.Foundation.Collections.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Element {
    /// A reference type, held as its interface with this IID.
    Object(GUID),
    String,
}

pub(crate) struct CollectionPlan {
    /// The IID the parameter asks for.
    pub(crate) requested: GUID,
    pub(crate) element: Element,
    iterable: GUID,
    iterator: GUID,
    view: GUID,
    vector: GUID,
}

/// A collection parameter's plan from its closed IID name, e.g.
/// ``Windows.Foundation.Collections.IIterable`1<NativeScript.Mason.IMasonElement>``. None for any
/// other interface, or an element type an array can't carry.
pub(crate) fn plan_for(iid_name: &str) -> Option<Arc<CollectionPlan>> {
    let name = iid_name.replace(", ", ",");
    let rest = name.strip_prefix(COLLECTIONS)?;
    let (open, inner) = rest.split_once("`1<")?;
    let element_name = inner.strip_suffix('>')?;
    if !matches!(open, "IIterable" | "IVectorView" | "IVector") {
        return None;
    }
    let element = element_for(element_name)?;
    let id = |interface: &str| GenericInstanceIdBuilder::generate_id_from_name(&format!("{COLLECTIONS}{interface}`1<{element_name}>"));
    let plan = CollectionPlan {
        requested: GenericInstanceIdBuilder::generate_id_from_name(&name),
        element,
        iterable: id("IIterable"),
        iterator: id("IIterator"),
        view: id("IVectorView"),
        vector: id("IVector"),
    };
    let zero = GUID::zeroed();
    if [plan.requested, plan.iterable, plan.iterator, plan.view, plan.vector].contains(&zero) {
        return None;
    }
    Some(Arc::new(plan))
}

fn element_for(name: &str) -> Option<Element> {
    if name == "String" {
        return Some(Element::String);
    }
    if name == "Object" {
        return Some(Element::Object(IID_IINSPECTABLE));
    }
    if name.contains('<') {
        let iid = GenericInstanceIdBuilder::generate_id_from_name(name);
        return (iid != GUID::zeroed()).then_some(Element::Object(iid));
    }
    let declaration = MetadataReader::find_by_name(name)?;
    let lock = declaration.read();
    let iid = match lock.kind() {
        DeclarationKind::Interface => lock.as_any().downcast_ref::<InterfaceDeclaration>()?.id(),
        DeclarationKind::Class => lock.as_any().downcast_ref::<ClassDeclaration>()?.default_interface()?.id(),
        // Structs, enums and primitives are stored by value: an array of them isn't supported.
        _ => return None,
    };
    Some(Element::Object(iid))
}

/// One element, owned by the collection.
pub(crate) enum Item {
    Object(Option<IUnknown>),
    String(HSTRING),
}

// ── The vector ───────────────────────────────────────────────────────────────────────────────────

type Raw = *mut c_void;

#[repr(C)]
struct InspectableSlots {
    query_interface: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT,
    add_ref: unsafe extern "system" fn(Raw) -> u32,
    release: unsafe extern "system" fn(Raw) -> u32,
    get_iids: unsafe extern "system" fn(Raw, *mut u32, *mut *mut GUID) -> HRESULT,
    get_runtime_class_name: unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT,
    get_trust_level: unsafe extern "system" fn(Raw, *mut i32) -> HRESULT,
}

#[repr(C)]
struct IterableVtbl {
    base: InspectableSlots,
    first: unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT,
}

#[repr(C)]
struct ViewVtbl {
    base: InspectableSlots,
    get_at: unsafe extern "system" fn(Raw, u32, *mut Raw) -> HRESULT,
    get_size: unsafe extern "system" fn(Raw, *mut u32) -> HRESULT,
    index_of: unsafe extern "system" fn(Raw, Raw, *mut u32, *mut u8) -> HRESULT,
    get_many: unsafe extern "system" fn(Raw, u32, u32, *mut Raw, *mut u32) -> HRESULT,
}

#[repr(C)]
struct VectorVtbl {
    base: InspectableSlots,
    get_at: unsafe extern "system" fn(Raw, u32, *mut Raw) -> HRESULT,
    get_size: unsafe extern "system" fn(Raw, *mut u32) -> HRESULT,
    get_view: unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT,
    index_of: unsafe extern "system" fn(Raw, Raw, *mut u32, *mut u8) -> HRESULT,
    set_at: unsafe extern "system" fn(Raw, u32, Raw) -> HRESULT,
    insert_at: unsafe extern "system" fn(Raw, u32, Raw) -> HRESULT,
    remove_at: unsafe extern "system" fn(Raw, u32) -> HRESULT,
    append: unsafe extern "system" fn(Raw, Raw) -> HRESULT,
    remove_at_end: unsafe extern "system" fn(Raw) -> HRESULT,
    clear: unsafe extern "system" fn(Raw) -> HRESULT,
    get_many: unsafe extern "system" fn(Raw, u32, u32, *mut Raw, *mut u32) -> HRESULT,
    replace_all: unsafe extern "system" fn(Raw, u32, *const Raw) -> HRESULT,
}

#[repr(C)]
struct IteratorVtbl {
    base: InspectableSlots,
    get_current: unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT,
    get_has_current: unsafe extern "system" fn(Raw, *mut u8) -> HRESULT,
    move_next: unsafe extern "system" fn(Raw, *mut u8) -> HRESULT,
    get_many: unsafe extern "system" fn(Raw, u32, *mut Raw, *mut u32) -> HRESULT,
}

/// One object behind three interface pointers, one per vtable field.
#[repr(C)]
struct Vector {
    iterable: *const IterableVtbl,
    view: *const ViewVtbl,
    vector: *const VectorVtbl,
    refs: AtomicU32,
    plan: Arc<CollectionPlan>,
    items: Mutex<Vec<Item>>,
}

const VIEW_OFFSET: usize = std::mem::size_of::<*const c_void>();
const VECTOR_OFFSET: usize = 2 * std::mem::size_of::<*const c_void>();

unsafe fn from_iterable<'a>(this: Raw) -> &'a Vector {
    &*(this as *const Vector)
}
unsafe fn from_view<'a>(this: Raw) -> &'a Vector {
    &*((this as *const u8).sub(VIEW_OFFSET) as *const Vector)
}
unsafe fn from_vector<'a>(this: Raw) -> &'a Vector {
    &*((this as *const u8).sub(VECTOR_OFFSET) as *const Vector)
}

impl Vector {
    fn base(&self) -> Raw {
        self as *const Vector as Raw
    }

    unsafe fn query(&self, iid: *const GUID, out: *mut Raw) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        let iid = &*iid;
        let base = self.base() as *mut u8;
        let p = self.plan.as_ref();
        let pointer = if *iid == IUnknown::IID || *iid == IID_IINSPECTABLE || *iid == IID_IAGILE_OBJECT || *iid == p.iterable {
            base
        } else if *iid == p.view {
            base.add(VIEW_OFFSET)
        } else if *iid == p.vector {
            base.add(VECTOR_OFFSET)
        } else {
            *out = std::ptr::null_mut();
            return E_NOINTERFACE;
        };
        self.refs.fetch_add(1, Ordering::Relaxed);
        *out = pointer as Raw;
        S_OK
    }

    unsafe fn add_ref(&self) -> u32 {
        self.refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe fn release(&self) -> u32 {
        let left = self.refs.fetch_sub(1, Ordering::Release) - 1;
        if left == 0 {
            std::sync::atomic::fence(Ordering::Acquire);
            drop(Box::from_raw(self as *const Vector as *mut Vector));
        }
        left
    }

    /// A new reference to item `index` in its ABI form.
    fn read(&self, items: &[Item], index: u32, out: *mut Raw) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        let Some(item) = items.get(index as usize) else {
            return E_BOUNDS;
        };
        unsafe { *out = abi_clone(item) };
        S_OK
    }

    /// Takes a borrowed ABI value as a new owned item.
    unsafe fn adopt(&self, value: Raw) -> Item {
        match self.plan.element {
            Element::String => Item::String(borrow_hstring(value).clone()),
            Element::Object(_) => Item::Object(borrow_unknown(value).cloned()),
        }
    }

    unsafe fn index_of(&self, value: Raw, index: *mut u32, found: *mut u8) -> HRESULT {
        if index.is_null() || found.is_null() {
            return E_POINTER;
        }
        let items = self.items.lock().unwrap();
        let position = match self.plan.element {
            Element::String => {
                let wanted = borrow_hstring(value);
                items.iter().position(|item| matches!(item, Item::String(s) if s == wanted))
            }
            Element::Object(_) => {
                let wanted = identity(borrow_unknown(value));
                items.iter().position(|item| matches!(item, Item::Object(o) if identity(o.as_ref()) == wanted))
            }
        };
        *index = position.unwrap_or(0) as u32;
        *found = position.is_some() as u8;
        S_OK
    }

    unsafe fn get_many(&self, start: u32, capacity: u32, items_out: *mut Raw, actual: *mut u32) -> HRESULT {
        if actual.is_null() || (capacity > 0 && items_out.is_null()) {
            return E_POINTER;
        }
        let items = self.items.lock().unwrap();
        if start as usize > items.len() {
            return E_BOUNDS;
        }
        let count = (items.len() - start as usize).min(capacity as usize);
        for i in 0..count {
            *items_out.add(i) = abi_clone(&items[start as usize + i]);
        }
        *actual = count as u32;
        S_OK
    }
}

/// The element's ABI value with its own reference: an AddRef'd interface or a duplicated HSTRING.
fn abi_clone(item: &Item) -> Raw {
    match item {
        Item::Object(Some(object)) => object.clone().into_raw(),
        Item::Object(None) => std::ptr::null_mut(),
        Item::String(s) => unsafe { std::mem::transmute::<HSTRING, Raw>(s.clone()) },
    }
}

unsafe fn borrow_hstring<'a>(value: Raw) -> &'a HSTRING {
    // HSTRING is a transparent handle; this reads it without taking the caller's reference.
    &*(&value as *const Raw as *const HSTRING)
}

unsafe fn borrow_unknown<'a>(value: Raw) -> Option<&'a IUnknown> {
    if value.is_null() {
        None
    } else {
        Some(&*(&value as *const Raw as *const IUnknown))
    }
}

/// COM identity: the IUnknown pointer every interface of an object QIs to.
fn identity(object: Option<&IUnknown>) -> Raw {
    object.and_then(|o| o.cast::<IUnknown>().ok()).map(|u| u.as_raw()).unwrap_or(std::ptr::null_mut())
}

unsafe extern "system" fn get_iids(_: Raw, count: *mut u32, iids: *mut *mut GUID) -> HRESULT {
    if count.is_null() || iids.is_null() {
        return E_POINTER;
    }
    *count = 0;
    *iids = std::ptr::null_mut();
    S_OK
}

unsafe extern "system" fn get_runtime_class_name(_: Raw, name: *mut Raw) -> HRESULT {
    if name.is_null() {
        return E_POINTER;
    }
    *name = std::ptr::null_mut();
    S_OK
}

unsafe extern "system" fn get_trust_level(_: Raw, level: *mut i32) -> HRESULT {
    if level.is_null() {
        return E_POINTER;
    }
    *level = 0;
    S_OK
}

macro_rules! inspectable_slots {
    ($from:ident) => {{
        unsafe extern "system" fn qi(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
            $from(this).query(iid, out)
        }
        unsafe extern "system" fn add_ref(this: Raw) -> u32 {
            $from(this).add_ref()
        }
        unsafe extern "system" fn release(this: Raw) -> u32 {
            $from(this).release()
        }
        InspectableSlots { query_interface: qi, add_ref, release, get_iids, get_runtime_class_name, get_trust_level }
    }};
}

static ITERABLE_VTBL: IterableVtbl = IterableVtbl {
    base: inspectable_slots!(from_iterable),
    first: {
        unsafe extern "system" fn first(this: Raw, out: *mut Raw) -> HRESULT {
            if out.is_null() {
                return E_POINTER;
            }
            let owner = from_iterable(this);
            owner.add_ref();
            let iterator = Box::new(ItemIterator {
                vtbl: &ITERATOR_VTBL,
                refs: AtomicU32::new(1),
                owner: owner as *const Vector,
                index: AtomicU32::new(0),
            });
            *out = Box::into_raw(iterator) as Raw;
            S_OK
        }
        first
    },
};

unsafe extern "system" fn view_get_at(this: Raw, index: u32, out: *mut Raw) -> HRESULT {
    let v = from_view(this);
    let items = v.items.lock().unwrap();
    v.read(&items, index, out)
}
unsafe extern "system" fn view_get_size(this: Raw, size: *mut u32) -> HRESULT {
    if size.is_null() {
        return E_POINTER;
    }
    *size = from_view(this).items.lock().unwrap().len() as u32;
    S_OK
}
unsafe extern "system" fn view_index_of(this: Raw, value: Raw, index: *mut u32, found: *mut u8) -> HRESULT {
    from_view(this).index_of(value, index, found)
}
unsafe extern "system" fn view_get_many(this: Raw, start: u32, capacity: u32, items: *mut Raw, actual: *mut u32) -> HRESULT {
    from_view(this).get_many(start, capacity, items, actual)
}

static VIEW_VTBL: ViewVtbl = ViewVtbl {
    base: inspectable_slots!(from_view),
    get_at: view_get_at,
    get_size: view_get_size,
    index_of: view_index_of,
    get_many: view_get_many,
};

unsafe extern "system" fn vector_get_at(this: Raw, index: u32, out: *mut Raw) -> HRESULT {
    let v = from_vector(this);
    let items = v.items.lock().unwrap();
    v.read(&items, index, out)
}
unsafe extern "system" fn vector_get_size(this: Raw, size: *mut u32) -> HRESULT {
    if size.is_null() {
        return E_POINTER;
    }
    *size = from_vector(this).items.lock().unwrap().len() as u32;
    S_OK
}
unsafe extern "system" fn vector_get_view(this: Raw, out: *mut Raw) -> HRESULT {
    // A live view of the same items, which is what a caller of GetView may observe anyway.
    let v = from_vector(this);
    let view = v.plan.view;
    v.query(&view, out)
}
unsafe extern "system" fn vector_index_of(this: Raw, value: Raw, index: *mut u32, found: *mut u8) -> HRESULT {
    from_vector(this).index_of(value, index, found)
}
unsafe extern "system" fn vector_set_at(this: Raw, index: u32, value: Raw) -> HRESULT {
    let v = from_vector(this);
    let item = v.adopt(value);
    let mut items = v.items.lock().unwrap();
    match items.get_mut(index as usize) {
        Some(slot) => {
            *slot = item;
            S_OK
        }
        None => E_BOUNDS,
    }
}
unsafe extern "system" fn vector_insert_at(this: Raw, index: u32, value: Raw) -> HRESULT {
    let v = from_vector(this);
    let item = v.adopt(value);
    let mut items = v.items.lock().unwrap();
    if index as usize > items.len() {
        return E_BOUNDS;
    }
    items.insert(index as usize, item);
    S_OK
}
unsafe extern "system" fn vector_remove_at(this: Raw, index: u32) -> HRESULT {
    let mut items = from_vector(this).items.lock().unwrap();
    if index as usize >= items.len() {
        return E_BOUNDS;
    }
    items.remove(index as usize);
    S_OK
}
unsafe extern "system" fn vector_append(this: Raw, value: Raw) -> HRESULT {
    let v = from_vector(this);
    let item = v.adopt(value);
    v.items.lock().unwrap().push(item);
    S_OK
}
unsafe extern "system" fn vector_remove_at_end(this: Raw) -> HRESULT {
    match from_vector(this).items.lock().unwrap().pop() {
        Some(_) => S_OK,
        None => E_BOUNDS,
    }
}
unsafe extern "system" fn vector_clear(this: Raw) -> HRESULT {
    from_vector(this).items.lock().unwrap().clear();
    S_OK
}
unsafe extern "system" fn vector_get_many(this: Raw, start: u32, capacity: u32, items: *mut Raw, actual: *mut u32) -> HRESULT {
    from_vector(this).get_many(start, capacity, items, actual)
}
unsafe extern "system" fn vector_replace_all(this: Raw, count: u32, values: *const Raw) -> HRESULT {
    if count > 0 && values.is_null() {
        return E_POINTER;
    }
    let v = from_vector(this);
    let next: Vec<Item> = (0..count as usize).map(|i| v.adopt(*values.add(i))).collect();
    *v.items.lock().unwrap() = next;
    S_OK
}

static VECTOR_VTBL: VectorVtbl = VectorVtbl {
    base: inspectable_slots!(from_vector),
    get_at: vector_get_at,
    get_size: vector_get_size,
    get_view: vector_get_view,
    index_of: vector_index_of,
    set_at: vector_set_at,
    insert_at: vector_insert_at,
    remove_at: vector_remove_at,
    append: vector_append,
    remove_at_end: vector_remove_at_end,
    clear: vector_clear,
    get_many: vector_get_many,
    replace_all: vector_replace_all,
};

// ── The iterator ─────────────────────────────────────────────────────────────────────────────────

#[repr(C)]
struct ItemIterator {
    vtbl: *const IteratorVtbl,
    refs: AtomicU32,
    /// Holds a reference to the vector.
    owner: *const Vector,
    index: AtomicU32,
}

unsafe fn iterator<'a>(this: Raw) -> &'a ItemIterator {
    &*(this as *const ItemIterator)
}

unsafe fn owner<'a>(it: &ItemIterator) -> &'a Vector {
    &*it.owner
}

unsafe extern "system" fn iterator_qi(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let it = iterator(this);
    let iid = &*iid;
    if *iid == IUnknown::IID || *iid == IID_IINSPECTABLE || *iid == IID_IAGILE_OBJECT || *iid == owner(it).plan.iterator {
        it.refs.fetch_add(1, Ordering::Relaxed);
        *out = this;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}
unsafe extern "system" fn iterator_add_ref(this: Raw) -> u32 {
    iterator(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn iterator_release(this: Raw) -> u32 {
    let it = iterator(this);
    let left = it.refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        owner(it).release();
        drop(Box::from_raw(this as *mut ItemIterator));
    }
    left
}
unsafe extern "system" fn iterator_get_current(this: Raw, out: *mut Raw) -> HRESULT {
    let it = iterator(this);
    let owner = owner(it);
    let items = owner.items.lock().unwrap();
    owner.read(&items, it.index.load(Ordering::Relaxed), out)
}
unsafe extern "system" fn iterator_get_has_current(this: Raw, out: *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let it = iterator(this);
    let len = owner(it).items.lock().unwrap().len();
    *out = ((it.index.load(Ordering::Relaxed) as usize) < len) as u8;
    S_OK
}
unsafe extern "system" fn iterator_move_next(this: Raw, out: *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let it = iterator(this);
    let len = owner(it).items.lock().unwrap().len();
    let index = it.index.load(Ordering::Relaxed) as usize;
    if index < len {
        it.index.store(index as u32 + 1, Ordering::Relaxed);
    }
    *out = ((index + 1) < len) as u8;
    S_OK
}
unsafe extern "system" fn iterator_get_many(this: Raw, capacity: u32, items: *mut Raw, actual: *mut u32) -> HRESULT {
    let it = iterator(this);
    let start = it.index.load(Ordering::Relaxed);
    let hr = owner(it).get_many(start, capacity, items, actual);
    if hr == S_OK {
        it.index.store(start + *actual, Ordering::Relaxed);
    }
    hr
}

static ITERATOR_VTBL: IteratorVtbl = IteratorVtbl {
    base: InspectableSlots {
        query_interface: iterator_qi,
        add_ref: iterator_add_ref,
        release: iterator_release,
        get_iids,
        get_runtime_class_name,
        get_trust_level,
    },
    get_current: iterator_get_current,
    get_has_current: iterator_get_has_current,
    move_next: iterator_move_next,
    get_many: iterator_get_many,
};

/// The collection as the interface the parameter asked for, holding the only reference.
pub(crate) fn new_collection(plan: &Arc<CollectionPlan>, items: Vec<Item>) -> IUnknown {
    let vector = Box::new(Vector {
        iterable: &ITERABLE_VTBL,
        view: &VIEW_VTBL,
        vector: &VECTOR_VTBL,
        refs: AtomicU32::new(1),
        plan: plan.clone(),
        items: Mutex::new(items),
    });
    let base = Box::into_raw(vector) as *mut u8;
    let offset = if plan.requested == plan.view {
        VIEW_OFFSET
    } else if plan.requested == plan.vector {
        VECTOR_OFFSET
    } else {
        0
    };
    unsafe { IUnknown::from_raw(base.add(offset) as Raw) }
}

unsafe impl Send for Vector {}
unsafe impl Sync for Vector {}

// ── Engine glue ──────────────────────────────────────────────────────────────────────────────────

/// A collection argument for the classic engine: the array's items, or the value QI'd as usual.
#[cfg(feature = "classic")]
pub(crate) fn classic_arg(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
    plan: &Arc<CollectionPlan>,
) -> Result<(crate::value::NativeValue, Option<IUnknown>), crate::error::AnyError> {
    let Ok(array) = v8::Local::<v8::Array>::try_from(value) else {
        return crate::value::ffi_parse_query_interface_arg(scope, value, &plan.requested);
    };
    let mut items = Vec::with_capacity(array.length() as usize);
    for i in 0..array.length() {
        let element = array.get_index(scope, i).unwrap_or_else(|| v8::undefined(scope).into());
        items.push(match plan.element {
            Element::String => {
                let text = element.to_string(scope).map(|s| s.to_rust_string_lossy(scope)).unwrap_or_default();
                Item::String(HSTRING::from(text))
            }
            Element::Object(iid) => {
                let (pointer, guard) = crate::value::ffi_parse_query_interface_arg(scope, element, &iid)?;
                object_item(unsafe { pointer.pointer }, guard)?
            }
        });
    }
    let collection = new_collection(plan, items);
    let pointer = collection.as_raw();
    Ok((crate::value::NativeValue { pointer }, Some(collection)))
}

/// A collection argument for the napi engine: the array's items, or the value QI'd as usual.
#[cfg(feature = "napi_engine")]
pub(crate) fn napi_arg(
    env: &napi::Env,
    value: &napi::JsUnknown,
    plan: &Arc<CollectionPlan>,
) -> Result<(crate::value::NativeValue, Option<IUnknown>), crate::error::AnyError> {
    use napi::sys;
    use napi::{NapiRaw, NapiValue};
    let raw_env = env.raw();
    let raw = unsafe { value.raw() };
    let mut is_array = false;
    let mut length = 0u32;
    let array = unsafe {
        sys::napi_is_array(raw_env, raw, &mut is_array) == sys::Status::napi_ok
            && is_array
            && sys::napi_get_array_length(raw_env, raw, &mut length) == sys::Status::napi_ok
    };
    if !array {
        return crate::napi_engine::value::napi_parse_query_interface(env, value, &plan.requested);
    }
    let mut items = Vec::with_capacity(length as usize);
    for i in 0..length {
        let mut element: sys::napi_value = std::ptr::null_mut();
        if unsafe { sys::napi_get_element(raw_env, raw, i, &mut element) } != sys::Status::napi_ok {
            return Err(crate::error::type_error("Couldn't read an element of the array argument"));
        }
        let element = unsafe { napi::JsUnknown::from_raw_unchecked(raw_env, element) };
        items.push(match plan.element {
            Element::String => Item::String(HSTRING::from(crate::napi_engine::value::js_to_rust_string(env, &element))),
            Element::Object(iid) => {
                let (pointer, guard) = crate::napi_engine::value::napi_parse_query_interface(env, &element, &iid)?;
                object_item(unsafe { pointer.pointer }, guard)?
            }
        });
    }
    let collection = new_collection(plan, items);
    let pointer = collection.as_raw();
    Ok((crate::value::NativeValue { pointer }, Some(collection)))
}

/// An element QI'd to `T`: null stays null; anything that isn't a WinRT object is an error.
fn object_item(pointer: Raw, guard: Option<IUnknown>) -> Result<Item, crate::error::AnyError> {
    match guard {
        Some(object) => Ok(Item::Object(Some(object))),
        None if pointer.is_null() => Ok(Item::Object(None)),
        None => Err(crate::error::type_error("Array element is not the collection's WinRT element type")),
    }
}
