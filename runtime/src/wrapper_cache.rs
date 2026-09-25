//! Classic engine: the COM identity to JS wrapper cache, and the wrappers' weak finalization.
//!
//! Each cached wrapper owns one heap [`WrapperSlot`]: its `DeclarationFFI` (internal field 0
//! points at it) plus a raw weak global handle whose V8 callbacks free the slot. Using the raw
//! handle directly avoids `v8::Weak`'s per-wrap closure box and finalizer map entry.

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::{DeclarationFFI, INSTANCE_CACHE};

#[repr(C)]
struct WeakCallbackInfo {
    _opaque: [u8; 0],
}

// rusty_v8's C++ bindings (`src/binding.cc` of the pinned `v8` crate), linked from its static
// library. These are the calls `v8::Weak` itself is built on.
unsafe extern "C" {
    fn v8__Global__NewWeak(
        isolate: *mut c_void,
        data: *const v8::Data,
        parameter: *const c_void,
        callback: unsafe extern "C" fn(*const WeakCallbackInfo),
    ) -> *const v8::Data;
    fn v8__Global__Reset(data: *const v8::Data);
    fn v8__Local__New(isolate: *mut c_void, other: *const v8::Data) -> *const v8::Data;
    fn v8__WeakCallbackInfo__GetParameter(this: *const WeakCallbackInfo) -> *mut c_void;
    fn v8__WeakCallbackInfo__SetSecondPassCallback(
        this: *const WeakCallbackInfo,
        callback: unsafe extern "C" fn(*const WeakCallbackInfo),
    );
}

/// Per-wrapper state, freed by the second-pass weak callback once V8 collects the wrapper.
pub(crate) struct WrapperSlot {
    ffi: DeclarationFFI,
    /// The weak global handle; null once the first-pass callback has reset it.
    handle: std::cell::Cell<*const v8::Data>,
    key: usize,
}

#[inline]
fn raw_isolate(isolate: &v8::Isolate) -> *mut c_void {
    // `UnsafeRawIsolatePtr` is a transparent wrapper around the `v8::Isolate*`.
    unsafe { std::mem::transmute::<v8::UnsafeRawIsolatePtr, *mut c_void>(isolate.as_raw_isolate_ptr()) }
}

/// The live wrapper cached for COM identity `key`.
#[inline]
pub(crate) fn get<'s>(scope: &v8::PinScope<'s, '_, ()>, key: usize) -> Option<v8::Local<'s, v8::Object>> {
    let handle = INSTANCE_CACHE.with(|cache| {
        cache
            .borrow()
            .get(&key)
            .map(|slot| unsafe { slot.as_ref() }.handle.get())
    })?;
    if handle.is_null() {
        return None;
    }
    let isolate: &v8::Isolate = scope;
    let local = unsafe { v8__Local__New(raw_isolate(isolate), handle) };
    // `Local` is a `#[repr(C)]` non-null handle pointer bound to the current handle scope,
    // which is where `v8__Local__New` allocated it.
    NonNull::new(local as *mut v8::Object)
        .map(|ptr| unsafe { std::mem::transmute::<NonNull<v8::Object>, v8::Local<'s, v8::Object>>(ptr) })
}

/// Cache `object` as the wrapper of COM identity `key` and hand it `ffi`. Returns the address
/// of the stored `DeclarationFFI` for internal field 0; it lives until V8 collects `object`.
pub(crate) fn insert(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<v8::Object>,
    ffi: DeclarationFFI,
    key: usize,
) -> *mut DeclarationFFI {
    let slot = Box::into_raw(Box::new(WrapperSlot {
        ffi,
        handle: std::cell::Cell::new(std::ptr::null()),
        key,
    }));
    let isolate: &mut v8::Isolate = scope.as_mut();
    let handle = unsafe {
        v8__Global__NewWeak(
            raw_isolate(isolate),
            &*object as *const v8::Object as *const v8::Data,
            slot as *const c_void,
            first_pass,
        )
    };
    unsafe { (*slot).handle.set(handle) };
    let size = INSTANCE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        // A replaced entry is a collected wrapper still waiting for its second pass, which
        // then sees it is no longer the cached slot and only frees itself.
        cache.insert(key, unsafe { NonNull::new_unchecked(slot) });
        cache.len()
    });
    crate::maybe_request_gc_nudge(size, isolate);
    unsafe { &mut (*slot).ffi }
}

/// Drop every cache entry before the isolate goes away. Live wrappers get their weak handle
/// reset so V8 never calls back into a slot, and their state is left alive rather than released
/// during teardown. Collected wrappers still waiting
/// for their second pass keep their slot, which that pass frees if it runs.
pub(crate) fn clear() {
    let slots: Vec<NonNull<WrapperSlot>> =
        INSTANCE_CACHE.with(|cache| cache.borrow_mut().drain().map(|(_, slot)| slot).collect());
    for slot in slots {
        let slot = unsafe { slot.as_ref() };
        let handle = slot.handle.replace(std::ptr::null());
        if !handle.is_null() {
            unsafe { v8__Global__Reset(handle) };
        }
    }
}

unsafe extern "C" fn first_pass(info: *const WeakCallbackInfo) {
    // Only resetting the handle is allowed here; the slot is freed in the second pass.
    unsafe {
        let slot = &*(v8__WeakCallbackInfo__GetParameter(info) as *const WrapperSlot);
        let handle = slot.handle.replace(std::ptr::null());
        if !handle.is_null() {
            v8__Global__Reset(handle);
        }
        v8__WeakCallbackInfo__SetSecondPassCallback(info, second_pass);
    }
}

unsafe extern "C" fn second_pass(info: *const WeakCallbackInfo) {
    let slot = unsafe { v8__WeakCallbackInfo__GetParameter(info) } as *mut WrapperSlot;
    let key = unsafe { (*slot).key };
    // Unlinked (or never linked, after `clear`) means nothing else can reach the slot. If the
    // cache is unavailable the slot is leaked instead: a dangling entry would be worse.
    let unlinked = INSTANCE_CACHE
        .try_with(|cache| {
            let Ok(mut cache) = cache.try_borrow_mut() else {
                return false;
            };
            if cache.get(&key).is_some_and(|s| s.as_ptr() == slot) {
                cache.remove(&key);
            }
            true
        })
        .unwrap_or(false);
    if unlinked {
        crate::global_fns::drop_unless_com_teardown(unsafe { Box::from_raw(slot) });
    }
}
