use crate::declarations::base_class_declaration::BaseClassDeclarationImpl;
use crate::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use crate::declarations::interface_declaration::InterfaceDeclaration;
use crate::declarations::method_declaration::MethodDeclaration;
use crate::prelude::*;
use parking_lot::RwLock;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::addr_of_mut;
use std::sync::Arc;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::WinRT::Metadata::{
    mdtMemberRef, mdtMethodDef, mdtTypeDef, mdtTypeRef, mdtTypeSpec, CorTokenType,
    IMetaDataImport2, ELEMENT_TYPE_CLASS, ELEMENT_TYPE_GENERICINST, ELEMENT_TYPE_VOID,
};

pub struct Metadata {}

impl Metadata {
    fn resolve_methods_owner_token(
        metadata: &IMetaDataImport2,
        token: CorTokenType,
    ) -> Option<u32> {
        if token.0 == 0 {
            return None;
        }

        match CorTokenType(type_from_token(token)) {
            mdtTypeDef => Some(token.0 as u32),
            mdtTypeRef => {
                let type_name = get_type_name(metadata, token);
                let type_name = HSTRING::from(type_name);
                let type_name = PCWSTR(type_name.as_ptr());
                let mut resolved_token = 0_u32;
                let result =
                    unsafe { metadata.FindTypeDefByName(type_name, 0 as _, &mut resolved_token) };

                if result.is_ok() && resolved_token != 0 {
                    Some(resolved_token)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn get_method_containing_class_token(
        metadata: &IMetaDataImport2,
        method_token: CorTokenType,
    ) -> u32 {
        let mut class_token = 0_u32;

        match CorTokenType(type_from_token(method_token)) {
            mdtMethodDef => {
                let result = unsafe {
                    metadata.GetMethodProps(
                        method_token.0 as u32,
                        &mut class_token,
                        None,
                        0 as _,
                        0 as _,
                        0 as _,
                        0 as _,
                        0 as _,
                        0 as _,
                    )
                };
                debug_assert!(result.is_ok());
            }
            mdtMemberRef => {
                let result = unsafe {
                    metadata.GetMemberRefProps(
                        method_token.0 as u32,
                        &mut class_token,
                        None,
                        0 as _,
                        0 as _,
                        0 as _,
                    )
                };
                debug_assert!(result.is_ok());
            }
            _ => {
                std::unreachable!()
            }
        }

        class_token
    }

    pub fn get_custom_attribute_constructor_token(
        metadata: &IMetaDataImport2,
        custom_attribute: CorTokenType,
    ) -> u32 {
        let mut constructor_token = 0_u32;
        let result = unsafe {
            metadata.GetCustomAttributeProps(
                custom_attribute.0 as u32,
                0 as _,
                &mut constructor_token,
                0 as _,
                0 as _,
            )
        };
        debug_assert!(result.is_ok());
        constructor_token
    }

    pub fn get_custom_attribute_class_token(
        metadata: &IMetaDataImport2,
        custom_attribute: CorTokenType,
    ) -> u32 {
        let token = Metadata::get_custom_attribute_constructor_token(metadata, custom_attribute);
        Metadata::get_method_containing_class_token(metadata, CorTokenType(token as i32))
    }

    pub fn get_method_signature(
        metadata: &IMetaDataImport2,
        token: CorTokenType,
    ) -> PCCOR_SIGNATURE {
        let mut signature = std::ptr::null_mut();
        let mut signature_size = 0;

        match CorTokenType(type_from_token(token)) {
            mdtMethodDef => {
                let result = unsafe {
                    metadata.GetMethodProps(
                        token.0 as u32,
                        0 as _,
                        None,
                        0 as _,
                        0 as _,
                        addr_of_mut!(signature),
                        &mut signature_size,
                        0 as _,
                        0 as _,
                    )
                };
                debug_assert!(result.is_ok());
            }
            mdtMemberRef => {
                let result = unsafe {
                    metadata.GetMemberRefProps(
                        token.0 as u32,
                        0 as _,
                        None,
                        0 as _,
                        addr_of_mut!(signature),
                        &mut signature_size,
                    )
                };
                assert!(result.is_ok());
            }
            _ => {
                std::unreachable!()
            }
        }
        unsafe { PCCOR_SIGNATURE(signature.offset(1)) }
    }

    pub fn get_signature_argument_count(
        _metadata: &IMetaDataImport2,
        signature: &mut PCCOR_SIGNATURE,
    ) -> u32 {
        cor_sig_uncompress_data(signature)
    }

    pub fn get_method_argument_count(metadata: &IMetaDataImport2, token: CorTokenType) -> u32 {
        let mut signature = Metadata::get_method_signature(metadata, token);
        Metadata::get_signature_argument_count(metadata, &mut signature)
    }

    pub fn get_custom_attributes_with_name(
        metadata: &IMetaDataImport2,
        token: CorTokenType,
        attribute_name: &str,
    ) -> Vec<u32> {
        // mdCustomAttribute
        let mut attributes = [0_u32; 512];
        let mut attributes_count = 0;
        let mut attributes_enumerator = std::ptr::null_mut();

        let result = unsafe {
            metadata.EnumCustomAttributes(
                addr_of_mut!(attributes_enumerator),
                token.0 as u32,
                0 as _,
                attributes.as_mut_ptr(),
                attributes.len() as u32,
                &mut attributes_count,
            )
        };

        debug_assert!(result.is_ok());
        debug_assert!(attributes_count < (attributes.len().saturating_sub(1)) as u32);

        unsafe { metadata.CloseEnum(attributes_enumerator) }

        // Resolve the desired attribute class to a token once so we can compare
        // tokens (integer equality) in the loop instead of allocating a String
        // per attribute to compare names.
        let attr_hstring = HSTRING::from(attribute_name);
        let mut target_typedef_token = 0_u32;
        let target_resolved = unsafe {
            metadata.FindTypeDefByName(
                PCWSTR(attr_hstring.as_ptr()),
                0 as _,
                &mut target_typedef_token,
            )
        }
        .is_ok();

        let mut filtered_attributes: Vec<u32> = Vec::new();
        for i in 0..attributes_count as usize {
            let attribute = attributes[i];
            let class_token = Metadata::get_custom_attribute_class_token(
                metadata,
                CorTokenType(attribute as i32),
            );

            // Fast path: exact token match (both in same assembly).
            // When the attribute type is defined in a different assembly, the class_token
            // will be a TypeRef (0x01xxxxxx) while target_typedef_token is a TypeDef
            // (0x02xxxxxx) — they represent the same type but are different token values.
            // In that case fall through to the name-based comparison.
            let matches = if target_resolved && class_token == target_typedef_token {
                true
            } else {
                // Fallback: name comparison handles cross-assembly TypeRef vs TypeDef mismatches.
                let name = get_type_name(metadata, CorTokenType(class_token as i32));
                name.as_str() == attribute_name
            };

            if matches {
                filtered_attributes.push(attribute);
            }
        }

        filtered_attributes
    }

    pub fn get_custom_attribute_type_argument(
        metadata: &IMetaDataImport2,
        token: CorTokenType,
    ) -> u32 {
        let mut attribute_value = std::ptr::null_mut() as *const c_void;
        let mut attribute_value_size = 0_u32;

        let result = unsafe {
            metadata.GetCustomAttributeProps(
                token.0 as u32,
                0 as _,
                0 as _,
                addr_of_mut!(attribute_value),
                &mut attribute_value_size,
            )
        };

        debug_assert!(result.is_ok());

        let attribute_value = attribute_value as *mut u8;
        let signature = unsafe { PCCOR_SIGNATURE((attribute_value as *mut u8).offset(2)) };

        let type_name = get_string_value_from_blob(&signature);

        let type_name = HSTRING::from(type_name);
        let type_name = PCWSTR(type_name.as_ptr());

        let mut type_token = 0_u32;

        let result = unsafe { metadata.FindTypeDefByName(type_name, 0 as _, &mut type_token) };

        debug_assert!(result.is_ok());
        type_token
    }

    pub fn get_class_methods(metadata: &IMetaDataImport2, token: CorTokenType) -> Vec<u32> {
        let Some(token) = Self::resolve_methods_owner_token(metadata, token) else {
            return Vec::new();
        };

        let mut methods = [0_u32; 1024];
        let mut methods_count = 0;
        let mut methods_enumerator = std::ptr::null_mut();

        let result = unsafe {
            metadata.EnumMethods(
                addr_of_mut!(methods_enumerator),
                token,
                methods.as_mut_ptr(),
                methods.len() as u32,
                &mut methods_count,
            )
        };
        if result.is_err() {
            if !methods_enumerator.is_null() {
                unsafe { metadata.CloseEnum(methods_enumerator) }
            }
            return Vec::new();
        }
        debug_assert!(methods_count < (methods.len().saturating_sub(1)) as u32);
        if !methods_enumerator.is_null() {
            unsafe { metadata.CloseEnum(methods_enumerator) }
        }

        methods[..methods_count as usize].to_vec()
    }

    pub fn has_method_first_type_argument(
        metadata: &IMetaDataImport2,
        token: CorTokenType,
    ) -> bool {
        let mut signature = Metadata::get_method_signature(metadata, token);
        let argument_count = cor_sig_uncompress_data(&mut signature);

        if argument_count == 0 {
            return false;
        }

        let return_type = cor_sig_uncompress_element_type(&mut signature);

        debug_assert!(return_type == ELEMENT_TYPE_VOID);

        let first_argument = cor_sig_uncompress_element_type(&mut signature);

        if first_argument != ELEMENT_TYPE_CLASS {
            return false;
        }

        let first_argument_token = cor_sig_uncompress_token(&mut signature);
        let first_argument_type_name =
            get_type_name(metadata, CorTokenType(first_argument_token as i32));

        if first_argument_type_name != SYSTEM_TYPE {
            return false;
        }

        return true;
    }

    pub fn declaring_interface_for_initializer(
        metadata: Option<&IMetaDataImport2>,
        method_token: CorTokenType,
        out_index: &mut usize,
    ) -> Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> {
        // InterfaceDeclaration

        match metadata {
            None => None,
            Some(metadata) => {
                let method_argument_count =
                    Metadata::get_method_argument_count(metadata, method_token);

                let class_token =
                    Metadata::get_method_containing_class_token(metadata, method_token);

                debug_assert!(
                    CorTokenType(type_from_token(CorTokenType(class_token as i32))) == mdtTypeDef
                );

                let composable_attributes = Metadata::get_custom_attributes_with_name(
                    metadata,
                    CorTokenType(class_token as i32),
                    COMPOSABLE_ATTRIBUTE,
                );

                for attributeToken in composable_attributes.iter() {
                    let attribute_token = CorTokenType(*attributeToken as i32);
                    let factory_token =
                        Metadata::get_custom_attribute_type_argument(metadata, attribute_token);

                    let factory_methods =
                        Metadata::get_class_methods(metadata, CorTokenType(factory_token as i32));

                    for (i, factoryMethod) in factory_methods.iter().enumerate() {
                        let factory_method_arguments_count = Metadata::get_method_argument_count(
                            metadata,
                            CorTokenType(*factoryMethod as i32),
                        );
                        if factory_method_arguments_count.saturating_sub(2) != method_argument_count
                        {
                            continue;
                        }

                        *out_index = i;
                        return Some(Arc::new(RwLock::new(InterfaceDeclaration::new(
                            Some(metadata),
                            CorTokenType(factory_token as i32),
                        ))));
                    }
                }

                if method_argument_count == 0 {
                    *out_index = usize::MAX;
                    return None;
                }

                let activatable_attributes = Metadata::get_custom_attributes_with_name(
                    metadata,
                    CorTokenType(class_token as i32),
                    ACTIVATABLE_ATTRIBUTE,
                );

                for attributeToken in activatable_attributes.into_iter() {
                    let attribute_constructor_token =
                        Metadata::get_custom_attribute_constructor_token(
                            metadata,
                            CorTokenType(attributeToken as i32),
                        );

                    if !Metadata::has_method_first_type_argument(
                        metadata,
                        CorTokenType(attribute_constructor_token as i32),
                    ) {
                        continue;
                    }

                    let factory_token = Metadata::get_custom_attribute_type_argument(
                        metadata,
                        CorTokenType(attributeToken as i32),
                    );

                    let factory_methods =
                        Metadata::get_class_methods(metadata, CorTokenType(factory_token as i32));

                    for (i, factory_method) in factory_methods.iter().enumerate() {
                        let factory_method_arguments_count = Metadata::get_method_argument_count(
                            metadata,
                            CorTokenType(*factory_method as i32),
                        );

                        if factory_method_arguments_count != method_argument_count {
                            continue;
                        }

                        *out_index = i;
                        return Some(Arc::new(RwLock::new(InterfaceDeclaration::new(
                            Some(metadata),
                            CorTokenType(factory_token as i32),
                        ))));
                    }
                }

                None
            }
        }
    }

    pub fn declaring_interface_for_static_method(
        metadata: Option<&IMetaDataImport2>,
        method_token: CorTokenType,
        out_index: &mut usize,
    ) -> Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> {
        match metadata {
            None => None,
            Some(metadata) => {
                // Retrieve the method signature and its size so we can
                // perform a length-aware comparison against candidate
                // static method signatures. This avoids false-positive
                // prefix matches when signatures differ in length.
                let mut method_sig_ptr = std::ptr::null_mut();
                let mut method_sig_size = 0_u32;
                let _ = unsafe {
                    metadata.GetMethodProps(
                        method_token.0 as u32,
                        0 as _,
                        None,
                        0 as _,
                        0 as _,
                        addr_of_mut!(method_sig_ptr),
                        &mut method_sig_size,
                        0 as _,
                        0 as _,
                    )
                };
                let method_signature = unsafe { PCCOR_SIGNATURE(method_sig_ptr.offset(1)) };
                // Param count, decoded straight from the signature blob (the compressed integer
                // right after the calling-convention byte skipped above). Used below so the
                // name-only fallback can't cross-match a same-named overload with a different
                // arity from a sibling statics interface (e.g. `Launcher.LaunchUriAsync` exists
                // on `ILauncherStatics`/`2`/`3` with 1/2/3 args; a name-only match with no arity
                // guard non-deterministically picks whichever one metadata enumerates first,
                // computing that overload's vtable slot on the WRONG interface. A clean IID
                // mismatch on our side, but the callee has no way to know that, so it reads
                // whatever garbage argument landed where a real parameter should be: an
                // access-violation inside the WinRT DLL, not a catchable JS error).
                let target_param_count = unsafe {
                    let mut p = method_signature;
                    cor_sig_uncompress_data(&mut p)
                };
                let class_token =
                    Metadata::get_method_containing_class_token(metadata, method_token);
                debug_assert!(
                    CorTokenType(type_from_token(CorTokenType(class_token as i32))) == mdtTypeDef
                );

                // Extract the target method name for a fallback name-based match
                let mut target_method_name = String::new();
                {
                    let mut name_length = 0_u32;
                    let mut name_buf = [0_u16; MAX_IDENTIFIER_LENGTH];
                    let _ = unsafe {
                        metadata.GetMethodProps(
                            method_token.0 as u32,
                            0 as _,
                            Some(&mut name_buf),
                            &mut name_length,
                            0 as _,
                            0 as _,
                            0 as _,
                            0 as _,
                            0 as _,
                        )
                    };
                    if name_length > 0 {
                        target_method_name = String::from_utf16_lossy(
                            &name_buf[..name_length.saturating_sub(1) as usize],
                        );
                    }
                }

                let static_attributes = Metadata::get_custom_attributes_with_name(
                    metadata,
                    CorTokenType(class_token as i32),
                    STATIC_ATTRIBUTE,
                );

                let mut ret: Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> = None;
                for attributeToken in static_attributes.iter() {
                    let statics_token = Metadata::get_custom_attribute_type_argument(
                        metadata,
                        CorTokenType(*attributeToken as i32),
                    );

                    let static_methods =
                        Metadata::get_class_methods(metadata, CorTokenType(statics_token as i32));

                    for (i, staticMethod) in static_methods.iter().enumerate() {
                        let mut static_signature = std::ptr::null_mut();
                        let mut static_signature_size = 0_u32;
                        let result = unsafe {
                            metadata.GetMethodProps(
                                *staticMethod,
                                0 as _,
                                None,
                                0 as _,
                                0 as _,
                                addr_of_mut!(static_signature),
                                &mut static_signature_size,
                                0 as _,
                                0 as _,
                            )
                        };
                        debug_assert!(result.is_ok());

                        let candidate_param_count = unsafe {
                            let mut p = PCCOR_SIGNATURE(static_signature.offset(1));
                            cor_sig_uncompress_data(&mut p)
                        };

                        if static_signature_size != method_sig_size {
                            // Signature sizes differ; fall back to name-based
                            // matching (if available). Arity must still match. A same-named
                            // overload on a sibling statics interface (see comment above
                            // `target_param_count`) is a false positive here, not a fallback hit.
                            if !target_method_name.is_empty() && candidate_param_count == target_param_count {
                                let mut name_length = 0_u32;
                                let mut name_buf = [0_u16; MAX_IDENTIFIER_LENGTH];
                                let _ = unsafe {
                                    metadata.GetMethodProps(
                                        *staticMethod,
                                        0 as _,
                                        Some(&mut name_buf),
                                        &mut name_length,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                    )
                                };
                                if name_length == 0 {
                                    continue;
                                }
                                let static_name = String::from_utf16_lossy(
                                    &name_buf[..name_length.saturating_sub(1) as usize],
                                );
                                if static_name != target_method_name {
                                    continue;
                                }
                                // name matches; treat this as the correct statics method
                            } else {
                                continue;
                            }
                        }

                        let a = unsafe {
                            std::slice::from_raw_parts(
                                static_signature.offset(1),
                                static_signature_size as usize - 1,
                            )
                        };

                        let b = unsafe {
                            std::slice::from_raw_parts(
                                method_signature.0,
                                method_sig_size as usize - 1,
                            )
                        };

                        if a != b {
                            // Signature bytes didn't match; attempt a name-based
                            // fallback in case metadata signature encodings differ. Arity must
                            // still match (see comment above `target_param_count`).
                            if !target_method_name.is_empty() && candidate_param_count == target_param_count {
                                let mut name_length = 0_u32;
                                let mut name_buf = [0_u16; MAX_IDENTIFIER_LENGTH];
                                let _ = unsafe {
                                    metadata.GetMethodProps(
                                        *staticMethod,
                                        0 as _,
                                        Some(&mut name_buf),
                                        &mut name_length,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                    )
                                };
                                if name_length == 0 {
                                    continue;
                                }
                                let static_name = String::from_utf16_lossy(
                                    &name_buf[..name_length.saturating_sub(1) as usize],
                                );
                                if static_name != target_method_name {
                                    continue;
                                }
                                // name matches; treat this as the correct statics method
                            } else {
                                continue;
                            }
                        } else {
                            // Signatures matched; ensure the method name also
                            // matches the target to avoid accidental matches
                            // (e.g., differing calling-convention flags).
                            if !target_method_name.is_empty() {
                                let mut name_length = 0_u32;
                                let mut name_buf = [0_u16; MAX_IDENTIFIER_LENGTH];
                                let _ = unsafe {
                                    metadata.GetMethodProps(
                                        *staticMethod,
                                        0 as _,
                                        Some(&mut name_buf),
                                        &mut name_length,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                        0 as _,
                                    )
                                };
                                if name_length == 0 {
                                    continue;
                                }
                                let static_name = String::from_utf16_lossy(
                                    &name_buf[..name_length.saturating_sub(1) as usize],
                                );
                                if static_name != target_method_name {
                                    continue;
                                }
                            }
                        }
                        *out_index = i;
                        ret = Some(Arc::new(RwLock::new(InterfaceDeclaration::new(
                            Some(metadata),
                            CorTokenType(statics_token as i32),
                        ))));
                        break;
                    }
                }

                ret
            }
        }
    }

    pub fn find_method_index(
        metadata: &IMetaDataImport2,
        class_token: CorTokenType,
        method_token: CorTokenType,
    ) -> usize {
        let mut first_method = 0_u32;

        let mut methods_enumerator = std::ptr::null_mut();

        let result = unsafe {
            metadata.EnumMethods(
                addr_of_mut!(methods_enumerator),
                class_token.0 as u32,
                &mut first_method,
                1,
                0 as _,
            )
        };
        debug_assert!(result.is_ok());

        unsafe { metadata.CloseEnum(methods_enumerator) }

        return (method_token.0 as u32 - first_method) as usize;
    }

    pub fn declaring_interface_for_instance_method(
        metadata: Option<&IMetaDataImport2>,
        method_token: CorTokenType,
        out_index: &mut usize,
    ) -> Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> {
        match metadata {
            None => None,
            Some(metadata) => {
                let class_token =
                    Metadata::get_method_containing_class_token(metadata, method_token);
                debug_assert!(type_from_token(CorTokenType(class_token as i32)) == mdtTypeDef.0);

                let mut method_body_tokens = [0_u32; 1024];
                let mut method_decl_tokens = [0_u32; 1024];
                let mut method_impls_count = 0;
                let mut method_impls_enumerator = std::ptr::null_mut();

                let result = unsafe {
                    metadata.EnumMethodImpls(
                        addr_of_mut!(method_impls_enumerator),
                        class_token,
                        method_body_tokens.as_mut_ptr(),
                        method_decl_tokens.as_mut_ptr(),
                        method_body_tokens.len() as u32,
                        &mut method_impls_count,
                    )
                };

                debug_assert!(result.is_ok());

                debug_assert!(
                    method_impls_count < (method_body_tokens.len().saturating_sub(1)) as u32
                );

                unsafe { metadata.CloseEnum(method_impls_enumerator) }

                let mut ret: Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> = None;
                for i in 0..method_impls_count as usize {
                    let method_body_token = method_body_tokens[i];
                    debug_assert!(
                        CorTokenType(type_from_token(CorTokenType(method_body_token as i32)))
                            == mdtMethodDef
                    );

                    if method_token != CorTokenType(method_body_token as i32) {
                        continue;
                    }

                    let method_decl_token = method_decl_tokens[i];
                    match CorTokenType(type_from_token(CorTokenType(method_decl_token as i32))) {
                        mdtMethodDef => {
                            let mut declaring_interface_token = 0_u32;
                            let result = unsafe {
                                metadata.GetMethodProps(
                                    method_decl_token,
                                    &mut declaring_interface_token,
                                    None,
                                    0 as _,
                                    0 as _,
                                    0 as _,
                                    0 as _,
                                    0 as _,
                                    0 as _,
                                )
                            };
                            debug_assert!(result.is_ok());

                            *out_index = Metadata::find_method_index(
                                metadata,
                                CorTokenType(declaring_interface_token as i32),
                                CorTokenType(method_decl_token as i32),
                            );
                            ret = Some(Arc::new(RwLock::new(InterfaceDeclaration::new(
                                Some(metadata),
                                CorTokenType(declaring_interface_token as i32),
                            ))));
                            break;
                        }
                        mdtMemberRef => {
                            let mut parent_token = 0_u32;
                            let result = unsafe {
                                metadata.GetMemberRefProps(
                                    method_decl_token,
                                    &mut parent_token,
                                    None,
                                    0 as _,
                                    0 as _,
                                    0 as _,
                                )
                            };
                            debug_assert!(result.is_ok());

                            match CorTokenType(type_from_token(CorTokenType(parent_token as i32))) {
                                mdtTypeRef => {
                                    let mut external_metadata = MaybeUninit::zeroed();
                                    let mut declaring_interface_token = CorTokenType::default();
                                    let is_resolved = unsafe {
                                        resolve_type_ref(
                                            Some(metadata),
                                            CorTokenType(parent_token as i32),
                                            std::mem::transmute(external_metadata.as_mut_ptr()),
                                            &mut declaring_interface_token,
                                        )
                                    };
                                    debug_assert!(is_resolved);

                                    let mut external_metadata: IMetaDataImport2 =
                                        unsafe { external_metadata.assume_init() };

                                    let mut declaring_method_name = [0_u16; MAX_IDENTIFIER_LENGTH];
                                    let mut declaring_method_name_length = 0_u32;
                                    let mut signature = std::ptr::null_mut();
                                    let mut signature_size = 0;
                                    let result = unsafe {
                                        metadata.GetMemberRefProps(
                                            method_decl_token,
                                            0 as _,
                                            Some(declaring_method_name.as_mut_slice()),
                                            &mut declaring_method_name_length,
                                            addr_of_mut!(signature),
                                            &mut signature_size,
                                        )
                                    };
                                    debug_assert!(result.is_ok());
                                    let mut declaring_method = 0_u32;

                                    let declaring_method_name = String::from_utf16_lossy(
                                        &declaring_method_name
                                            [..declaring_method_name_length as usize],
                                    );
                                    let declaring_method_name =
                                        HSTRING::from(declaring_method_name);
                                    let declaring_method_name =
                                        PCWSTR(declaring_method_name.as_ptr());
                                    let result = unsafe {
                                        external_metadata.FindMethod(
                                            declaring_interface_token.0 as u32,
                                            declaring_method_name,
                                            signature,
                                            signature_size,
                                            &mut declaring_method,
                                        )
                                    };

                                    debug_assert!(result.is_ok());

                                    *out_index = Metadata::find_method_index(
                                        &mut external_metadata,
                                        declaring_interface_token,
                                        CorTokenType(declaring_method as i32),
                                    );

                                    ret = Some(Arc::new(RwLock::new(InterfaceDeclaration::new(
                                        Some(&external_metadata),
                                        declaring_interface_token,
                                    ))));
                                }
                                mdtTypeSpec => {
                                    let mut type_spec_signature = std::ptr::null_mut();
                                    let mut type_spec_signature_size = 0_u32;

                                    let result = unsafe {
                                        metadata.GetTypeSpecFromToken(
                                            parent_token,
                                            addr_of_mut!(type_spec_signature),
                                            &mut type_spec_signature_size,
                                        )
                                    };

                                    debug_assert!(result.is_ok());

                                    let mut declaring_method_name = [0_u16; MAX_IDENTIFIER_LENGTH];
                                    let mut declaring_method_name_size = 0_u32;
                                    let mut signature = std::ptr::null_mut();
                                    let mut signature_size = 0_u32;

                                    let result = unsafe {
                                        metadata.GetMemberRefProps(
                                            method_decl_token,
                                            0 as _,
                                            Some(declaring_method_name.as_mut_slice()),
                                            &mut declaring_method_name_size,
                                            addr_of_mut!(signature),
                                            &mut signature_size,
                                        )
                                    };

                                    debug_assert!(result.is_ok());

                                    let declaring_method_name = String::from_utf16_lossy(
                                        &declaring_method_name
                                            [..declaring_method_name_size as usize],
                                    );
                                    let declaring_method_name =
                                        HSTRING::from(declaring_method_name);
                                    let declaring_method_name =
                                        PCWSTR(declaring_method_name.as_ptr());

                                    let mut type_spec_signature =
                                        PCCOR_SIGNATURE(type_spec_signature);

                                    let type1 =
                                        cor_sig_uncompress_element_type(&mut type_spec_signature);
                                    debug_assert!(type1 == ELEMENT_TYPE_GENERICINST);

                                    let type2 =
                                        cor_sig_uncompress_element_type(&mut type_spec_signature);
                                    debug_assert!(type2 == ELEMENT_TYPE_CLASS);

                                    // TODO: Use signature in matching
                                    let open_generic_class_token =
                                        cor_sig_uncompress_token(&mut type_spec_signature);
                                    match CorTokenType(type_from_token(CorTokenType(
                                        open_generic_class_token as i32,
                                    ))) {
                                        mdtTypeDef => {
                                            let mut declaring_method = 0_u32;
                                            let result = unsafe {
                                                metadata.FindMethod(
                                                    open_generic_class_token,
                                                    declaring_method_name,
                                                    0 as _,
                                                    0 as _,
                                                    &mut declaring_method,
                                                )
                                            };
                                            debug_assert!(result.is_ok());
                                            *out_index = Metadata::find_method_index(
                                                metadata,
                                                CorTokenType(open_generic_class_token as i32),
                                                CorTokenType::default(),
                                            );

                                            ret = Some(Arc::new(RwLock::new(
                                                GenericInterfaceInstanceDeclaration::new(
                                                    Some(metadata),
                                                    CorTokenType(open_generic_class_token as i32),
                                                    Some(metadata),
                                                    CorTokenType(parent_token as i32),
                                                ),
                                            )));
                                        }
                                        mdtTypeRef => {
                                            let mut external_metadata = MaybeUninit::zeroed();
                                            let mut external_class_token = CorTokenType::default();

                                            let is_resolved = unsafe {
                                                resolve_type_ref(
                                                    Some(metadata),
                                                    CorTokenType(open_generic_class_token as i32),
                                                    std::mem::transmute(
                                                        external_metadata.as_mut_ptr(),
                                                    ),
                                                    &mut external_class_token,
                                                )
                                            };
                                            debug_assert!(is_resolved);

                                            let mut external_metadata: IMetaDataImport2 =
                                                unsafe { external_metadata.assume_init() };

                                            let mut declaring_method = 0_u32;

                                            let result = unsafe {
                                                external_metadata.FindMethod(
                                                    external_class_token.0 as u32,
                                                    declaring_method_name,
                                                    0 as _,
                                                    0 as _,
                                                    &mut declaring_method,
                                                )
                                            };

                                            debug_assert!(result.is_ok());

                                            *out_index = Metadata::find_method_index(
                                                &mut external_metadata,
                                                external_class_token,
                                                CorTokenType(declaring_method as i32),
                                            );
                                            ret = Some(Arc::new(RwLock::new(
                                                GenericInterfaceInstanceDeclaration::new(
                                                    Some(&external_metadata),
                                                    external_class_token,
                                                    Some(metadata),
                                                    CorTokenType(parent_token as i32),
                                                ),
                                            )));
                                        }
                                        _ => {
                                            std::unreachable!()
                                        }
                                    }
                                }
                                _ => {
                                    std::unreachable!()
                                }
                            }
                        }
                        _ => {
                            std::unreachable!();
                        }
                    }
                }
                ret
            }
        }
    }

    pub fn find_declaring_interface_for_method(
        method: &MethodDeclaration,
        out_index: &mut usize,
    ) -> Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> {
        debug_assert!(*out_index == 0);
        let method_token = method.token();

        if method.is_static() {
            return Metadata::declaring_interface_for_static_method(
                method.metadata(),
                method_token,
                out_index,
            );
        }

        if method.is_initializer() {
            return Metadata::declaring_interface_for_initializer(
                method.metadata(),
                method_token,
                out_index,
            );
        }

        Metadata::declaring_interface_for_instance_method(
            method.metadata(),
            method_token,
            out_index,
        )
    }
}
