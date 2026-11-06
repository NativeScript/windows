//! A JS delegate passed where WinRT expects an `Object` must arrive boxed as `IReference<D>`:
//! `UIElement.AddHandler` unboxes its handler that way. Each JS delegate embeds a `DelegateBox`
//! tear-off that answers `IInspectable` and `IReference<D>` and returns the delegate from
//! `get_Value`. Its IUnknown methods forward to the delegate, so COM identity is the delegate's.

use std::ffi::c_void;
use std::sync::OnceLock;

use metadata::generic_instance_id_builder::GenericInstanceIdBuilder;
use windows::core::{IInspectable, Interface, GUID, HRESULT};

const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002u32 as i32);
const E_POINTER: HRESULT = HRESULT(0x8000_4003u32 as i32);

#[repr(C)]
struct DelegateBoxVtbl {
    query_interface:
        unsafe extern "system" fn(*mut DelegateBox, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut DelegateBox) -> u32,
    release: unsafe extern "system" fn(*mut DelegateBox) -> u32,
    get_iids: unsafe extern "system" fn(*mut DelegateBox, *mut u32, *mut *mut GUID) -> HRESULT,
    get_runtime_class_name: unsafe extern "system" fn(*mut DelegateBox, *mut *mut c_void) -> HRESULT,
    get_trust_level: unsafe extern "system" fn(*mut DelegateBox, *mut i32) -> HRESULT,
    get_value: unsafe extern "system" fn(*mut DelegateBox, *mut *mut c_void) -> HRESULT,
}

static DELEGATE_BOX_VTBL: DelegateBoxVtbl = DelegateBoxVtbl {
    query_interface: box_query_interface,
    add_ref: box_add_ref,
    release: box_release,
    get_iids: box_get_iids,
    get_runtime_class_name: box_get_runtime_class_name,
    get_trust_level: box_get_trust_level,
    get_value: box_get_value,
};

#[repr(C)]
pub(crate) struct DelegateBox {
    vtable: *const DelegateBoxVtbl,
    /// The delegate this box is part of; set once the delegate has its final address.
    owner: *mut c_void,
    delegate_iid: GUID,
    reference_iid: OnceLock<GUID>,
}

impl DelegateBox {
    pub(crate) fn new(delegate_iid: GUID) -> Self {
        Self {
            vtable: &DELEGATE_BOX_VTBL,
            owner: std::ptr::null_mut(),
            delegate_iid,
            reference_iid: OnceLock::new(),
        }
    }

    /// Ties the box to the delegate that contains it.
    pub(crate) fn set_owner(&mut self, owner: *mut c_void) {
        self.owner = owner;
    }

    /// The box's interface pointer when the owning delegate is asked for `iid` and the box
    /// implements it. The caller AddRefs the delegate.
    pub(crate) fn query(&self, iid: &GUID) -> Option<*mut c_void> {
        let reference_iid = self
            .reference_iid
            .get_or_init(|| GenericInstanceIdBuilder::ireference_of_delegate(&self.delegate_iid));
        (*iid == IInspectable::IID || iid == reference_iid)
            .then(|| self as *const Self as *mut c_void)
    }
}

/// The owning delegate's IUnknown vtable.
unsafe fn owner_vtable(this: *mut DelegateBox) -> &'static windows::core::IUnknown_Vtbl {
    &**((*this).owner as *const *const windows::core::IUnknown_Vtbl)
}

unsafe extern "system" fn box_query_interface(
    this: *mut DelegateBox,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if iid.is_null() {
        *out = std::ptr::null_mut();
        return E_NOINTERFACE;
    }
    (owner_vtable(this).QueryInterface)((*this).owner, iid, out)
}

unsafe extern "system" fn box_add_ref(this: *mut DelegateBox) -> u32 {
    (owner_vtable(this).AddRef)((*this).owner)
}

unsafe extern "system" fn box_release(this: *mut DelegateBox) -> u32 {
    (owner_vtable(this).Release)((*this).owner)
}

unsafe extern "system" fn box_get_iids(
    _this: *mut DelegateBox,
    count: *mut u32,
    iids: *mut *mut GUID,
) -> HRESULT {
    if count.is_null() || iids.is_null() {
        return E_POINTER;
    }
    *count = 0;
    *iids = std::ptr::null_mut();
    HRESULT(0)
}

unsafe extern "system" fn box_get_runtime_class_name(
    _this: *mut DelegateBox,
    name: *mut *mut c_void,
) -> HRESULT {
    if name.is_null() {
        return E_POINTER;
    }
    // The empty HSTRING.
    *name = std::ptr::null_mut();
    HRESULT(0)
}

unsafe extern "system" fn box_get_trust_level(_this: *mut DelegateBox, level: *mut i32) -> HRESULT {
    if level.is_null() {
        return E_POINTER;
    }
    *level = 0;
    HRESULT(0)
}

unsafe extern "system" fn box_get_value(this: *mut DelegateBox, value: *mut *mut c_void) -> HRESULT {
    if value.is_null() {
        return E_POINTER;
    }
    let owner = (*this).owner;
    (owner_vtable(this).AddRef)(owner);
    *value = owner;
    HRESULT(0)
}
