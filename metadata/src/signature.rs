#![allow(non_upper_case_globals)]

use crate::prelude::*;
use std::mem::MaybeUninit;
use windows::core::Interface;
use windows::Win32::System::WinRT::Metadata::{
    mdtTypeDef, mdtTypeRef, CorElementType, CorTokenType, IMetaDataImport2, ELEMENT_TYPE_BOOLEAN,
    ELEMENT_TYPE_BYREF, ELEMENT_TYPE_CHAR, ELEMENT_TYPE_CLASS, ELEMENT_TYPE_GENERICINST,
    ELEMENT_TYPE_I1, ELEMENT_TYPE_I2, ELEMENT_TYPE_I4, ELEMENT_TYPE_I8, ELEMENT_TYPE_OBJECT,
    ELEMENT_TYPE_R4, ELEMENT_TYPE_R8, ELEMENT_TYPE_STRING, ELEMENT_TYPE_SZARRAY, ELEMENT_TYPE_U1,
    ELEMENT_TYPE_U2, ELEMENT_TYPE_U4, ELEMENT_TYPE_U8, ELEMENT_TYPE_VALUETYPE, ELEMENT_TYPE_VAR,
    ELEMENT_TYPE_VOID,
};

const Guid: &str = "Guid";

/// Returns true when `token` is a TypeDef (or a TypeRef that resolves to one) whose base
/// class is `System.Enum` — the invariant for all WinRT enum types.
/// WinRT enums are always backed by a 32-bit integer on the ABI wire.
fn is_enum_type(metadata: &IMetaDataImport2, token: CorTokenType) -> bool {
    let token_kind = CorTokenType(type_from_token(token));

    // For TypeRef tokens, resolve to the TypeDef in the external metadata scope first.
    if token_kind == mdtTypeRef {
        let key = (metadata.as_raw() as usize, token.0);
        if let Some(hit) = TYPE_REF_IS_ENUM_CACHE.with(|c| c.borrow().get(&key).copied()) {
            return hit;
        }
        // resolve_type_ref opens the metadata file that owns the referenced type and
        // returns both the external IMetaDataImport2 scope and the TypeDef token within it.
        let mut ext_metadata: MaybeUninit<IMetaDataImport2> = MaybeUninit::zeroed();
        let mut ext_token = token;
        let is_enum = resolve_type_ref(
            Some(metadata),
            token,
            unsafe { ext_metadata.assume_init_mut() },
            &mut ext_token,
        ) && {
            let ext_metadata = unsafe { ext_metadata.assume_init() };
            is_enum_type(&ext_metadata, ext_token)
        };
        TYPE_REF_IS_ENUM_CACHE.with(|c| c.borrow_mut().insert(key, is_enum));
        return is_enum;
    }

    if token_kind != mdtTypeDef {
        return false;
    }

    let mut extends = 0u32;
    let mut len = 0u32;
    let ok = unsafe {
        metadata
            .GetTypeDefProps(token.0 as u32, None, &mut len, 0 as _, &mut extends)
            .is_ok()
    };
    if !ok || extends == 0 {
        return false;
    }
    let extends_kind = CorTokenType(type_from_token(CorTokenType(extends as i32)));
    if extends_kind != mdtTypeDef && extends_kind != mdtTypeRef {
        return false;
    }
    let mut name = get_fully_qualified_type_name(metadata, CorTokenType(extends as i32));
    if let Some(pos) = name.find('`') {
        name.truncate(pos);
    }
    name == SYSTEM_ENUM
}

thread_local! {
    // (metadata scope, TypeRef token) → resolved name. Resolving a TypeRef goes through
    // RoGetMetaDataFile (a winmd lookup), and signatures are re-stringified on hot paths such as
    // wrapping every returned WinRT object, so this dominated those calls uncached.
    static TYPE_REF_NAME_CACHE: std::cell::RefCell<ahash::AHashMap<(usize, i32), String>> =
        std::cell::RefCell::new(ahash::AHashMap::new());
    static TYPE_REF_IS_ENUM_CACHE: std::cell::RefCell<ahash::AHashMap<(usize, i32), bool>> =
        std::cell::RefCell::new(ahash::AHashMap::new());
}

