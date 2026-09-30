use crate::declarations::class_declaration::ClassDeclaration;
use crate::declarations::declaration::{Declaration, DeclarationKind};
use crate::declarations::delegate_declaration::generic_delegate_declaration::GenericDelegateDeclaration;
use crate::declarations::delegate_declaration::DelegateDeclaration;
use crate::declarations::enum_declaration::EnumDeclaration;
use crate::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
use crate::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use crate::declarations::interface_declaration::InterfaceDeclaration;
use crate::declarations::namespace_declaration::NamespaceDeclaration;
use crate::declarations::struct_declaration::StructDeclaration;
use crate::prelude::*;
use ahash::{AHashMap, AHashSet};
use parking_lot::RwLock;
use std::cell::RefCell;
use std::ffi::OsString;
use std::mem::MaybeUninit;
use std::os::windows::prelude::OsStringExt;
use std::sync::{Arc, OnceLock};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::RO_E_METADATA_NAME_IS_NAMESPACE;
use windows::Win32::System::WinRT::Metadata::{
    mdtTypeDef, mdtTypeRef, ofRead, CorTokenType, IMetaDataDispenserEx, IMetaDataImport2,
    MetaDataGetDispenser, RoGetMetaDataFile, CLSID_CorMetaDataDispenser,
};

// Thread-local cache for resolved declarations.  V8 (and therefore all metadata
// lookups) runs on a single thread, so a thread_local avoids the Send+Sync
// requirements that a global static cache would impose on `dyn Declaration`.
thread_local! {
    static DECLARATION_CACHE: RefCell<AHashMap<String, Arc<RwLock<dyn Declaration>>>> =
        RefCell::new(AHashMap::new());
    static DECLARATION_MISS_CACHE: RefCell<AHashSet<String>> =
        RefCell::new(AHashSet::new());
    // Sideloaded app-local .winmd scopes (e.g. Microsoft.Web.WebView2.Core.winmd).
    // RoGetMetaDataFile only resolves system/app-package metadata, so third-party
    // WinRT components are consulted here as a fallback. Each entry keeps the
    // opened import scope plus its typedef names (for namespace synthesis).
    static SIDELOADED_SCOPES: RefCell<Vec<SideloadedScope>> = RefCell::new(Vec::new());
    static SIDELOADED_PATHS: RefCell<AHashSet<String>> = RefCell::new(AHashSet::new());
    // What `FILTER` said about each type JS asked for, so each is matched once.
    static FILTER_VERDICTS: RefCell<AHashMap<String, bool>> = RefCell::new(AHashMap::new());
}

static FILTER: OnceLock<metadata_filter::MetadataFilter> = OnceLock::new();

struct SideloadedScope {
    import: IMetaDataImport2,
    type_names: Vec<String>,
}

#[derive(Debug)]
pub struct MetadataReader {}

impl MetadataReader {
    pub fn find_by_name_w(full_name: PCWSTR) -> Option<Arc<RwLock<dyn Declaration>>> {
        let name = OsString::from_wide(unsafe { full_name.as_wide() });
        let name = name.to_string_lossy();
        MetadataReader::find_by_name(name.as_ref())
    }
    /// Look up a type by full name, falling back to generic arities 1-4 when
    /// the plain name doesn't exist.  Allows JS code to reference generic
    /// delegates/interfaces without the CLR backtick-arity suffix:
    /// `Windows.Foundation.EventHandler` → `Windows.Foundation.EventHandler`1`.
    pub fn find_by_name_or_generic(full_name: &str) -> Option<Arc<RwLock<dyn Declaration>>> {
        if let Some(d) = MetadataReader::find_by_name(full_name) {
            return Some(d);
        }
        for arity in 1u8..=4 {
            let candidate = format!("{}`{}", full_name, arity);
            if let Some(d) = MetadataReader::find_by_name(&candidate) {
                return Some(d);
            }
        }
        None
    }

    /// The app's native API filter (`whitelist.mdg` / `blacklist.mdg`), set once at startup.
    pub fn set_filter(filter: metadata_filter::MetadataFilter) {
        let _ = FILTER.set(filter);
    }

