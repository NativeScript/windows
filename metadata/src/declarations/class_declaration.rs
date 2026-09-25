use crate::declaration_factory::DeclarationFactory;
use crate::declarations::base_class_declaration::{BaseClassDeclaration, BaseClassDeclarationImpl};
use crate::declarations::declaration::{Declaration, DeclarationKind};
use crate::declarations::event_declaration::EventDeclaration;
use crate::declarations::interface_declaration::InterfaceDeclaration;
use crate::declarations::method_declaration::MethodDeclaration;
use crate::declarations::property_declaration::PropertyDeclaration;
use crate::declarations::type_declaration::TypeDeclaration;
use std::any::Any;
use std::sync::OnceLock;
use windows::Win32::System::WinRT::Metadata::{
    CorTokenType, IMetaDataImport2, COR_CTOR_METHOD_NAME_W,
};

use crate::prelude::*;

const DEFAULT_ATTRIBUTE: &str = "Windows.Foundation.Metadata.DefaultAttribute";

#[derive(Clone)]
pub struct ClassDeclaration {
    initializers: OnceLock<Vec<MethodDeclaration>>,
    default_interface: OnceLock<Option<Box<dyn BaseClassDeclarationImpl>>>,
    sealed: OnceLock<bool>,
    base: BaseClassDeclaration,
    base_full_name: String,
}

impl ClassDeclaration {
    pub fn make_initializer_declarations(
        metadata: Option<&IMetaDataImport2>,
        token: CorTokenType,
    ) -> Vec<MethodDeclaration> {
        let mut ret = Vec::new();

        if let Some(metadata) = metadata {
            let tokens = BaseClassDeclaration::collect_enum_tokens(
                metadata,
                |enumerator, buffer, buffer_len, count| unsafe {
                    metadata.EnumMembersWithName(
                        enumerator,
                        token.0 as u32,
                        COR_CTOR_METHOD_NAME_W,
                        buffer,
                        buffer_len,
                        count,
                    )
                },
            );

            for method_token in tokens {
                // TODO: Make a InstanceInitializerDeclaration and check this in it's isExported method
                let mut flags = 0;
                let result_inner = unsafe {
                    metadata.GetMethodProps(
                        method_token,
                        0 as _,
                        None,
                        0 as _,
                        &mut flags,
                        0 as _,
                        0 as _,
                        0 as _,
                        0 as _,
                    )
                };
                debug_assert!(result_inner.is_ok());

                if !is_md_public(flags as i32) {
                    continue;
                }

                ret.push(MethodDeclaration::new(
                    Some(&metadata),
                    CorTokenType(method_token as i32),
                ));
            }
        }
        ret
    }

    pub fn make_default_interface(
        metadata: Option<&IMetaDataImport2>,
        token: CorTokenType,
    ) -> Option<Box<dyn BaseClassDeclarationImpl>> {
        match metadata {
            None => {}
            Some(metadata) => {
                let interface_impl_tokens = BaseClassDeclaration::collect_enum_tokens(
                    metadata,
                    |enumerator, buffer, buffer_len, count| unsafe {
                        metadata.EnumInterfaceImpls(
                            enumerator,
                            token.0 as u32,
                            buffer,
                            buffer_len,
                            count,
                        )
                    },
                );

                for interface_impl_token in interface_impl_tokens {
                    if has_custom_attribute(
                        metadata,
                        CorTokenType(interface_impl_token as i32),
                        DEFAULT_ATTRIBUTE,
                    ) {
                        let mut interface_token = 0_u32;
                        let result = unsafe {
                            metadata.GetInterfaceImplProps(
                                interface_impl_token,
                                0 as _,
                                &mut interface_token,
                            )
                        };
                        debug_assert!(result.is_ok());
                        return DeclarationFactory::make_interface_declaration(
                            Some(metadata),
                            CorTokenType(interface_token as i32),
                        );
                    }
                }
            }
        }

        None
    }