/// Returns the fully qualified name (namespace + type name) for the given token.
fn get_fully_qualified_type_name(metadata: &IMetaDataImport2, token: CorTokenType) -> String {
    let token_kind = CorTokenType(type_from_token(token));
    if token_kind == mdtTypeRef {
        let key = (metadata.as_raw() as usize, token.0);
        if let Some(name) = TYPE_REF_NAME_CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return name;
        }
        let mut ext_metadata: MaybeUninit<IMetaDataImport2> = MaybeUninit::zeroed();
        let mut ext_token = token;
        if resolve_type_ref(
            Some(metadata),
            token,
            unsafe { ext_metadata.assume_init_mut() },
            &mut ext_token,
        ) {
            // RoGetMetaDataFile handed us an owned reference: take it so it is released.
            let ext_metadata = unsafe { ext_metadata.assume_init() };
            let name = get_type_name(&ext_metadata, ext_token);
            TYPE_REF_NAME_CACHE.with(|c| c.borrow_mut().insert(key, name.clone()));
            return name;
        }
    }
    get_type_name(metadata, token)
}

pub struct Signature {}

impl Signature {
    pub fn consume_type(signature: &mut PCCOR_SIGNATURE) -> PCCOR_SIGNATURE {
        let start = signature.clone();

        let element_type = cor_sig_uncompress_element_type(signature);

        match element_type {
            ELEMENT_TYPE_VOID | ELEMENT_TYPE_BOOLEAN | ELEMENT_TYPE_CHAR | ELEMENT_TYPE_I1
            | ELEMENT_TYPE_U1 | ELEMENT_TYPE_I2 | ELEMENT_TYPE_U2 | ELEMENT_TYPE_I4
            | ELEMENT_TYPE_U4 | ELEMENT_TYPE_I8 | ELEMENT_TYPE_U8 | ELEMENT_TYPE_R4
            | ELEMENT_TYPE_R8 | ELEMENT_TYPE_STRING => start,
            ELEMENT_TYPE_VALUETYPE => {
                cor_sig_uncompress_token(signature);
                start
            }
            ELEMENT_TYPE_CLASS => {
                cor_sig_uncompress_token(signature);
                start
            }
            ELEMENT_TYPE_OBJECT => start,
            ELEMENT_TYPE_SZARRAY => {
                Signature::consume_type(signature);
                start
            }
            ELEMENT_TYPE_VAR => {
                cor_sig_uncompress_data(signature);
                start
            }
            ELEMENT_TYPE_GENERICINST => {
                cor_sig_uncompress_element_type(signature);
                cor_sig_uncompress_token(signature);
                let generic_arguments_count = cor_sig_uncompress_data(signature);
                for _ in 0..generic_arguments_count {
                    Signature::consume_type(signature);
                }
                start
            }
            ELEMENT_TYPE_BYREF => {
                Signature::consume_type(signature);
                start
            }
            _ => {
                // Unknown element type — skip
                start
            }
        }
    }