    /// Loads the filter from the first of `dirs` holding `whitelist.mdg` or `blacklist.mdg` (the build
    /// copies them next to the exe).
    pub fn load_filter(dirs: impl IntoIterator<Item = std::path::PathBuf>) {
        use metadata_filter::{MetadataFilter, BLACKLIST_FILE, WHITELIST_FILE};
        let Some(dir) = dirs
            .into_iter()
            .find(|dir| [WHITELIST_FILE, BLACKLIST_FILE].iter().any(|file| dir.join(file).is_file()))
        else {
            return;
        };
        match MetadataFilter::load(&dir) {
            Ok(filter) => MetadataReader::set_filter(filter),
            Err(error) => eprintln!(
                "[NativeScript] metadata filter in {} not loaded: {error}",
                dir.display()
            ),
        }
    }

    /// Whether JS may reach `declaration` by name. A type the app's filter leaves out isn't there,
    /// as on Android and iOS where it's left out of the metadata; namespaces always are, so JS can
    /// still reach the types it may use. The runtime's own lookups (return types, bases,
    /// interfaces, an instance's runtime class) are not filtered.
    pub fn visible_to_js(declaration: &Arc<RwLock<dyn Declaration>>) -> bool {
        let Some(filter) = FILTER.get().filter(|filter| !filter.is_empty()) else {
            return true;
        };
        let lock = declaration.read();
        if lock.kind() == DeclarationKind::Namespace {
            return true;
        }
        let full_name = lock.full_name();
        if let Some(visible) = FILTER_VERDICTS.with(|verdicts| verdicts.borrow().get(full_name).copied()) {
            return visible;
        }
        let verdict = filter.check(full_name);
        let visible = verdict == metadata_filter::Verdict::Allowed;
        if !visible {
            eprintln!("[NativeScript] metadata filter: {full_name} is unavailable ({verdict})");
        }
        FILTER_VERDICTS.with(|verdicts| verdicts.borrow_mut().insert(full_name.to_string(), visible));
        visible
    }

