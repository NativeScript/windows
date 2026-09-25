use crate::create_struct_object_from_raw;
use crate::error::AnyError;
use crate::helpers::ffi_native_type_from_signature;
use crate::ns_proxy;
use crate::value::{
    ffi_parse_bool_arg, ffi_parse_buffer_arg_with_length, ffi_parse_f32_arg, ffi_parse_f64_arg,
    ffi_parse_function_arg, ffi_parse_i16_arg, ffi_parse_i32_arg, ffi_parse_i64_arg,
    ffi_parse_i8_arg, ffi_parse_isize_arg, ffi_parse_pointer_arg, ffi_parse_query_interface_arg,
    ffi_parse_string_arg, ffi_parse_struct_arg, ffi_parse_u16_arg, ffi_parse_u32_arg,
    ffi_parse_u64_arg, ffi_parse_u8_arg, ffi_parse_usize_arg, read_value_from_ptr,
    set_out_param_value, try_unwrap_out_param, write_v8_value_to_ptr, NativeType, NativeValue,
};
use libffi::middle::*;
use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::DeclarationKind;
use metadata::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::declarations::parameter_declaration::ParameterDeclaration;
use metadata::declaring_interface_for_method::Metadata;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use windows::core::{IUnknown, Interface, GUID, HRESULT};

pub struct GenericMethodCall {
    index: usize,
    number_of_parameters: usize,
    number_of_abi_parameters: usize,
    is_initializer: bool,
    is_sealed: bool,
    is_void: bool,
    iid: GUID,
    interface: IUnknown,
    cif: Cif,
    func: *mut c_void,
    parameter_types: Vec<NativeType>,
    parse_parameter_types: Vec<NativeType>,
    parameters: Vec<ParameterDeclaration>,
    return_type: String,
    /// Pre-computed interface IIDs for pointer-typed parameters (one per parse param, None if no QI needed).
    parameter_arg_iids: Vec<Option<GUID>>,
    /// Scratch buffer for value-type returns (same role as in MethodCall).
    return_value_buf: [u8; 128],
}

#[inline]
fn call_failure() -> HRESULT {
    // E_FAIL
    HRESULT(0x8000_4005u32 as i32)
}

impl GenericMethodCall {
    pub fn is_void(&self) -> bool {
        self.is_void
    }

    pub fn return_type(&self) -> &str {
        self.return_type.as_str()
    }

