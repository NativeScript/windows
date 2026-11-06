use crate::declarations::class_declaration::ClassDeclaration;
use crate::declarations::declaration::{Declaration, DeclarationKind};
use crate::declarations::delegate_declaration::generic_delegate_declaration::GenericDelegateDeclaration;
use crate::declarations::delegate_declaration::generic_delegate_instance_declaration::GenericDelegateInstanceDeclaration;
use crate::declarations::delegate_declaration::{DelegateDeclaration, DelegateDeclarationImpl};
use crate::declarations::enum_declaration::EnumDeclaration;
use crate::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
use crate::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use crate::declarations::interface_declaration::InterfaceDeclaration;
use crate::declarations::struct_declaration::StructDeclaration;
use crate::meta_data_reader::MetadataReader;
use crate::signature::Signature;
use std::ffi::OsString;
use std::os::windows::prelude::OsStringExt;
use windows::core::{Ref, GUID, HSTRING, PCWSTR};
use windows::Win32::System::WinRT::Metadata::{
    IRoMetaDataLocator, IRoMetaDataLocator_Impl, IRoSimpleMetaDataBuilder,
    RoGetParameterizedTypeInstanceIID, RoParseTypeName,
};

pub struct GenericInstanceIdBuilder {}

#[derive(Clone)]
pub struct IRoMetaDataLocatorImpl;