    pub fn find_by_name(full_name: &str) -> Option<Arc<RwLock<dyn Declaration>>> {
        let cached = DECLARATION_CACHE.with(|cache| cache.borrow().get(full_name).map(Arc::clone));
        if let Some(arc) = cached {
            return Some(arc);
        }

        let known_miss = DECLARATION_MISS_CACHE.with(|cache| cache.borrow().contains(full_name));
        if known_miss {
            return None;
        }

        let Some(declaration) = MetadataReader::find_by_name_uncached(full_name) else {
            DECLARATION_MISS_CACHE.with(|cache| {
                cache.borrow_mut().insert(full_name.to_string());
            });
            return None;
        };

        DECLARATION_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert(full_name.to_string(), Arc::clone(&declaration));
        });
        Some(declaration)
    }

    fn find_by_name_uncached(full_name: &str) -> Option<Arc<RwLock<dyn Declaration>>> {
        if full_name.is_empty() {
            return Some(Arc::new(RwLock::new(NamespaceDeclaration::new(""))));
        }

    
        if let Some(mapped) = projected_value_type_alias(full_name) {
            return MetadataReader::find_by_name(mapped);
        }

        // Closed generic instance (e.g. "IAsyncOperation`1<Windows.Foundation.Uri>").
        // Look up the open generic, then wrap it as a GenericInterfaceInstanceDeclaration
        // so that the correct parameterized COM IID is used for QueryInterface.
        if let Some(angle_pos) = full_name.find('<') {
            let open_name = &full_name[..angle_pos];
            let mut open_metadata: MaybeUninit<IMetaDataImport2> = MaybeUninit::zeroed();
            let mut open_token = 0_u32;
            let open_name_hstring = HSTRING::from(open_name);
            let open_dispenser: MaybeUninit<IMetaDataDispenserEx> = MaybeUninit::zeroed();
            let open_result = unsafe {
                RoGetMetaDataFile(
                    &open_name_hstring,
                    open_dispenser.assume_init_ref(),
                    None,
                    Some(open_metadata.as_mut_ptr() as *mut Option<IMetaDataImport2>),
                    Some(&mut open_token),
                )
            };
            let resolved = if open_result.is_ok() {
                Some((unsafe { open_metadata.assume_init() }, open_token))
            } else {
                Self::find_typedef_sideloaded(open_name)
            };
            if let Some((open_metadata, open_token)) = resolved {
                let open_token = CorTokenType(open_token as i32);
                let mut flags = 0u32;
                let mut _parent = 0u32;
                let props_ok = unsafe {
                    open_metadata.GetTypeDefProps(
                        open_token.0 as u32,
                        None,
                        0 as _,
                        &mut flags,
                        &mut _parent,
                    )
                }
                .is_ok();
                if props_ok && !is_td_class(flags as i32) {
                    let declaration = GenericInterfaceInstanceDeclaration::new_from_names(
                        Some(&open_metadata),
                        open_token,
                        full_name.to_string(),
                        full_name.to_string(),
                    );
                    return Some(Arc::new(RwLock::new(declaration)));
                }
            }
            return None;
        }

        let mut metadata: MaybeUninit<IMetaDataImport2> = MaybeUninit::zeroed();
        let mut token = 0_u32;
        let full_name_hstring = HSTRING::from(full_name);

        let dispenser: MaybeUninit<IMetaDataDispenserEx> = MaybeUninit::zeroed();

        let result = unsafe {
            RoGetMetaDataFile(
                &full_name_hstring,
                dispenser.assume_init_ref(),
                None,
                Some(metadata.as_mut_ptr() as *mut Option<IMetaDataImport2>),
                Some(&mut token), /* std::option::Option<*mut u32> */
            )
        };

        if let Err(error) = result {
            if error.code() == RO_E_METADATA_NAME_IS_NAMESPACE {
                return Some(Arc::new(RwLock::new(NamespaceDeclaration::new(full_name))));
            }
            // Sideloaded app-local winmd fallback (third-party WinRT components,
            // e.g. Microsoft.Web.WebView2.Core, which RoGetMetaDataFile can't see).
            if let Some((metadata, token)) = Self::find_typedef_sideloaded(full_name) {
                return Self::declaration_from_typedef(&metadata, token, full_name);
            }
            // Synthesize intermediate namespaces (e.g. "Microsoft.Web") so JS
            // dotted traversal can reach sideloaded types.
            if Self::sideloaded_namespace_exists(full_name) {
                return Some(Arc::new(RwLock::new(NamespaceDeclaration::new(full_name))));
            }
            return None;
        }

        let metadata = unsafe { metadata.assume_init() };
        Self::declaration_from_typedef(&metadata, token, full_name)
    }

    /// Classifies a resolved typedef (flags + System.* parent) into the matching
    /// Declaration. Shared by the RoGetMetaDataFile and sideloaded-winmd paths.
    fn declaration_from_typedef(
        metadata: &IMetaDataImport2,
        token: u32,
        full_name: &str,
    ) -> Option<Arc<RwLock<dyn Declaration>>> {
        let mut flags = 0;
        let mut parent_token = 0;

        {
            let result = unsafe {
                metadata.GetTypeDefProps(token, None, 0 as _, &mut flags, &mut parent_token)
            };
            assert!(result.is_ok());
        }

        if is_td_class(flags as i32) {
            let mut parent_name = [0_u16; MAX_IDENTIFIER_LENGTH];
            let pt = CorTokenType(parent_token as i32);
            let tt = type_from_token(pt);
            let mut size = 0_u32;
            match CorTokenType(tt) {
                mdtTypeDef => {
                    let result = unsafe {
                        metadata.GetTypeDefProps(
                            parent_token,
                            Some(&mut parent_name),
                            &mut size,
                            0 as _,
                            0 as _,
                        )
                    };

                    assert!(result.is_ok());
                }
                mdtTypeRef => {
                    let result = unsafe {
                        metadata.GetTypeRefProps(
                            parent_token,
                            0 as _,
                            Some(&mut parent_name),
                            &mut size,
                        )
                    };
                    assert!(result.is_ok());
                }
                _ => {
                    // Unexpected parent token type — not a known WinRT declaration.
                    return None;
                }
            }

            let parent_name_buf = &parent_name[0..size.saturating_sub(1) as usize];
            let parent_name_string = String::from_utf16_lossy(parent_name_buf);

            if parent_name_string == SYSTEM_ENUM {
                return Some(Arc::new(RwLock::new(EnumDeclaration::new(
                    Some(metadata),
                    CorTokenType(token as i32),
                ))));
            } else if parent_name_string == SYSTEM_VALUETYPE {
                return Some(Arc::new(RwLock::new(StructDeclaration::new(
                    Some(metadata),
                    CorTokenType(token as i32),
                ))));
            } else if parent_name_string == SYSTEM_MULTICASTDELEGATE {
                return if full_name.contains("`") {
                    Some(Arc::new(RwLock::new(GenericDelegateDeclaration::new(
                        Some(metadata),
                        CorTokenType(token as i32),
                    ))))
                } else {
                    Some(Arc::new(RwLock::new(DelegateDeclaration::new(
                        Some(metadata),
                        CorTokenType(token as i32),
                    ))))
                };
            }

            return Some(Arc::new(RwLock::new(ClassDeclaration::new(
                Some(metadata),
                CorTokenType(token as i32),
            ))));
        }

        if is_td_interface(flags as i32) {
            return if full_name.contains("`") {
                Some(Arc::new(RwLock::new(GenericInterfaceDeclaration::new(
                    Some(metadata),
                    CorTokenType(token as i32),
                ))))
            } else {
                Some(Arc::new(RwLock::new(InterfaceDeclaration::new(
                    Some(metadata),
                    CorTokenType(token as i32),
                ))))
            };
        }

        // The token is neither a class-family type nor an interface — not a
        // recognised WinRT declaration.  Return None rather than panicking so
        // callers can handle unknown types gracefully.
        None
    } // end declaration_from_typedef

    /// Opens an app-local .winmd file and adds it to the sideloaded scopes that
    /// `find_by_name` consults when `RoGetMetaDataFile` can't resolve a name
    /// (third-party WinRT components, e.g. Microsoft.Web.WebView2.Core.winmd).
    /// Idempotent per canonical path. Clears the miss cache so names that failed
    /// to resolve before registration succeed afterwards.
    ///
    /// Scopes are per-thread (like the declaration cache): each Runtime thread
    /// registers its own set, which `Runtime::new`'s auto-scan does automatically.
    pub fn register_winmd_file(path: &str) -> Result<(), String> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| format!("winmd not found '{}': {}", path, e))?
            .to_string_lossy()
            .to_string();
        let already_registered =
            SIDELOADED_PATHS.with(|p| !p.borrow_mut().insert(canonical.clone()));
        if already_registered {
            return Ok(());
        }

        let mut dispenser_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        unsafe {
            MetaDataGetDispenser(
                &CLSID_CorMetaDataDispenser,
                &IMetaDataDispenserEx::IID,
                &mut dispenser_ptr,
            )
        }
        .map_err(|e| format!("MetaDataGetDispenser failed: {}", e))?;
        let dispenser = unsafe { IMetaDataDispenserEx::from_raw(dispenser_ptr) };

        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let unknown = unsafe {
            dispenser.OpenScope(
                PCWSTR(wide.as_ptr()),
                ofRead.0 as u32,
                &IMetaDataImport2::IID,
            )
        }
        .map_err(|e| format!("OpenScope failed for '{}': {}", path, e))?;
        let import = unknown
            .cast::<IMetaDataImport2>()
            .map_err(|e| format!("IMetaDataImport2 cast failed for '{}': {}", path, e))?;

        let type_names = enumerate_typedef_names(&import);

        SIDELOADED_SCOPES.with(|scopes| {
            scopes.borrow_mut().push(SideloadedScope {
                import,
                type_names,
            });
        });
        DECLARATION_MISS_CACHE.with(|cache| cache.borrow_mut().clear());
        Ok(())
    }

    /// Looks a full type name up in the sideloaded scopes only.
    fn find_typedef_sideloaded(full_name: &str) -> Option<(IMetaDataImport2, u32)> {
        const MD_TYPEDEF_NIL: u32 = 0x0200_0000;
        let wide: Vec<u16> = full_name.encode_utf16().chain(std::iter::once(0)).collect();
        SIDELOADED_SCOPES.with(|scopes| {
            for scope in scopes.borrow().iter() {
                let mut token = 0u32;
                let found = unsafe {
                    scope
                        .import
                        .FindTypeDefByName(PCWSTR(wide.as_ptr()), 0, &mut token)
                }
                .is_ok();
                if found && token != 0 && token != MD_TYPEDEF_NIL {
                    return Some((scope.import.clone(), token));
                }
            }
            None
        })
    }

    /// True when `prefix` is a namespace segment of any sideloaded type
    /// (e.g. "Microsoft.Web" for Microsoft.Web.WebView2.Core.CoreWebView2).
    fn sideloaded_namespace_exists(prefix: &str) -> bool {
        let with_dot = format!("{}.", prefix);
        SIDELOADED_SCOPES.with(|scopes| {
            scopes
                .borrow()
                .iter()
                .any(|s| s.type_names.iter().any(|n| n.starts_with(&with_dot)))
        })
    }
}

