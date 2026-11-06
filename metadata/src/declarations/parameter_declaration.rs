use crate::declarations::declaration::{Declaration, DeclarationKind};
use crate::prelude::*;
use std::any::Any;
use windows::Win32::System::WinRT::Metadata::{
    mdtParamDef, CorTokenType, IMetaDataImport2, ELEMENT_TYPE_BYREF,
};

#[derive(Clone, Debug)]
pub struct ParameterDeclaration {
    kind: DeclarationKind,
    pub(crate) metadata: Option<IMetaDataImport2>,
    token: CorTokenType,
    parameter_type: PCCOR_SIGNATURE,
    full_name: String,
    attributes: u32,
}

impl ParameterDeclaration {
    pub fn new(
        metadata: Option<&IMetaDataImport2>,
        token: CorTokenType,
        sig_type: PCCOR_SIGNATURE,
    ) -> Self {
        //assert!(metadata.is_none());
        assert_eq!(type_from_token(token), mdtParamDef.0);
        assert_ne!(token.0, 0);

        let mut attributes = 0u32;
        let full_name = match metadata {
            None => String::new(),
            Some(metadata) => {
                let mut length = 0;
                let mut full_name_data = [0_u16; MAX_IDENTIFIER_LENGTH];

                let result = unsafe {
                    metadata.GetParamProps(
                        token.0 as u32,
                        0 as _,
                        0 as _,
                        Some(&mut full_name_data),
                        &mut length,
                        &mut attributes,
                        0 as _,
                        0 as _,
                        0 as _,
                    )
                };

                assert!(result.is_ok());
                String::from_utf16_lossy(&full_name_data[..length.saturating_sub(1) as usize])
            }
        };

        Self {
            kind: DeclarationKind::Parameter,
            metadata: metadata.map(|f| f.clone()),
            token,
            parameter_type: sig_type,
            full_name,
            attributes,
        }
    }

    /// Marked `[out]` without being ByRef: a caller-allocated array the callee fills.
    pub fn is_fill_array(&self) -> bool {
        const PD_OUT: u32 = 0x2;
        self.attributes & PD_OUT != 0 && !self.is_out()
    }

    pub fn is_out(&self) -> bool {
        let mut parameter_type = self.parameter_type.clone();
        cor_sig_uncompress_token(&mut parameter_type) == ELEMENT_TYPE_BYREF.0 as u32
    }

    pub fn token(&self) -> CorTokenType {
        self.token
    }

    pub fn type_(&self) -> PCCOR_SIGNATURE {
        self.parameter_type
    }

    pub fn metadata(&self) -> Option<&IMetaDataImport2> {
        self.metadata.as_ref()
    }
}

impl Declaration for ParameterDeclaration {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn name(&self) -> &str {
        self.full_name()
    }

    fn full_name(&self) -> &str {
        self.full_name.as_str()
    }

    fn kind(&self) -> DeclarationKind {
        self.kind
    }
}