    fn get_string(
        metadata: Option<&IMetaDataImport2>,
        signature: &PCCOR_SIGNATURE,
        preserve_arity: bool,
    ) -> String {
        let mut signature = signature.clone();

        let element_type = cor_sig_uncompress_element_type(&mut signature);

        return match element_type {
            ELEMENT_TYPE_VOID => "Void".to_string(),
            ELEMENT_TYPE_BOOLEAN => "Boolean".to_string(),
            ELEMENT_TYPE_CHAR => "Char16".to_string(),
            ELEMENT_TYPE_I1 => "Int8".to_string(),
            ELEMENT_TYPE_U1 => "UInt8".to_string(),
            ELEMENT_TYPE_I2 => "Int16".to_string(),
            ELEMENT_TYPE_U2 => "UInt16".to_string(),
            ELEMENT_TYPE_I4 => "Int32".to_string(),
            ELEMENT_TYPE_U4 => "UInt32".to_string(),
            ELEMENT_TYPE_I8 => "Int64".to_string(),
            ELEMENT_TYPE_U8 => "UInt64".to_string(),
            ELEMENT_TYPE_R4 => "Single".to_string(),
            ELEMENT_TYPE_R8 => "Double".to_string(),
            ELEMENT_TYPE_STRING => "String".to_string(),
            ELEMENT_TYPE_VALUETYPE => {
                let token = cor_sig_uncompress_token(&mut signature);
                if let Some(metadata_ref) = metadata {
                    let token_type = CorTokenType(token as i32);

                    // WinRT enums are ELEMENT_TYPE_VALUETYPE but must be passed as
                    // Int32 on the ABI — not as a COM pointer.
                    if is_enum_type(metadata_ref, token_type) {
                        return "Int32".to_string();
                    }

                    let class_name = get_fully_qualified_type_name(metadata_ref, token_type);
                    if class_name.eq("System.Guid") {
                        Guid.to_string()
                    } else {
                        class_name
                    }
                } else {
                    // Fallback when metadata is not available (e.g. for as_string debug output)
                    format!("ValueType(0x{:08X})", token)
                }
            }
            ELEMENT_TYPE_CLASS => {
                let token = cor_sig_uncompress_token(&mut signature);
                if let Some(metadata_ref) = metadata {
                    get_fully_qualified_type_name(metadata_ref, CorTokenType(token as i32))
                } else {
                    // Fallback when metadata is not available
                    "Object".to_string()
                }
            }
            ELEMENT_TYPE_OBJECT => "Object".to_string(),
            ELEMENT_TYPE_SZARRAY => {
                let result = Signature::get_string(metadata, &mut signature, preserve_arity);
                format!("{}[]", result)
            }
            ELEMENT_TYPE_VAR => {
                let index = cor_sig_uncompress_data(&mut signature);
                format!("Var!{}", index)
            }
            ELEMENT_TYPE_GENERICINST => {
                let generic_type = cor_sig_uncompress_element_type(&mut signature);

                assert_eq!(generic_type, ELEMENT_TYPE_CLASS);

                let token = cor_sig_uncompress_token(&mut signature);

                let mut result = if let Some(metadata_ref) = metadata {
                    let mut name =
                        get_fully_qualified_type_name(metadata_ref, CorTokenType(token as i32));
                    if !preserve_arity {
                        if let Some(pos) = name.find('`') {
                            name.truncate(pos);
                        }
                    }
                    name
                } else {
                    "Object".to_string()
                };

                result += "<";

                let generic_arguments_count = cor_sig_uncompress_data(&mut signature);

                for i in 0..generic_arguments_count {
                    let mut sig_type = Signature::consume_type(&mut signature);
                    let data = Signature::get_string(metadata, &mut sig_type, preserve_arity);

                    result += data.as_ref();
                    if i != generic_arguments_count.saturating_sub(1) {
                        result += ", ";
                    }
                }

                result += ">";

                result
            }
            ELEMENT_TYPE_BYREF => {
                let mut result = "ByRef ".to_string();
                result += Signature::get_string(metadata, &mut signature, preserve_arity).as_ref();
                result
            }
            _ => "Object".to_string(),
        };
    }

    pub fn to_string(metadata: &IMetaDataImport2, signature: &PCCOR_SIGNATURE) -> String {
        Signature::get_string(Some(metadata), signature, true)
    }

    /// Like `to_string` but keeps backtick+arity on generic names — required by `RoParseTypeName`.
    pub fn to_iid_string(metadata: &IMetaDataImport2, signature: &PCCOR_SIGNATURE) -> String {
        Signature::get_string(Some(metadata), signature, true)
    }

    pub fn as_string(signature: &PCCOR_SIGNATURE) -> String {
        Signature::get_string(None, signature, false)
    }

    /// Like `to_string` but returns the actual enum type name (e.g.
    /// "Windows.Web.Http.HttpProgressStage") instead of the underlying primitive
    /// ("Int32") for ELEMENT_TYPE_VALUETYPE enums. Required for SetStruct field
    /// type names so that struct field signatures end up as `enum(name;i4)`
    /// rather than primitive `i4` in the composed PIID signature.
    pub fn to_struct_field_name(
        metadata: &IMetaDataImport2,
        signature: &PCCOR_SIGNATURE,
    ) -> String {
        Signature::get_struct_field_name(metadata, signature)
    }

    fn get_struct_field_name(metadata: &IMetaDataImport2, signature: &PCCOR_SIGNATURE) -> String {
        let mut sig = signature.clone();
        let element_type = cor_sig_uncompress_element_type(&mut sig);
        match element_type {
            ELEMENT_TYPE_VALUETYPE => {
                let token = cor_sig_uncompress_token(&mut sig);
                let token_type = CorTokenType(token as i32);
                let full_name = get_fully_qualified_type_name(metadata, token_type);
                if full_name == "System.Guid" {
                    "Guid".to_string()
                } else {
                    // Whether enum or non-enum value type, return the actual name.
                    full_name
                }
            }
            _ => Signature::get_string(Some(metadata), signature, true),
        }
    }