fn projected_value_type_alias(full_name: &str) -> Option<&'static str> {
    Some(match full_name {
        "System.TimeSpan" => "Windows.Foundation.TimeSpan",
        "System.DateTimeOffset" => "Windows.Foundation.DateTime",
        "System.Numerics.Vector2" => "Windows.Foundation.Numerics.Vector2",
        "System.Numerics.Vector3" => "Windows.Foundation.Numerics.Vector3",
        "System.Numerics.Vector4" => "Windows.Foundation.Numerics.Vector4",
        "System.Numerics.Matrix3x2" => "Windows.Foundation.Numerics.Matrix3x2",
        "System.Numerics.Matrix4x4" => "Windows.Foundation.Numerics.Matrix4x4",
        "System.Numerics.Quaternion" => "Windows.Foundation.Numerics.Quaternion",
        "System.Numerics.Plane" => "Windows.Foundation.Numerics.Plane",
        _ => return None,
    })
}

#[cfg(test)]
mod sideload_tests {
    use super::*;
    use crate::declarations::declaration::DeclarationKind;

    // Present on every Windows installation; opened via the dispenser exactly
    // like a third-party winmd would be.
    const FIXTURE: &str = "C:\\Windows\\System32\\WinMetadata\\Windows.Globalization.winmd";