    pub fn new(metadata: Option<&IMetaDataImport2>, token: CorTokenType) -> Self {
        let mut base_full_name = String::new();

        if let Some(metadata) = metadata {
            let mut parent_token = 0_u32;

            let result = unsafe {
                metadata.GetTypeDefProps(token.0 as u32, None, 0 as _, 0 as _, &mut parent_token)
            };
            debug_assert!(result.is_ok());
            base_full_name = get_type_name(metadata, CorTokenType(parent_token as i32));
        }

        Self {
            initializers: OnceLock::new(),
            default_interface: OnceLock::new(),
            sealed: OnceLock::new(),
            base: BaseClassDeclaration::new(DeclarationKind::Class, metadata.clone(), token),
            base_full_name,
        }
    }

    pub fn base_full_name(&self) -> &str {
        self.base_full_name.as_str()
    }

    pub fn default_interface(&self) -> Option<&InterfaceDeclaration> {
        self.default_interface
            .get_or_init(|| {
                ClassDeclaration::make_default_interface(
                    self.base.base().metadata(),
                    self.base.base().token(),
                )
            })
            .as_ref()
            .map(|f| {
                let f = f.as_declaration();
                let declaration = f;
                declaration.as_any().downcast_ref::<InterfaceDeclaration>()
            })
            .flatten()
    }

    /// Full name of the default interface, including generic instances (e.g. StringMap's
    /// `Windows.Foundation.Collections.IMap`2<String, String>`) which `default_interface()`
    /// cannot return (its downcast only admits plain `InterfaceDeclaration`s).
    pub fn default_interface_full_name(&self) -> Option<&str> {
        self.default_interface
            .get_or_init(|| {
                ClassDeclaration::make_default_interface(
                    self.base.base().metadata(),
                    self.base.base().token(),
                )
            })
            .as_ref()
            .map(|f| f.as_declaration().full_name())
    }

    pub fn is_instantiable(&self) -> bool {
        !self.initializers().is_empty()
    }

    /// Whether the class is sealed. The type-def flags are immutable, so the metadata call
    /// runs once per declaration; the runtime asks on every construct and wrap.
    pub fn is_sealed(&self) -> bool {
        *self.sealed.get_or_init(|| {
            let mut flags = 0_u32;
            if let Some(metadata) = self.base.base().metadata() {
                let result = unsafe {
                    metadata.GetTypeDefProps(
                        self.base.base().token().0 as u32,
                        None,
                        0 as _,
                        &mut flags,
                        0 as _,
                    )
                };
                debug_assert!(result.is_ok());
            }
            is_td_sealed(flags as i32)
        })
    }

    pub fn initializers(&self) -> &[MethodDeclaration] {
        self.initializers.get_or_init(|| {
            ClassDeclaration::make_initializer_declarations(
                self.base.base().metadata(),
                self.base.base().token(),
            )
        })
    }
}

impl Declaration for ClassDeclaration {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn name(&self) -> &str {
        self.base.name()
    }

    fn full_name(&self) -> &str {
        self.base.full_name()
    }

    fn kind(&self) -> DeclarationKind {
        self.base.kind()
    }
}

impl BaseClassDeclarationImpl for ClassDeclaration {
    fn as_declaration(&self) -> &dyn Declaration {
        self
    }

    fn as_declaration_mut(&mut self) -> &mut dyn Declaration {
        self
    }

    fn base(&self) -> &TypeDeclaration {
        self.base.base()
    }

    fn implemented_interfaces(&self) -> Vec<&InterfaceDeclaration> {
        self.base.implemented_interfaces()
    }

    fn methods(&self) -> &[MethodDeclaration] {
        self.base.methods()
    }

    fn properties(&self) -> &[PropertyDeclaration] {
        self.base.properties()
    }

    fn events(&self) -> &[EventDeclaration] {
        self.base.events()
    }
}
