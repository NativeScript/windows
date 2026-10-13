//! A JS struct value passed where WinRT expects `IReference<T>` of a struct, such as the nullable
//! `IReference<Windows.UI.Color>` colors of `AppWindowTitleBar`.
//!
//! `PropertyValue` only boxes primitives and a few Foundation types, so the struct's bytes (laid
//! out as for a struct parameter) are held by an agile object that answers the reference's IID,
//! which is computed from its closed generic name, and copies them out of `get_Value`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use metadata::declarations::declaration::{Declaration, DeclarationKind};
use metadata::meta_data_reader::MetadataReader;
use parking_lot::RwLock;
use windows::core::{Interface, GUID, HRESULT, IUnknown};

type Raw = *mut c_void;

const E_NOINTERFACE: HRESULT = HRESULT(0x80004002u32 as i32);
const E_POINTER: HRESULT = HRESULT(0x80004003u32 as i32);
const S_OK: HRESULT = HRESULT(0);
const IID_IINSPECTABLE: GUID = GUID::from_u128(0xAF86E2E0_B12D_4c6a_9C5A_D7AA65101E90);
const IID_IAGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);

/// The struct declaration for an `IReference<T>`'s `T`, or `None` when `T` isn't a struct.
/// `TimeSpan` and `DateTime` stay with `PropertyValue`, which also accepts a number for them.
pub(crate) fn struct_declaration(inner_type: &str) -> Option<Arc<RwLock<dyn Declaration>>> {
    let name = inner_type.trim();
    if matches!(name, "Windows.Foundation.TimeSpan" | "Windows.Foundation.DateTime") {
        return None;
    }
    let declaration = MetadataReader::find_by_name(crate::helpers::strip_generic_suffix(name))?;
    let is_struct = declaration.read().kind() == DeclarationKind::Struct;
    is_struct.then_some(declaration)
}

#[repr(C)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT,
    add_ref: unsafe extern "system" fn(Raw) -> u32,
    release: unsafe extern "system" fn(Raw) -> u32,
    get_iids: unsafe extern "system" fn(Raw, *mut u32, *mut *mut GUID) -> HRESULT,
    get_runtime_class_name: unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT,
    get_trust_level: unsafe extern "system" fn(Raw, *mut i32) -> HRESULT,
    get_value: unsafe extern "system" fn(Raw, *mut c_void) -> HRESULT,
}

#[repr(C)]
struct StructReference {
    vtbl: &'static Vtbl,
    refs: AtomicU32,
    iid: GUID,
    bytes: Box<[u8]>,
}

unsafe fn from_raw<'a>(this: Raw) -> &'a StructReference {
    &*(this as *const StructReference)
}

unsafe extern "system" fn query_interface(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    let object = from_raw(this);
    let iid = &*iid;
    if *iid == IUnknown::IID || *iid == IID_IINSPECTABLE || *iid == IID_IAGILE_OBJECT || *iid == object.iid {
        object.refs.fetch_add(1, Ordering::Relaxed);
        *out = this;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: Raw) -> u32 {
    from_raw(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: Raw) -> u32 {
    let remaining = from_raw(this).refs.fetch_sub(1, Ordering::Release) - 1;
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(this as *mut StructReference));
    }
    remaining
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

unsafe extern "system" fn get_value(this: Raw, out: *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let bytes = &from_raw(this).bytes;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
    S_OK
}

static VTBL: Vtbl = Vtbl {
    query_interface,
    add_ref,
    release,
    get_iids,
    get_runtime_class_name,
    get_trust_level,
    get_value,
};

/// An `IReference<T>` (`iid`) holding a struct's bytes.
pub(crate) fn create(bytes: Vec<u8>, iid: GUID) -> IUnknown {
    let object = Box::new(StructReference {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        iid,
        bytes: bytes.into_boxed_slice(),
    });
    unsafe { IUnknown::from_raw(Box::into_raw(object) as Raw) }
}