    pub fn new(
        class: &GenericInterfaceDeclaration,
        method: &MethodDeclaration,
        is_sealed: bool,
        interface: IUnknown,
        is_initializer: bool,
        return_type: String,
        type_args: Vec<String>,
    ) -> Self {
        let number_of_parameters = method.number_of_parameters();

        let mut index = Metadata::find_method_index(
            method.metadata().unwrap(),
            class.base().token(),
            method.token(),
        );

        let iid = class.id();

        index = index.saturating_add(6); // account for IInspectable vtable overhead

        let mut interface_ptr: *mut c_void = std::ptr::null_mut();

        let result = unsafe {
            ((*interface.vtable()).QueryInterface)(
                interface.as_raw(),
                &iid,
                &mut interface_ptr as *mut _ as *mut *mut c_void,
            )
        };

        // Generic interface QI is best-effort: fall back to the original interface
        // when the object doesn't implement the expected instantiation.
        let qi_ok = result.is_ok() && !interface_ptr.is_null();

        let is_composition = !is_sealed;

        let is_void = method.is_void();

        let other_params: usize = if is_initializer {
            if is_sealed {
                2
            } else {
                4
            }
        } else {
            if is_void {
                1
            } else {
                2
            }
        };

        let mut parameter_types: Vec<NativeType> =
            Vec::with_capacity(number_of_parameters + other_params + 4);
        let mut parse_parameter_types: Vec<NativeType> = Vec::with_capacity(number_of_parameters);
        let mut parameter_arg_iids: Vec<Option<GUID>> = Vec::with_capacity(number_of_parameters);
        parameter_types.push(NativeType::Pointer);

        for parameter in method.parameters().iter() {
            let type_ = parameter.type_();
            let metadata = parameter.metadata().unwrap();

            let signature = Signature::to_string(metadata, &type_);

            let parse_native_type = NativeType::try_from(signature.as_str());
            assert!(parse_native_type.is_ok());
            let parse_native_type = parse_native_type.unwrap();

            // For pointer params, resolve the concrete type (substituting Var!N) to its interface IID.
            let arg_iid = if matches!(parse_native_type, NativeType::Pointer) {
                let concrete_type = if signature.starts_with("Var!") {
                    signature["Var!".len()..]
                        .parse::<usize>()
                        .ok()
                        .and_then(|idx| type_args.get(idx))
                        .map(|s| s.as_str().to_owned())
                } else if signature.contains('.') {
                    Some(signature.clone())
                } else {
                    None
                };
                concrete_type.and_then(|type_name| {
                    let lookup = crate::helpers::strip_generic_suffix(&type_name);
                    MetadataReader::find_by_name(lookup).and_then(|decl| {
                        let lock = decl.read();
                        match lock.kind() {
                            DeclarationKind::Interface => lock
                                .as_any()
                                .downcast_ref::<InterfaceDeclaration>()
                                .map(|i| i.id()),
                            DeclarationKind::GenericInterface => lock
                                .as_any()
                                .downcast_ref::<GenericInterfaceDeclaration>()
                                .map(|i| i.id()),
                            DeclarationKind::GenericInterfaceInstance => lock
                                .as_any()
                                .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                                .map(|i| i.id()),
                            DeclarationKind::Class => lock
                                .as_any()
                                .downcast_ref::<ClassDeclaration>()
                                .and_then(|c| c.default_interface())
                                .map(|i| i.id()),
                            _ => None,
                        }
                    })
                })
            } else {
                None
            };
            parameter_arg_iids.push(arg_iid);
            parse_parameter_types.push(parse_native_type);
            if parameter.is_out() || signature.trim().starts_with("ByRef ") {
                parameter_types.push(NativeType::Pointer);
            } else {
                let abi_native = crate::helpers::struct_native_type_for_sig(signature.as_str())
                    .unwrap_or_else(|| ffi_native_type_from_signature(signature.as_str()));
                if matches!(abi_native, NativeType::Buffer) {
                    parameter_types.push(NativeType::U32);
                    parameter_types.push(NativeType::Buffer);
                } else {
                    parameter_types.push(abi_native);
                }
            }
        }

        if is_initializer {
            if is_composition {
                parameter_types.push(NativeType::Pointer);
                parameter_types.push(NativeType::Pointer);
            }
            parameter_types.push(NativeType::Pointer);
        } else {
            if !is_void {
                parameter_types.push(NativeType::Pointer);
            }
        }

        let number_of_abi_parameters = parameter_types.len();

        let params = parameter_types
            .iter()
            .cloned()
            .map(libffi::middle::Type::try_from)
            .collect::<std::result::Result<Vec<Type>, AnyError>>();

        assert!(params.is_ok());

        let cif = Cif::new(params.unwrap(), Type::i32());

        let effective_interface = if qi_ok {
            unsafe { IUnknown::from_raw(interface_ptr) }
        } else {
            interface.clone()
        };
        let vtable: *mut *mut c_void = unsafe { std::mem::transmute(effective_interface.vtable()) };
        let func = unsafe { *vtable.offset(index as isize) };

        Self {
            cif,
            func,
            index,
            number_of_parameters,
            number_of_abi_parameters,
            is_initializer,
            is_sealed,
            is_void: method.is_void(),
            iid,
            interface: effective_interface,
            parameter_types,
            parse_parameter_types,
            parameters: method.parameters().to_vec(),
            return_type,
            parameter_arg_iids,
            return_value_buf: [0u8; 128],
        }
    }