    /// Returns the WinRT wire-format signature for a type (e.g. "u8", "i4",
    /// "struct(Name;field;...)", "enum(Name;i4)", "{IID}", "pinterface({piid};arg1;...)").
    /// This is the format used internally by `RoGetParameterizedTypeInstanceIID`
    /// when composing the parameterized type's hashed signature, and is the
    /// correct format for `IRoSimpleMetaDataBuilder::SetStruct` field signatures.
    pub fn to_wire_signature(metadata: &IMetaDataImport2, signature: &PCCOR_SIGNATURE) -> String {
        Signature::get_wire_signature(Some(metadata), signature)
    }

    fn get_wire_signature(
        metadata: Option<&IMetaDataImport2>,
        signature: &PCCOR_SIGNATURE,
    ) -> String {
        let mut signature = signature.clone();
        let element_type = cor_sig_uncompress_element_type(&mut signature);

        match element_type {
            ELEMENT_TYPE_VOID => "void".to_string(),
            ELEMENT_TYPE_BOOLEAN => "b1".to_string(),
            ELEMENT_TYPE_CHAR => "c2".to_string(),
            ELEMENT_TYPE_I1 => "i1".to_string(),
            ELEMENT_TYPE_U1 => "u1".to_string(),
            ELEMENT_TYPE_I2 => "i2".to_string(),
            ELEMENT_TYPE_U2 => "u2".to_string(),
            ELEMENT_TYPE_I4 => "i4".to_string(),
            ELEMENT_TYPE_U4 => "u4".to_string(),
            ELEMENT_TYPE_I8 => "i8".to_string(),
            ELEMENT_TYPE_U8 => "u8".to_string(),
            ELEMENT_TYPE_R4 => "f4".to_string(),
            ELEMENT_TYPE_R8 => "f8".to_string(),
            ELEMENT_TYPE_STRING => "string".to_string(),
            ELEMENT_TYPE_OBJECT => "cinterface(IInspectable)".to_string(),
            ELEMENT_TYPE_VALUETYPE => {
                let token = cor_sig_uncompress_token(&mut signature);
                let Some(meta) = metadata else {
                    return format!("ValueType(0x{:08X})", token);
                };
                let token_type = CorTokenType(token as i32);
                let full_name = get_fully_qualified_type_name(meta, token_type);
                if full_name == "System.Guid" {
                    return "g16".to_string();
                }
                // Enum?
                if is_enum_type(meta, token_type) {
                    return format!("enum({};i4)", full_name);
                }
                // Otherwise: external struct or value type — look up via MetadataReader-like recursive lookup
                Signature::wire_signature_for_named_type(&full_name).unwrap_or(full_name)
            }
            ELEMENT_TYPE_CLASS => {
                let token = cor_sig_uncompress_token(&mut signature);
                let Some(meta) = metadata else {
                    return "cinterface(IInspectable)".to_string();
                };
                let full_name = get_fully_qualified_type_name(meta, CorTokenType(token as i32));
                Signature::wire_signature_for_named_type(&full_name).unwrap_or(full_name)
            }
            ELEMENT_TYPE_GENERICINST => {
                let _ = cor_sig_uncompress_element_type(&mut signature);
                let token = cor_sig_uncompress_token(&mut signature);
                let Some(meta) = metadata else {
                    return "cinterface(IInspectable)".to_string();
                };
                let open_name = get_fully_qualified_type_name(meta, CorTokenType(token as i32));
                // open_name includes `N suffix (e.g. "Windows.Foundation.IReference`1")
                let generic_arguments_count = cor_sig_uncompress_data(&mut signature);
                let mut arg_sigs: Vec<String> =
                    Vec::with_capacity(generic_arguments_count as usize);
                for _ in 0..generic_arguments_count {
                    let mut sig_type = Signature::consume_type(&mut signature);
                    arg_sigs.push(Signature::get_wire_signature(metadata, &mut sig_type));
                }
                // Determine the open generic's PIID. We need to look it up.
                let open_piid_str = Signature::open_generic_piid_or_name(&open_name);
                format!("pinterface({};{})", open_piid_str, arg_sigs.join(";"))
            }
            ELEMENT_TYPE_VAR => {
                let index = cor_sig_uncompress_data(&mut signature);
                format!("Var!{}", index)
            }
            ELEMENT_TYPE_SZARRAY => {
                let result = Signature::get_wire_signature(metadata, &mut signature);
                format!("{}[]", result)
            }
            ELEMENT_TYPE_BYREF => Signature::get_wire_signature(metadata, &mut signature),
            _ => "cinterface(IInspectable)".to_string(),
        }
    }