    #[test]
    fn register_winmd_rejects_missing_file() {
        assert!(MetadataReader::register_winmd_file("C:\\does\\not\\exist.winmd").is_err());
    }

    #[test]
    fn register_winmd_loads_and_resolves_typedefs() {
        MetadataReader::register_winmd_file(FIXTURE).expect("register fixture winmd");
        // Idempotent re-register.
        MetadataReader::register_winmd_file(FIXTURE).expect("re-register fixture winmd");

        let (import, token) =
            MetadataReader::find_typedef_sideloaded("Windows.Globalization.Calendar")
                .expect("Calendar typedef in sideloaded scope");
        let decl = MetadataReader::declaration_from_typedef(
            &import,
            token,
            "Windows.Globalization.Calendar",
        )
        .expect("declaration built from sideloaded typedef");
        assert_eq!(decl.read().kind(), DeclarationKind::Class);

        assert!(
            MetadataReader::find_typedef_sideloaded("Windows.Globalization.NoSuchType").is_none()
        );
    }

    #[test]
    fn sideloaded_namespaces_synthesize() {
        MetadataReader::register_winmd_file(FIXTURE).expect("register fixture winmd");
        assert!(MetadataReader::sideloaded_namespace_exists(
            "Windows.Globalization"
        ));
        assert!(!MetadataReader::sideloaded_namespace_exists(
            "Windows.Bogus"
        ));
    }

