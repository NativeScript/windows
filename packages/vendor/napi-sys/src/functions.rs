#![allow(clippy::too_many_arguments)]

mod napi1 {
  use super::super::types::*;
  use std::os::raw::{c_char, c_void};

  generate!(
    extern "C" {
      fn napi_get_last_error_info(
        env: napi_env,
        result: *mut *const napi_extended_error_info,
      ) -> napi_status;

      fn napi_get_undefined(env: napi_env, result: *mut napi_value) -> napi_status;
      fn napi_get_null(env: napi_env, result: *mut napi_value) -> napi_status;
      fn napi_get_global(env: napi_env, result: *mut napi_value) -> napi_status;
      fn napi_get_boolean(env: napi_env, value: bool, result: *mut napi_value) -> napi_status;
      fn napi_create_object(env: napi_env, result: *mut napi_value) -> napi_status;
      fn napi_create_array(env: napi_env, result: *mut napi_value) -> napi_status;
      fn napi_create_array_with_length(
        env: napi_env,
        length: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_double(env: napi_env, value: f64, result: *mut napi_value) -> napi_status;
      fn napi_create_int32(env: napi_env, value: i32, result: *mut napi_value) -> napi_status;
      fn napi_create_uint32(env: napi_env, value: u32, result: *mut napi_value) -> napi_status;
      fn napi_create_int64(env: napi_env, value: i64, result: *mut napi_value) -> napi_status;
      fn napi_create_string_latin1(
        env: napi_env,
        str_: *const c_char,
        length: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_string_utf8(
        env: napi_env,
        str_: *const c_char,
        length: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_string_utf16(
        env: napi_env,
        str_: *const u16,
        length: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_symbol(
        env: napi_env,
        description: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_function(
        env: napi_env,
        utf8name: *const c_char,
        length: usize,
        cb: napi_callback,
        data: *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_error(
        env: napi_env,
        code: napi_value,
        msg: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_type_error(
        env: napi_env,
        code: napi_value,
        msg: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_range_error(
        env: napi_env,
        code: napi_value,
        msg: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_typeof(env: napi_env, value: napi_value, result: *mut napi_valuetype) -> napi_status;
      fn napi_get_value_double(env: napi_env, value: napi_value, result: *mut f64) -> napi_status;
      fn napi_get_value_int32(env: napi_env, value: napi_value, result: *mut i32) -> napi_status;
      fn napi_get_value_uint32(env: napi_env, value: napi_value, result: *mut u32) -> napi_status;
      fn napi_get_value_int64(env: napi_env, value: napi_value, result: *mut i64) -> napi_status;
      fn napi_get_value_bool(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_get_value_string_latin1(
        env: napi_env,
        value: napi_value,
        buf: *mut c_char,
        bufsize: usize,
        result: *mut usize,
      ) -> napi_status;
      fn napi_get_value_string_utf8(
        env: napi_env,
        value: napi_value,
        buf: *mut c_char,
        bufsize: usize,
        result: *mut usize,
      ) -> napi_status;
      fn napi_get_value_string_utf16(
        env: napi_env,
        value: napi_value,
        buf: *mut u16,
        bufsize: usize,
        result: *mut usize,
      ) -> napi_status;
      fn napi_coerce_to_bool(
        env: napi_env,
        value: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_coerce_to_number(
        env: napi_env,
        value: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_coerce_to_object(
        env: napi_env,
        value: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_coerce_to_string(
        env: napi_env,
        value: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_get_prototype(
        env: napi_env,
        object: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_get_property_names(
        env: napi_env,
        object: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_set_property(
        env: napi_env,
        object: napi_value,
        key: napi_value,
        value: napi_value,
      ) -> napi_status;
      fn napi_has_property(
        env: napi_env,
        object: napi_value,
        key: napi_value,
        result: *mut bool,
      ) -> napi_status;
      fn napi_get_property(
        env: napi_env,
        object: napi_value,
        key: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_delete_property(
        env: napi_env,
        object: napi_value,
        key: napi_value,
        result: *mut bool,
      ) -> napi_status;
      fn napi_has_own_property(
        env: napi_env,
        object: napi_value,
        key: napi_value,
        result: *mut bool,
      ) -> napi_status;
      fn napi_set_named_property(
        env: napi_env,
        object: napi_value,
        utf8name: *const c_char,
        value: napi_value,
      ) -> napi_status;
      fn napi_has_named_property(
        env: napi_env,
        object: napi_value,
        utf8name: *const c_char,
        result: *mut bool,
      ) -> napi_status;
      fn napi_get_named_property(
        env: napi_env,
        object: napi_value,
        utf8name: *const c_char,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_set_element(
        env: napi_env,
        object: napi_value,
        index: u32,
        value: napi_value,
      ) -> napi_status;
      fn napi_has_element(
        env: napi_env,
        object: napi_value,
        index: u32,
        result: *mut bool,
      ) -> napi_status;
      fn napi_get_element(
        env: napi_env,
        object: napi_value,
        index: u32,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_delete_element(
        env: napi_env,
        object: napi_value,
        index: u32,
        result: *mut bool,
      ) -> napi_status;
      fn napi_define_properties(
        env: napi_env,
        object: napi_value,
        property_count: usize,
        properties: *const napi_property_descriptor,
      ) -> napi_status;
      fn napi_is_array(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_get_array_length(env: napi_env, value: napi_value, result: *mut u32) -> napi_status;
      fn napi_strict_equals(
        env: napi_env,
        lhs: napi_value,
        rhs: napi_value,
        result: *mut bool,
      ) -> napi_status;
      fn napi_call_function(
        env: napi_env,
        recv: napi_value,
        func: napi_value,
        argc: usize,
        argv: *const napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_new_instance(
        env: napi_env,
        constructor: napi_value,
        argc: usize,
        argv: *const napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_instanceof(
        env: napi_env,
        object: napi_value,
        constructor: napi_value,
        result: *mut bool,
      ) -> napi_status;
      fn napi_get_cb_info(
        env: napi_env,
        cbinfo: napi_callback_info,
        argc: *mut usize,
        argv: *mut napi_value,
        this_arg: *mut napi_value,
        data: *mut *mut c_void,
      ) -> napi_status;
      fn napi_get_new_target(
        env: napi_env,
        cbinfo: napi_callback_info,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_define_class(
        env: napi_env,
        utf8name: *const c_char,
        length: usize,
        constructor: napi_callback,
        data: *mut c_void,
        property_count: usize,
        properties: *const napi_property_descriptor,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_wrap(
        env: napi_env,
        js_object: napi_value,
        native_object: *mut c_void,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_ref,
      ) -> napi_status;
      fn napi_unwrap(env: napi_env, js_object: napi_value, result: *mut *mut c_void)
        -> napi_status;
      fn napi_remove_wrap(
        env: napi_env,
        js_object: napi_value,
        result: *mut *mut c_void,
      ) -> napi_status;
      fn napi_create_external(
        env: napi_env,
        data: *mut c_void,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_get_value_external(
        env: napi_env,
        value: napi_value,
        result: *mut *mut c_void,
      ) -> napi_status;
      fn napi_create_reference(
        env: napi_env,
        value: napi_value,
        initial_refcount: u32,
        result: *mut napi_ref,
      ) -> napi_status;
      fn napi_delete_reference(env: napi_env, ref_: napi_ref) -> napi_status;
      fn napi_reference_ref(env: napi_env, ref_: napi_ref, result: *mut u32) -> napi_status;
      fn napi_reference_unref(env: napi_env, ref_: napi_ref, result: *mut u32) -> napi_status;
      fn napi_get_reference_value(
        env: napi_env,
        ref_: napi_ref,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_open_handle_scope(env: napi_env, result: *mut napi_handle_scope) -> napi_status;
      fn napi_close_handle_scope(env: napi_env, scope: napi_handle_scope) -> napi_status;
      fn napi_open_escapable_handle_scope(
        env: napi_env,
        result: *mut napi_escapable_handle_scope,
      ) -> napi_status;
      fn napi_close_escapable_handle_scope(
        env: napi_env,
        scope: napi_escapable_handle_scope,
      ) -> napi_status;
      fn napi_escape_handle(
        env: napi_env,
        scope: napi_escapable_handle_scope,
        escapee: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_throw(env: napi_env, error: napi_value) -> napi_status;
      fn napi_throw_error(env: napi_env, code: *const c_char, msg: *const c_char) -> napi_status;
      fn napi_throw_type_error(
        env: napi_env,
        code: *const c_char,
        msg: *const c_char,
      ) -> napi_status;
      fn napi_throw_range_error(
        env: napi_env,
        code: *const c_char,
        msg: *const c_char,
      ) -> napi_status;
      fn napi_is_error(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_is_exception_pending(env: napi_env, result: *mut bool) -> napi_status;
      fn napi_get_and_clear_last_exception(env: napi_env, result: *mut napi_value) -> napi_status;
      fn napi_is_arraybuffer(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_create_arraybuffer(
        env: napi_env,
        byte_length: usize,
        data: *mut *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_external_arraybuffer(
        env: napi_env,
        external_data: *mut c_void,
        byte_length: usize,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_get_arraybuffer_info(
        env: napi_env,
        arraybuffer: napi_value,
        data: *mut *mut c_void,
        byte_length: *mut usize,
      ) -> napi_status;
      fn napi_is_typedarray(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_create_typedarray(
        env: napi_env,
        type_: napi_typedarray_type,
        length: usize,
        arraybuffer: napi_value,
        byte_offset: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_get_typedarray_info(
        env: napi_env,
        typedarray: napi_value,
        type_: *mut napi_typedarray_type,
        length: *mut usize,
        data: *mut *mut c_void,
        arraybuffer: *mut napi_value,
        byte_offset: *mut usize,
      ) -> napi_status;
      fn napi_create_dataview(
        env: napi_env,
        length: usize,
        arraybuffer: napi_value,
        byte_offset: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_is_dataview(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_get_dataview_info(
        env: napi_env,
        dataview: napi_value,
        bytelength: *mut usize,
        data: *mut *mut c_void,
        arraybuffer: *mut napi_value,
        byte_offset: *mut usize,
      ) -> napi_status;
      fn napi_get_version(env: napi_env, result: *mut u32) -> napi_status;
      fn napi_create_promise(
        env: napi_env,
        deferred: *mut napi_deferred,
        promise: *mut napi_value,
      ) -> napi_status;
      fn napi_resolve_deferred(
        env: napi_env,
        deferred: napi_deferred,
        resolution: napi_value,
      ) -> napi_status;
      fn napi_reject_deferred(
        env: napi_env,
        deferred: napi_deferred,
        rejection: napi_value,
      ) -> napi_status;
      fn napi_is_promise(env: napi_env, value: napi_value, is_promise: *mut bool) -> napi_status;
      fn napi_run_script(env: napi_env, script: napi_value, result: *mut napi_value)
        -> napi_status;
      fn napi_adjust_external_memory(
        env: napi_env,
        change_in_bytes: i64,
        adjusted_value: *mut i64,
      ) -> napi_status;
      fn napi_module_register(mod_: *mut napi_module);
      fn napi_fatal_error(
        location: *const c_char,
        location_len: usize,
        message: *const c_char,
        message_len: usize,
      );
      fn napi_async_init(
        env: napi_env,
        async_resource: napi_value,
        async_resource_name: napi_value,
        result: *mut napi_async_context,
      ) -> napi_status;
      fn napi_async_destroy(env: napi_env, async_context: napi_async_context) -> napi_status;
      fn napi_make_callback(
        env: napi_env,
        async_context: napi_async_context,
        recv: napi_value,
        func: napi_value,
        argc: usize,
        argv: *const napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_buffer(
        env: napi_env,
        length: usize,
        data: *mut *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_external_buffer(
        env: napi_env,
        length: usize,
        data: *mut c_void,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_buffer_copy(
        env: napi_env,
        length: usize,
        data: *const c_void,
        result_data: *mut *mut c_void,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_is_buffer(env: napi_env, value: napi_value, result: *mut bool) -> napi_status;
      fn napi_get_buffer_info(
        env: napi_env,
        value: napi_value,
        data: *mut *mut c_void,
        length: *mut usize,
      ) -> napi_status;
      fn napi_create_async_work(
        env: napi_env,
        async_resource: napi_value,
        async_resource_name: napi_value,
        execute: napi_async_execute_callback,
        complete: napi_async_complete_callback,
        data: *mut c_void,
        result: *mut napi_async_work,
      ) -> napi_status;
      fn napi_delete_async_work(env: napi_env, work: napi_async_work) -> napi_status;
      fn napi_queue_async_work(env: napi_env, work: napi_async_work) -> napi_status;
      fn napi_cancel_async_work(env: napi_env, work: napi_async_work) -> napi_status;
      fn napi_get_node_version(
        env: napi_env,
        version: *mut *const napi_node_version,
      ) -> napi_status;
    }
  );
}

#[cfg(feature = "napi2")]
mod napi2 {
  use super::super::types::*;
  use std::os::raw::c_int;

  generate!(
    extern "C" {
      fn napi_get_uv_event_loop(env: napi_env, loop_: *mut *mut uv_loop_s) -> napi_status;

      fn uv_run(loop_: *mut uv_loop_s, mode: uv_run_mode) -> c_int;
    }
  );
}

#[cfg(feature = "napi3")]
mod napi3 {
  use std::os::raw::c_void;

  use super::super::types::*;

  generate!(
    extern "C" {
      fn napi_fatal_exception(env: napi_env, err: napi_value) -> napi_status;
      fn napi_add_env_cleanup_hook(
        env: napi_env,
        fun: Option<unsafe extern "C" fn(arg: *mut c_void)>,
        arg: *mut c_void,
      ) -> napi_status;
      fn napi_remove_env_cleanup_hook(
        env: napi_env,
        fun: Option<unsafe extern "C" fn(arg: *mut c_void)>,
        arg: *mut c_void,
      ) -> napi_status;
      fn napi_open_callback_scope(
        env: napi_env,
        resource_object: napi_value,
        context: napi_async_context,
        result: *mut napi_callback_scope,
      ) -> napi_status;
      fn napi_close_callback_scope(env: napi_env, scope: napi_callback_scope) -> napi_status;
    }
  );
}

#[cfg(feature = "napi4")]
mod napi4 {
  use super::super::types::*;
  use std::os::raw::c_void;

  generate!(
    extern "C" {
      fn napi_create_threadsafe_function(
        env: napi_env,
        func: napi_value,
        async_resource: napi_value,
        async_resource_name: napi_value,
        max_queue_size: usize,
        initial_thread_count: usize,
        thread_finalize_data: *mut c_void,
        thread_finalize_cb: napi_finalize,
        context: *mut c_void,
        call_js_cb: napi_threadsafe_function_call_js,
        result: *mut napi_threadsafe_function,
      ) -> napi_status;
      fn napi_get_threadsafe_function_context(
        func: napi_threadsafe_function,
        result: *mut *mut c_void,
      ) -> napi_status;
      fn napi_call_threadsafe_function(
        func: napi_threadsafe_function,
        data: *mut c_void,
        is_blocking: napi_threadsafe_function_call_mode,
      ) -> napi_status;
      fn napi_acquire_threadsafe_function(func: napi_threadsafe_function) -> napi_status;
      fn napi_release_threadsafe_function(
        func: napi_threadsafe_function,
        mode: napi_threadsafe_function_release_mode,
      ) -> napi_status;
      fn napi_unref_threadsafe_function(
        env: napi_env,
        func: napi_threadsafe_function,
      ) -> napi_status;
      fn napi_ref_threadsafe_function(env: napi_env, func: napi_threadsafe_function)
        -> napi_status;
    }
  );
}

#[cfg(feature = "napi5")]
mod napi5 {
  use super::super::types::*;
  use std::ffi::c_void;

  generate!(
    extern "C" {
      fn napi_create_date(env: napi_env, time: f64, result: *mut napi_value) -> napi_status;
      fn napi_is_date(env: napi_env, value: napi_value, is_date: *mut bool) -> napi_status;
      fn napi_get_date_value(env: napi_env, value: napi_value, result: *mut f64) -> napi_status;
      fn napi_add_finalizer(
        env: napi_env,
        js_object: napi_value,
        native_object: *mut c_void,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_ref,
      ) -> napi_status;
    }
  );
}

#[cfg(feature = "napi6")]
mod napi6 {
  use super::super::types::*;
  use std::os::raw::{c_int, c_void};

  generate!(
    extern "C" {
      fn napi_create_bigint_int64(
        env: napi_env,
        value: i64,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_bigint_uint64(
        env: napi_env,
        value: u64,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_create_bigint_words(
        env: napi_env,
        sign_bit: c_int,
        word_count: usize,
        words: *const u64,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_get_value_bigint_int64(
        env: napi_env,
        value: napi_value,
        result: *mut i64,
        lossless: *mut bool,
      ) -> napi_status;
      fn napi_get_value_bigint_uint64(
        env: napi_env,
        value: napi_value,
        result: *mut u64,
        lossless: *mut bool,
      ) -> napi_status;
      fn napi_get_value_bigint_words(
        env: napi_env,
        value: napi_value,
        sign_bit: *mut c_int,
        word_count: *mut usize,
        words: *mut u64,
      ) -> napi_status;
      fn napi_get_all_property_names(
        env: napi_env,
        object: napi_value,
        key_mode: napi_key_collection_mode,
        key_filter: napi_key_filter,
        key_conversion: napi_key_conversion,
        result: *mut napi_value,
      ) -> napi_status;
      fn napi_set_instance_data(
        env: napi_env,
        data: *mut c_void,
        finalize_cb: napi_finalize,
        finalize_hint: *mut c_void,
      ) -> napi_status;
      fn napi_get_instance_data(env: napi_env, data: *mut *mut c_void) -> napi_status;
    }
  );
}

#[cfg(feature = "napi7")]
mod napi7 {
  use super::super::types::*;

  generate!(
    extern "C" {
      fn napi_detach_arraybuffer(env: napi_env, arraybuffer: napi_value) -> napi_status;
      fn napi_is_detached_arraybuffer(
        env: napi_env,
        value: napi_value,
        result: *mut bool,
      ) -> napi_status;
    }
  );
}

#[cfg(feature = "napi8")]
mod napi8 {
  use std::os::raw::c_void;

  use super::super::types::*;

  generate!(
    extern "C" {
      fn napi_add_async_cleanup_hook(
        env: napi_env,
        hook: napi_async_cleanup_hook,
        arg: *mut c_void,
        remove_handle: *mut napi_async_cleanup_hook_handle,
      ) -> napi_status;

      fn napi_remove_async_cleanup_hook(
        remove_handle: napi_async_cleanup_hook_handle,
      ) -> napi_status;

      fn napi_object_freeze(env: napi_env, object: napi_value) -> napi_status;

      fn napi_object_seal(env: napi_env, object: napi_value) -> napi_status;
    }
  );
}

#[cfg(feature = "napi9")]
mod napi9 {
  use std::os::raw::c_char;

  use super::super::types::*;

  generate!(
    extern "C" {
      fn node_api_symbol_for(
        env: napi_env,
        utf8name: *const c_char,
        length: usize,
        result: *mut napi_value,
      ) -> napi_status;
      fn node_api_get_module_file_name(env: napi_env, result: *mut *const c_char) -> napi_status;
      fn node_api_create_syntax_error(
        env: napi_env,
        code: napi_value,
        msg: napi_value,
        result: *mut napi_value,
      ) -> napi_status;
      fn node_api_throw_syntax_error(
        env: napi_env,
        code: *const c_char,
        msg: *const c_char,
      ) -> napi_status;
    }
  );
}

#[cfg(feature = "experimental")]
mod experimental {
  use std::os::raw::{c_char, c_void};

  use super::super::types::*;

  generate!(
    extern "C" {
      fn node_api_create_external_string_latin1(
        env: napi_env,
        str_: *const c_char,
        length: usize,
        napi_finalize: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_value,
        copied: *mut bool,
      ) -> napi_status;

      fn node_api_create_external_string_utf16(
        env: napi_env,
        str_: *const u16,
        length: usize,
        napi_finalize: napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi_value,
        copied: *mut bool,
      ) -> napi_status;
    }
  );
}

#[cfg(feature = "experimental")]
pub use experimental::*;

pub use napi1::*;
#[cfg(feature = "napi2")]
pub use napi2::*;
#[cfg(feature = "napi3")]
pub use napi3::*;
#[cfg(feature = "napi4")]
pub use napi4::*;
#[cfg(feature = "napi5")]
pub use napi5::*;
#[cfg(feature = "napi6")]
pub use napi6::*;
#[cfg(feature = "napi7")]
pub use napi7::*;
#[cfg(feature = "napi8")]
pub use napi8::*;
#[cfg(feature = "napi9")]
pub use napi9::*;

#[cfg(any(windows, feature = "dyn-symbols"))]
pub(super) unsafe fn load_all() -> Result<libloading::Library, libloading::Error> {
  // An embedder with no Node-API in its executable names the modules that provide it, separated
  // by ';' and in priority order: each symbol comes from the first one that exports it. React
  // Native for Windows has the engine half (js_native_api.h) in hermes.dll and needs the Node half
  // (node_api.h: threadsafe functions, cleanup hooks, buffers) from the module that embeds the
  // addon, so it sets NAPI_HOST_MODULES=<that module>;<hermes.dll>.
  #[cfg(windows)]
  if let Some(list) = std::env::var_os("NAPI_HOST_MODULES") {
    let mut hosts = Vec::new();
    for path in list.to_string_lossy().split(';').map(str::trim).filter(|p| !p.is_empty()) {
      hosts.push(libloading::Library::from(libloading::os::windows::Library::new(path)?));
    }
    if !hosts.is_empty() {
      // A symbol a library lacks keeps the value an earlier load gave it, so loading in reverse
      // leaves each symbol from the first library that has it.
      for host in hosts.iter().rev() {
        load_groups(host)?;
      }
      let mut hosts = hosts.into_iter();
      let first = hosts.next().unwrap();
      // The other libraries must stay loaded for as long as their symbols are in use: for good.
      hosts.for_each(std::mem::forget);
      return Ok(first);
    }
  }

  #[cfg(windows)]
  let host = {
    use std::os::windows::ffi::OsStringExt;
    windows_link::link!("kernel32.dll" "system" fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut isize) -> i32);
    windows_link::link!("kernel32.dll" "system" fn GetModuleFileNameW(module: isize, filename: *mut u16, size: u32) -> u32);
    windows_link::link!("kernel32.dll" "system" fn GetProcAddress(module: isize, proc_name: *const u8) -> *const core::ffi::c_void);
    extern "C" fn anchor() {}
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;
    let mut handle: isize = 0;
    let ok = GetModuleHandleExW(
      GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
      anchor as usize as *const u16,
      &mut handle,
    );
    let provides_napi =
      ok != 0 && handle != 0 && !GetProcAddress(handle, c"napi_create_string_utf8".as_ptr().cast()).is_null();
    // If THIS module exports napi_* (standalone .exe or the nativescript.dll cdylib), load it by
    // path via the normal refcounted LoadLibrary so symbols resolve from here. Otherwise (e.g. a
    // Node addon that consumes the host's napi) fall back to the process module.
    let by_self: Option<libloading::os::windows::Library> = if provides_napi {
      let mut buf = [0u16; 32768];
      let n = GetModuleFileNameW(handle, buf.as_mut_ptr(), buf.len() as u32) as usize;
      if n > 0 && n < buf.len() {
        let path = std::ffi::OsString::from_wide(&buf[..n]);
        libloading::os::windows::Library::new(path).ok()
      } else {
        None
      }
    } else {
      None
    };
    match by_self {
      Some(lib) => lib.into(),
      None => libloading::os::windows::Library::this()?.into(),
    }
  };

  #[cfg(unix)]
  let host = libloading::os::unix::Library::this().into();

  load_groups(&host)?;
  Ok(host)
}

#[cfg(any(windows, feature = "dyn-symbols"))]
unsafe fn load_groups(host: &libloading::Library) -> Result<(), libloading::Error> {
  napi1::load(host)?;
  #[cfg(feature = "napi2")]
  napi2::load(host)?;
  #[cfg(feature = "napi3")]
  napi3::load(host)?;
  #[cfg(feature = "napi4")]
  napi4::load(host)?;
  #[cfg(feature = "napi5")]
  napi5::load(host)?;
  #[cfg(feature = "napi6")]
  napi6::load(host)?;
  #[cfg(feature = "napi7")]
  napi7::load(host)?;
  #[cfg(feature = "napi8")]
  napi8::load(host)?;
  #[cfg(feature = "napi9")]
  napi9::load(host)?;
  #[cfg(feature = "experimental")]
  experimental::load(host)?;
  Ok(())
}