    /// Given a fully-qualified type name (no generic args), returns its WinRT wire-format
    /// signature. Used for nested signatures (struct fields, parameterized args).
    /// Falls back to the bare name if the type can't be resolved.
    fn wire_signature_for_named_type(_full_name: &str) -> Option<String> {
        // Resolution requires the higher-level MetadataReader; let the caller wrap
        // recursively. For now, return None so callers fall back to the bare name —
        // see GenericInstanceIdBuilder's Locate for proper struct-field handling.
        None
    }

    /// Returns the open-generic PIID as `{GUID}` string for known open generics,
    /// or the type name as a placeholder otherwise.
    fn open_generic_piid_or_name(open_name: &str) -> String {
        // Strip backtick-arity for matching the well-known list below.
        let base = open_name.split('`').next().unwrap_or(open_name);
        let piid: Option<&str> = match base {
            "Windows.Foundation.IReference" => Some("{61c17706-2d65-11e0-9ae8-d48564015472}"),
            "Windows.Foundation.IAsyncOperation" => Some("{9fc2b0bb-e446-44e2-aa61-9cab8f636af2}"),
            "Windows.Foundation.IAsyncOperationWithProgress" => {
                Some("{b5d036d7-e297-498f-ba60-0289e76e23dd}")
            }
            "Windows.Foundation.IAsyncActionWithProgress" => {
                Some("{1f6db258-e803-48a1-9546-eb7353398884}")
            }
            "Windows.Foundation.AsyncOperationCompletedHandler" => {
                Some("{fcdcf02c-e5d8-4478-915a-4d90b74b83a5}")
            }
            "Windows.Foundation.AsyncOperationProgressHandler" => {
                Some("{55690902-0aab-421a-8778-f8ce5026d758}")
            }
            "Windows.Foundation.AsyncOperationWithProgressCompletedHandler" => {
                Some("{e85df41d-6aa7-46e3-a8e2-f009d840c627}")
            }
            "Windows.Foundation.AsyncActionProgressHandler" => {
                Some("{6d844858-0cff-4590-ae89-95a5a5c8b4b8}")
            }
            "Windows.Foundation.AsyncActionWithProgressCompletedHandler" => {
                Some("{9c029f91-cc84-44fd-ac26-0a6cd0a47db4}")
            }
            "Windows.Foundation.EventHandler" => Some("{9de1c534-6ae1-11e0-84e1-18a905bcc53f}"),
            "Windows.Foundation.TypedEventHandler" => {
                Some("{9de1c535-6ae1-11e0-84e1-18a905bcc53f}")
            }
            "Windows.Foundation.Collections.IIterator" => {
                Some("{6a79e863-4300-459a-9966-cbb660963ee1}")
            }
            "Windows.Foundation.Collections.IIterable" => {
                Some("{faa585ea-6214-4217-afda-7f46de5869b3}")
            }
            "Windows.Foundation.Collections.IVectorView" => {
                Some("{bbe1fa4c-b0e3-4583-baef-1f1b2e483e56}")
            }
            "Windows.Foundation.Collections.IVector" => {
                Some("{913337e9-11a1-4345-a3a2-4e7f956e222d}")
            }
            "Windows.Foundation.Collections.IMap" => Some("{3c2925fe-8519-45c1-aa79-197b6718c1c1}"),
            "Windows.Foundation.Collections.IMapView" => {
                Some("{e480ce40-a338-4ada-adcf-272272e48cb9}")
            }
            "Windows.Foundation.Collections.IKeyValuePair" => {
                Some("{02b51929-c1c4-4a7e-8940-0312b5c18500}")
            }
            "Windows.Foundation.Collections.IObservableVector" => {
                Some("{5917eb53-50b4-4a0d-b309-65862b3f1dbc}")
            }
            "Windows.Foundation.Collections.IObservableMap" => {
                Some("{65df2bf5-bf39-41b5-aebc-5a9d865e472b}")
            }
            "Windows.Foundation.Collections.VectorChangedEventHandler" => {
                Some("{0c051752-9fbf-4c70-aa0c-0e4c82d9a761}")
            }
            "Windows.Foundation.Collections.MapChangedEventHandler" => {
                Some("{179517f3-94ee-41f8-bddc-768a895544f3}")
            }
            _ => None,
        };
        match piid {
            Some(s) => s.to_string(),
            None => open_name.to_string(),
        }
    }

    pub fn get_signature_element_type(signature: &PCCOR_SIGNATURE) -> CorElementType {
        let mut signature = signature.clone();

        cor_sig_uncompress_element_type(&mut signature)
    }
}