    #[test]
    fn projected_value_type_alias_maps_known_and_skips_base_types() {
        assert_eq!(
            projected_value_type_alias("System.TimeSpan"),
            Some("Windows.Foundation.TimeSpan")
        );
        assert_eq!(
            projected_value_type_alias("System.DateTimeOffset"),
            Some("Windows.Foundation.DateTime")
        );
        assert_eq!(
            projected_value_type_alias("System.Numerics.Vector3"),
            Some("Windows.Foundation.Numerics.Vector3")
        );
        // BCL base types must keep resolving as themselves, not be remapped.
        assert_eq!(projected_value_type_alias("System.ValueType"), None);
        assert_eq!(projected_value_type_alias("System.Enum"), None);
        assert_eq!(projected_value_type_alias("Windows.Foundation.TimeSpan"), None);
    }

    #[test]
    fn projected_value_type_resolves_to_winrt_struct() {
        // The projected `System.TimeSpan` name (as WinUI's `Duration` struct encodes
        // its field) must resolve to the real `Windows.Foundation.TimeSpan` struct so
        // nested value-struct marshaling can recurse into it.
        let decl = MetadataReader::find_by_name("System.TimeSpan")
            .expect("System.TimeSpan resolves via projected alias");
        let lock = decl.read();
        assert_eq!(lock.kind(), DeclarationKind::Struct);
        assert_eq!(lock.full_name(), "Windows.Foundation.TimeSpan");
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use metadata_filter::{MetadataFilter, PatternList};

    // The only test that sets the process-wide filter; the others never ask `visible_to_js`.
    #[test]
    fn filtered_types_are_hidden_from_js_but_resolve_for_the_runtime() {
        MetadataReader::set_filter(MetadataFilter::new(
            None,
            PatternList::parse("Windows.Storage.Pickers:FileOpen*"),
        ));
        let picker = MetadataReader::find_by_name("Windows.Storage.Pickers.FileOpenPicker")
            .expect("the runtime still resolves a filtered type");
        assert!(!MetadataReader::visible_to_js(&picker));
        // Twice: the second answer comes from the verdict cache.
        assert!(!MetadataReader::visible_to_js(&picker));
        let save = MetadataReader::find_by_name("Windows.Storage.Pickers.FileSavePicker").unwrap();
        assert!(MetadataReader::visible_to_js(&save));
        let namespace = MetadataReader::find_by_name("Windows.Storage.Pickers").unwrap();
        assert!(MetadataReader::visible_to_js(&namespace));
        let handler = MetadataReader::find_by_name_or_generic("Windows.Foundation.EventHandler").unwrap();
        assert!(MetadataReader::visible_to_js(&handler));
    }
}

/// Collects every typedef's full name from an opened metadata scope; used to
/// synthesize namespace declarations for dotted traversal of sideloaded types.
fn enumerate_typedef_names(import: &IMetaDataImport2) -> Vec<String> {
    let mut names = Vec::new();
    let mut henum: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut tokens = [0u32; 64];
    loop {
        let mut count = 0u32;
        let result = unsafe {
            import.EnumTypeDefs(&mut henum, tokens.as_mut_ptr(), tokens.len() as u32, &mut count)
        };
        if result.is_err() || count == 0 {
            break;
        }
        for &td in &tokens[..count as usize] {
            let mut name_buf = [0u16; MAX_IDENTIFIER_LENGTH];
            let mut size = 0u32;
            let ok = unsafe {
                import.GetTypeDefProps(td, Some(&mut name_buf), &mut size, 0 as _, 0 as _)
            }
            .is_ok();
            if ok && size > 1 {
                names.push(String::from_utf16_lossy(
                    &name_buf[..size.saturating_sub(1) as usize],
                ));
            }
        }
    }
    if !henum.is_null() {
        unsafe { import.CloseEnum(henum) };
    }
    names
}