impl IRoMetaDataLocator_Impl for IRoMetaDataLocatorImpl {
    fn Locate(
        &self,
        nameelement: &PCWSTR,
        metadatadestination: Ref<'_, IRoSimpleMetaDataBuilder>,
    ) -> windows::core::Result<()> {
        let name_os = OsString::from_wide(unsafe { nameelement.as_wide() });
        let name_str = name_os.to_string_lossy();

        let declaration = MetadataReader::find_by_name(name_str.as_ref());

        let name = PCWSTR(nameelement.as_ptr());

        match declaration.as_ref() {
            None => {
                return Ok(());
            }
            Some(declaration) => {
                let declaration = declaration.read();
                let kind = declaration.kind();

                match kind {
                    DeclarationKind::Class => {
                        let class_declaration = declaration
                            .as_any()
                            .downcast_ref::<ClassDeclaration>()
                            .unwrap();

                        let default_interface = class_declaration.default_interface().unwrap();
                        let default_interface_id = default_interface.id();
                        let full_name = HSTRING::from(default_interface.full_name());
                        let full_name = PCWSTR::from_raw(full_name.as_ptr());

                        if let Ok(builder) = metadatadestination.ok() {
                            let result = unsafe {
                                builder.SetRuntimeClassSimpleDefault(
                                    name,
                                    full_name,
                                    Some(&default_interface_id),
                                )
                            };

                            debug_assert!(result.is_ok());
                        }

                        return Ok(());
                    }
                    DeclarationKind::Interface => {
                        let interface_declaration = declaration
                            .as_any()
                            .downcast_ref::<InterfaceDeclaration>()
                            .unwrap();
                        let interface_declaration_id = interface_declaration.id();

                        if let Ok(builder) = metadatadestination.ok() {
                            let result =
                                unsafe { builder.SetWinRtInterface(interface_declaration_id) };

                            debug_assert!(result.is_ok())
                        }
                        return Ok(());
                    }
                    DeclarationKind::GenericInterface => {
                        let generic_interface_declaration = declaration
                            .as_any()
                            .downcast_ref::<GenericInterfaceDeclaration>()
                            .unwrap();

                        if let Ok(builder) = metadatadestination.ok() {
                            return unsafe {
                                builder.SetParameterizedInterface(
                                    generic_interface_declaration.id(),
                                    generic_interface_declaration.number_of_generic_parameters()
                                        as u32,
                                )
                            };
                        }
                        return Ok(());
                    }
                    DeclarationKind::Enum => {
                        let enum_declaration = declaration
                            .as_any()
                            .downcast_ref::<EnumDeclaration>()
                            .unwrap();
                        let type_ = enum_declaration.type_();
                        let full_name = HSTRING::from(enum_declaration.full_name());
                        // Use the primitive's friendly name ("Int32"/"UInt32") so the runtime
                        // recognizes it as a built-in primitive. Wire format ("i4") triggers
                        // a recursive Locate that fails to resolve.
                        let signature_str = enum_declaration
                            .metadata()
                            .map(|m| Signature::to_string(m, &type_))
                            .unwrap_or_else(|| "Int32".to_string());
                        let signature = HSTRING::from(signature_str.as_str());

                        if let Ok(builder) = metadatadestination.ok() {
                            let full_name = PCWSTR(full_name.as_ptr());
                            let signature = PCWSTR(signature.as_ptr());
                            let result = unsafe { builder.SetEnum(full_name, signature) };

                            debug_assert!(result.is_ok());
                        }

                        return Ok(());
                    }
                    DeclarationKind::Struct => {
                        let struct_declaration = declaration
                            .as_any()
                            .downcast_ref::<StructDeclaration>()
                            .unwrap();

                        if let Ok(builder) = metadatadestination.ok() {
                            let mut field_names = Vec::new();
                            for field in struct_declaration.fields().iter() {
                                let field_type = field.type_();
                                let signature = Signature::to_struct_field_name(
                                    field.base().metadata().unwrap(),
                                    &field_type,
                                );
                                let signature = HSTRING::from(signature);
                                field_names.push(signature);
                            }

                            let full_name = HSTRING::from(struct_declaration.full_name());
                            let full_name = PCWSTR::from_raw(full_name.as_ptr());

                            let field_names: Vec<PCWSTR> = field_names
                                .iter()
                                .map(|field| PCWSTR(field.as_ptr()))
                                .collect();

                            let result =
                                unsafe { builder.SetStruct(full_name, field_names.as_slice()) };

                            debug_assert!(result.is_ok());
                        }

                        return Ok(());
                    }
                    DeclarationKind::Delegate => {
                        let delegate_declaration = declaration
                            .as_any()
                            .downcast_ref::<DelegateDeclaration>()
                            .unwrap();

                        if let Ok(builder) = metadatadestination.ok() {
                            let result = unsafe { builder.SetDelegate(delegate_declaration.id()) };

                            debug_assert!(result.is_ok());
                        }

                        return Ok(());
                    }
                    DeclarationKind::GenericDelegate => {
                        let generic_delegate_declaration = declaration
                            .as_any()
                            .downcast_ref::<GenericDelegateDeclaration>()
                            .unwrap();

                        if let Ok(builder) = metadatadestination.ok() {
                            let result = unsafe {
                                builder.SetParameterizedDelegate(
                                    generic_delegate_declaration.id(),
                                    generic_delegate_declaration.number_of_generic_parameters()
                                        as u32,
                                )
                            };

                            debug_assert!(result.is_ok());
                        }

                        return Ok(());
                    }
                    DeclarationKind::GenericInterfaceInstance => {
                        let open_name = name_str.split('<').next().unwrap_or(&name_str);
                        if let Some(open_decl) = MetadataReader::find_by_name(open_name) {
                            let open_lock = open_decl.read();
                            if let Some(open_iface) = open_lock
                                .as_any()
                                .downcast_ref::<GenericInterfaceDeclaration>()
                            {
                                if let Ok(builder) = metadatadestination.ok() {
                                    let result = unsafe {
                                        builder.SetParameterizedInterface(
                                            open_iface.id(),
                                            open_iface.number_of_generic_parameters() as u32,
                                        )
                                    };
                                    debug_assert!(result.is_ok());
                                }
                            }
                        }
                        return Ok(());
                    }
                    DeclarationKind::GenericDelegateInstance => {
                        // Closed generic delegate — register as parameterized delegate
                        // so the runtime recursively composes the signature.
                        let open_name = name_str.split('<').next().unwrap_or(&name_str);
                        if let Some(open_decl) = MetadataReader::find_by_name(open_name) {
                            let open_lock = open_decl.read();
                            if let Some(open_del) = open_lock
                                .as_any()
                                .downcast_ref::<GenericDelegateDeclaration>()
                            {
                                if let Ok(builder) = metadatadestination.ok() {
                                    let result = unsafe {
                                        builder.SetParameterizedDelegate(
                                            open_del.id(),
                                            open_del.number_of_generic_parameters() as u32,
                                        )
                                    };
                                    debug_assert!(result.is_ok());
                                }
                            }
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        unreachable!();
    }
}

impl GenericInstanceIdBuilder {
    /// Computes the WinRT parameterized type instance IID for a closed generic
    /// type name (e.g. "Windows.Foundation.IReference`1<UInt64>").
    ///
    /// We bypass the OS `RoGetParameterizedTypeInstanceIID` because its
    /// `IRoSimpleMetaDataBuilder` callback API composes the type signature in
    /// a way that doesn't reliably reproduce the canonical IIDs embedded in the
    /// Windows SDK headers. Instead we compose the canonical wire-format
    /// signature ourselves and SHA-1 hash it with the cppwinrt namespace UUID
    /// — the same algorithm used at SDK header generation time.
    pub fn generate_id_from_name(iid_name: &str) -> GUID {
        let normalized = iid_name.replace(", ", ",");
        let Some(signature) = wire_signature_for_name(&normalized) else {
            return GUID::zeroed();
        };
        compute_winrt_piid(&signature)
    }

    /// The IID of `IReference<D>` for the non-generic delegate type `D` whose IID is
    /// `delegate_iid`: the interface a delegate boxed as an `Object` answers.
    pub fn ireference_of_delegate(delegate_iid: &GUID) -> GUID {
        compute_winrt_piid(&format!(
            "pinterface({{61c17706-2d65-11e0-9ae8-d48564015472}};delegate({}))",
            format_guid(delegate_iid)
        ))
    }
}

/// Recursively computes the canonical WinRT wire-format signature for a type
/// referenced by its fully-qualified name (with backtick+arity preserved on
/// generics). Returns None if the name cannot be resolved.
fn wire_signature_for_name(name: &str) -> Option<String> {
    // Primitives — short-circuit before metadata lookup.
    if let Some(prim) = primitive_wire_signature(name) {
        return Some(prim.to_string());
    }

    // Parameterized type instance: "Name`N<arg1,arg2,...>"
    if let Some(angle) = name.find('<') {
        let close = name.rfind('>')?;
        let open_name = &name[..angle];
        let inner = &name[angle + 1..close];
        let args = split_type_args(inner);

        let open_decl = MetadataReader::find_by_name(open_name)?;
        let open_lock = open_decl.read();
        let (piid, is_delegate) = match open_lock.kind() {
            DeclarationKind::GenericInterface => (
                open_lock
                    .as_any()
                    .downcast_ref::<GenericInterfaceDeclaration>()?
                    .id(),
                false,
            ),
            DeclarationKind::GenericDelegate => (
                open_lock
                    .as_any()
                    .downcast_ref::<GenericDelegateDeclaration>()?
                    .id(),
                true,
            ),
            _ => return None,
        };

        let mut arg_sigs = Vec::with_capacity(args.len());
        for a in args {
            arg_sigs.push(wire_signature_for_name(&a)?);
        }
        // Both parameterized interfaces and delegates use the `pinterface(...)` form.
        let _ = is_delegate;
        return Some(format!(
            "pinterface({};{})",
            format_guid(&piid),
            arg_sigs.join(";")
        ));
    }

    // Non-generic named type.
    let decl = MetadataReader::find_by_name(name)?;
    let lock = decl.read();
    match lock.kind() {
        DeclarationKind::Interface => {
            let iface = lock.as_any().downcast_ref::<InterfaceDeclaration>()?;
            Some(format_guid(&iface.id()))
        }
        DeclarationKind::Class => {
            let class = lock.as_any().downcast_ref::<ClassDeclaration>()?;
            let default_iface = class.default_interface()?;
            Some(format!("rc({};{})", name, format_guid(&default_iface.id())))
        }
        DeclarationKind::Enum => {
            let enum_decl = lock.as_any().downcast_ref::<EnumDeclaration>()?;
            let underlying = enum_decl
                .metadata()
                .map(|m| Signature::to_wire_signature(m, &enum_decl.type_()))
                .unwrap_or_else(|| "i4".to_string());
            Some(format!("enum({};{})", name, underlying))
        }
        DeclarationKind::Struct => {
            let s = lock.as_any().downcast_ref::<StructDeclaration>()?;
            let mut field_sigs = Vec::with_capacity(s.fields().len());
            for field in s.fields().iter() {
                let m = field.base().metadata()?;
                let type_name = Signature::to_struct_field_name(m, &field.type_());
                field_sigs.push(wire_signature_for_name(&type_name)?);
            }
            Some(format!("struct({};{})", name, field_sigs.join(";")))
        }
        DeclarationKind::Delegate => {
            let del = lock.as_any().downcast_ref::<DelegateDeclaration>()?;
            Some(format!("delegate({})", format_guid(&del.id())))
        }
        _ => None,
    }
}

fn primitive_wire_signature(name: &str) -> Option<&'static str> {
    Some(match name {
        "Boolean" => "b1",
        "Char16" => "c2",
        "UInt8" => "u1",
        "Int8" => "i1",
        "UInt16" => "u2",
        "Int16" => "i2",
        "UInt32" => "u4",
        "Int32" => "i4",
        "UInt64" => "u8",
        "Int64" => "i8",
        "Single" => "f4",
        "Double" => "f8",
        "String" => "string",
        "Guid" => "g16",
        "Object" => "cinterface(IInspectable)",
        _ => return None,
    })
}

/// Splits a comma-separated type argument list, respecting nested `<…>` groups.
fn split_type_args(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in inner.chars() {
        match ch {
            '<' => {
                depth += 1;
                cur.push(ch);
            }
            '>' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                args.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        args.push(cur.trim().to_string());
    }
    args
}

/// Formats a GUID as the lowercase, braced string used in WinRT type signatures
/// (e.g. "{b5d036d7-e297-498f-ba60-0289e76e23dd}").
fn format_guid(g: &GUID) -> String {
    format!(
        "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7],
    )
}

/// SHA-1 hash a WinRT wire-format type signature with the cppwinrt namespace UUID
/// to produce a version-5 GUID — the canonical parameterized type instance IID.
fn compute_winrt_piid(signature: &str) -> GUID {
    use sha1::{Digest, Sha1};

    // cppwinrt's WinRT namespace UUID (big-endian byte layout).
    const NAMESPACE: [u8; 16] = [
        0x11, 0xf4, 0x7a, 0xd5, 0x7b, 0x73, 0x42, 0xc0, 0xab, 0xae, 0x87, 0x8b, 0x1e, 0x16, 0xad,
        0xee,
    ];

    let mut hasher = Sha1::new();
    hasher.update(NAMESPACE);
    hasher.update(signature.as_bytes());
    let digest = hasher.finalize();

    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    // Version 5 (name-based, SHA-1).
    b[6] = (b[6] & 0x0F) | 0x50;
    // Variant RFC 4122.
    b[8] = (b[8] & 0x3F) | 0x80;

    GUID {
        data1: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        data2: u16::from_be_bytes([b[4], b[5]]),
        data3: u16::from_be_bytes([b[6], b[7]]),
        data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_piid_iasyncoperationwithprogress_httpresponsemessage_httpprogress() {
        // From Windows SDK windows.web.http.h
        let expected = GUID::from_u128(0x5d144364_77d7_5eca_8b09_936a69446652);
        let actual = GenericInstanceIdBuilder::generate_id_from_name(
            "Windows.Foundation.IAsyncOperationWithProgress`2<Windows.Web.Http.HttpResponseMessage,Windows.Web.Http.HttpProgress>",
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn ireference_of_delegate_matches_the_os() {
        use windows::core::Interface;
        use windows::Foundation::DeferralCompletedHandler;
        let names = [
            HSTRING::from("Windows.Foundation.IReference`1"),
            HSTRING::from("Windows.Foundation.DeferralCompletedHandler"),
        ];
        let elements: Vec<PCWSTR> = names.iter().map(|n| PCWSTR(n.as_ptr())).collect();
        let locator = IRoMetaDataLocatorImpl;
        let locator = IRoMetaDataLocator::new(&locator);
        let mut expected = GUID::zeroed();
        unsafe {
            RoGetParameterizedTypeInstanceIID(&elements, &*locator, &mut expected, None).unwrap();
        }
        assert_eq!(
            GenericInstanceIdBuilder::ireference_of_delegate(&DeferralCompletedHandler::IID),
            expected,
        );
    }
}