    pub fn call<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        args: &v8::FunctionCallbackArguments,
    ) -> (HRESULT, *mut c_void, Vec<v8::Local<'s, v8::Value>>) {
        let number_of_abi_parameters = self.number_of_abi_parameters;
        let mut arguments: Vec<NativeValue> = Vec::with_capacity(number_of_abi_parameters);
        // Track parse-level types for each ABI argument slot.
        let mut argument_parse_types: Vec<Option<NativeType>> =
            Vec::with_capacity(number_of_abi_parameters);
        // Keep QI'd interfaces alive for the duration of the FFI call.
        let mut queried_interfaces: Vec<IUnknown> = Vec::new();
        // Stable per-call buffers for out (ByRef) parameters.
        let mut struct_scratch: Vec<Vec<u8>> = Vec::new();
        let mut out_slots: Vec<(
            usize,
            NativeType,
            Option<String>,
            Option<v8::Local<'s, v8::Object>>,
        )> = Vec::new();

        arguments.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        argument_parse_types.push(None);

        for (i, native_type) in self.parse_parameter_types.iter().enumerate() {
            let parameter = &self.parameters[i];
            let param_sig_opt = parameter
                .metadata()
                .map(|m| Signature::to_string(m, &parameter.type_()));
            let is_sig_byref = param_sig_opt
                .as_ref()
                .map_or(false, |s| s.starts_with("ByRef "));

            // Handle out (ByRef) parameters by allocating stable storage.
            // Also treat a missing caller argument for a `ByRef` signature as an implicit out-slot.
            if parameter.is_out() || is_sig_byref {
                let slot_index = arguments.len();
                let slot_size = match native_type {
                    NativeType::Struct(_) => native_type.size(),
                    NativeType::Pointer
                    | NativeType::Buffer
                    | NativeType::Function
                    | NativeType::String => std::mem::size_of::<usize>(),
                    _ => native_type.size(),
                };
                let mut buf: Vec<u8> = vec![0u8; slot_size];
                let ptr = buf.as_mut_ptr() as *mut c_void;
                struct_scratch.push(buf);
                arguments.push(NativeValue { pointer: ptr });
                argument_parse_types.push(None);

                let raw_init_val = args.get(i as i32);
                let out_wrapper = try_unwrap_out_param(scope, raw_init_val);
                let (wrapper_obj, init_val) = match out_wrapper {
                    Some((obj, value)) => (Some(obj), value),
                    None => (None, raw_init_val),
                };

                // Initialize from caller-provided argument if present (in/out semantics).
                if (args.length() as usize) > i {
                    if !init_val.is_undefined() && !init_val.is_null() {
                        match write_v8_value_to_ptr(scope, init_val, ptr, native_type) {
                            Ok(_) => {}
                            Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
                        }
                    }
                }
                let sig = param_sig_opt;
                out_slots.push((slot_index, native_type.clone(), sig, wrapper_obj));
                continue;
            }

            let value = args.get(i as i32);

            let value = match *native_type {
                NativeType::Void => return (call_failure(), std::ptr::null_mut(), Vec::new()),
                NativeType::Bool => ffi_parse_bool_arg(value),
                NativeType::U8 => ffi_parse_u8_arg(value),
                NativeType::I8 => ffi_parse_i8_arg(value),
                NativeType::U16 => ffi_parse_u16_arg(value),
                NativeType::I16 => ffi_parse_i16_arg(value),
                NativeType::U32 => ffi_parse_u32_arg(value),
                NativeType::I32 => ffi_parse_i32_arg(value),
                NativeType::U64 => ffi_parse_u64_arg(scope, value),
                NativeType::I64 => ffi_parse_i64_arg(scope, value),
                NativeType::USize => ffi_parse_usize_arg(scope, value),
                NativeType::ISize => ffi_parse_isize_arg(scope, value),
                NativeType::F32 => ffi_parse_f32_arg(value),
                NativeType::F64 => ffi_parse_f64_arg(value),
                NativeType::Pointer => {
                    if let Some(Some(iid)) = self.parameter_arg_iids.get(i) {
                        match ffi_parse_query_interface_arg(scope, value, iid) {
                            Ok((pointer, Some(guard))) => {
                                queried_interfaces.push(guard);
                                Ok(pointer)
                            }
                            Ok((pointer, None)) => Ok(pointer),
                            Err(_) => ffi_parse_pointer_arg(scope, value),
                        }
                    } else {
                        ffi_parse_pointer_arg(scope, value)
                    }
                }
                NativeType::Buffer => {
                    let parsed = ffi_parse_buffer_arg_with_length(scope, value);
                    let (buffer_value, byte_length) = match parsed {
                        Ok(value) => value,
                        Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
                    };

                    arguments.push(NativeValue {
                        u32_value: byte_length,
                    });
                    argument_parse_types.push(Some(native_type.clone()));
                    arguments.push(buffer_value);
                    argument_parse_types.push(Some(native_type.clone()));
                    continue;
                }
                NativeType::Function => ffi_parse_function_arg(scope, value),
                NativeType::Struct(_) => ffi_parse_struct_arg(scope, value),
                NativeType::String => ffi_parse_string_arg(scope, value),
            };

            let value = match value {
                Ok(value) => value,
                Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
            };

            arguments.push(value);
            argument_parse_types.push(Some(native_type.clone()));
        }

        let mut result: *mut c_void = std::ptr::null_mut();

        let is_value_type = if self.return_type == "Guid" {
            true
        } else if self.return_type.contains('.') {
            let lookup = crate::helpers::strip_generic_suffix(self.return_type.as_str());
            MetadataReader::find_by_name(lookup).map_or(false, |dec| {
                matches!(dec.read().kind(), DeclarationKind::Struct)
            })
        } else {
            false
        };

        let is_scalar_return = matches!(
            self.return_type.as_str(),
            "UInt8"
                | "Int8"
                | "UInt16"
                | "Int16"
                | "UInt32"
                | "Int32"
                | "UInt64"
                | "Int64"
                | "USize"
                | "ISize"
                | "Single"
                | "Double"
                | "Boolean"
                | "Char16"
        );

        let is_string_return = self.return_type.as_str() == "String";

        if self.is_initializer {
            unsafe {
                arguments.push(NativeValue {
                    pointer: &mut result as *mut _ as *mut c_void,
                })
            };
            argument_parse_types.push(None);
        } else {
            if !self.is_void {
                if is_value_type || is_scalar_return || is_string_return {
                    let buf_ptr = self.return_value_buf.as_mut_ptr() as *mut c_void;
                    arguments.push(NativeValue { pointer: buf_ptr });
                    argument_parse_types.push(None);
                } else {
                    arguments.push(NativeValue {
                        pointer: &mut result as *mut _ as *mut c_void,
                    });
                    argument_parse_types.push(None);
                }
            }
        }

        let prep = match crate::ffi::prepare_string_storage(
            &arguments,
            &self.parameter_types,
            &argument_parse_types,
        ) {
            Ok(value) => value,
            Err(_) => {
                unsafe { crate::ffi::release_string_args(&mut arguments, &argument_parse_types) };
                return (call_failure(), std::ptr::null_mut(), Vec::new());
            }
        };

        let call_args = crate::ffi::build_call_args(&prep, &arguments, &self.parameter_types);

        let ret_i32_res = catch_unwind(AssertUnwindSafe(|| unsafe {
            self.cif.call(CodePtr::from_ptr(self.func), &call_args)
        }));

        // The libffi borrow of the slots ends here; release the HSTRINGs built for String
        // parameters (the union would otherwise leak them). Out-slots are untouched.
        drop(call_args);
        drop(prep);
        unsafe { crate::ffi::release_string_args(&mut arguments, &argument_parse_types) };

        let ret = match ret_i32_res {
            Ok(code) => code,
            Err(_) => {
                let msg = format!("WinRT call panicked during invocation: returning E_FAIL");
                crate::store_last_js_error(msg);
                return (
                    HRESULT(0x8000_4005u32 as i32),
                    std::ptr::null_mut(),
                    Vec::new(),
                ); // E_FAIL
            }
        };

        let hr = HRESULT(ret);
        const RPC_E_WRONG_THREAD: u32 = 0x8001010E;
        if (hr.0 as u32) == RPC_E_WRONG_THREAD {
            let msg = crate::error::format_hresult_message(hr);
            crate::store_last_js_error(msg.clone());
            if let Some(vmstr) = v8::String::new(scope, &msg) {
                let err = v8::Exception::error(scope, vmstr);
                scope.throw_exception(err);
            }
        }

        if !self.is_initializer
            && !self.is_void
            && (is_value_type || is_scalar_return || is_string_return)
        {
            result = self.return_value_buf.as_mut_ptr() as *mut c_void;
        }

        // Marshal out-parameters back into V8 values using the recorded slots.
        let mut out_values: Vec<v8::Local<'s, v8::Value>> = Vec::new();
        for (slot_index, parse_native_type, sig_opt, wrapper_obj) in out_slots.into_iter() {
            let storage_ptr = unsafe {
                arguments
                    .get(slot_index)
                    .map(|v| v.pointer)
                    .unwrap_or(std::ptr::null_mut())
            };
            if storage_ptr.is_null() {
                let v: v8::Local<v8::Value> = v8::null(scope).into();
                if let Some(wrapper) = wrapper_obj {
                    let _ = set_out_param_value(scope, wrapper, v);
                } else {
                    out_values.push(v);
                }
                continue;
            }
            unsafe {
                let v = match parse_native_type {
                    NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                        let inner =
                            std::ptr::read_unaligned(storage_ptr as *const usize) as *mut c_void;
                        if inner.is_null() {
                            v8::null(scope).into()
                        } else if let Some(sig) = sig_opt.as_ref() {
                            if sig.contains('.') {
                                let mut lookup = sig.as_str();
                                if let Some(stripped) = lookup.strip_prefix("ByRef ") {
                                    lookup = stripped;
                                }
                                let lookup = crate::helpers::strip_generic_suffix(lookup);
                                if let Some(declaration) = MetadataReader::find_by_name(lookup) {
                                    if matches!(declaration.read().kind(), DeclarationKind::Struct)
                                    {
                                        create_struct_object_from_raw(declaration, inner, scope)
                                            .into()
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(inner) };
                                        ns_proxy::create_ns_ctor_instance_object(
                                            sig.as_str(),
                                            None,
                                            None,
                                            declaration,
                                            Some(instance),
                                            scope,
                                        )
                                        .into()
                                    }
                                } else {
                                    read_value_from_ptr(
                                        inner as *const c_void,
                                        scope,
                                        NativeType::Pointer,
                                    )
                                }
                            } else {
                                read_value_from_ptr(
                                    inner as *const c_void,
                                    scope,
                                    NativeType::Pointer,
                                )
                            }
                        } else {
                            read_value_from_ptr(inner as *const c_void, scope, NativeType::Pointer)
                        }
                    }
                    _ => read_value_from_ptr(
                        storage_ptr as *const c_void,
                        scope,
                        parse_native_type.clone(),
                    ),
                };
                if let Some(wrapper) = wrapper_obj {
                    let _ = set_out_param_value(scope, wrapper, v);
                } else {
                    out_values.push(v);
                }
            }
        }

        (HRESULT(ret), result, out_values)
    }
}
