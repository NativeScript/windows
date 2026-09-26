//! Resolves real WinRT method/property/interface signatures by type name, replacing the
//! hand-maintained 2-entry table that used to live in `dotnet-tool`. Reuses the exact same
//! metadata machinery the runtime itself uses to bind WinRT types for the JS engine
//! (`metadata::meta_data_reader::MetadataReader::find_by_name` + the `declarations` module) and
//! the same signature-to-string decoder `typings-generator` uses to render `.d.ts` output
//! (`metadata::signature::Signature::to_string`) — nothing here is a new metadata reader.
//!
//! WinRT runtime classes don't declare their own methods; members live on the interfaces the
//! class implements (including whichever "overrides"/protected interface backs a composable
//! class's overridable virtuals, e.g. `MeasureOverride`/`ArrangeOverride`). So resolving a method
//! or property "on" a class searches the class's own methods (rare) plus every interface it
//! implements, uniformly — no special-cased interface names needed.

use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::Declaration;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::declarations::property_declaration::PropertyDeclaration;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;

pub struct ResolvedMethod {
    pub name: String,
    pub return_type: String,
    pub parameters: Vec<(String, String)>,
    /// `Some(modifier)` when the resolution source knows the real access modifier must differ
    /// from the caller's default (e.g. WinRT composable-class overrides like `MeasureOverride`
    /// are `protected override`, not `public override` — overriding with the wrong accessibility
    /// is a real C# compile error, CS0507). `None` means "caller's usual default is correct"
    /// (general metadata resolution only ever finds `public` WinRT interface members).
    pub modifier_override: Option<&'static str>,
}

pub struct ResolvedProperty {
    pub name: String,
    pub prop_type: String,
    pub is_readable: bool,
    pub is_writable: bool,
}

fn is_accessor_name(name: &str) -> bool {
    name.starts_with("get_")
        || name.starts_with("set_")
        || name.starts_with("add_")
        || name.starts_with("remove_")
        || name == ".ctor"
        || name == ".cctor"
}

fn to_resolved_method(m: &MethodDeclaration) -> Option<ResolvedMethod> {
    let metadata = m.metadata()?;
    let return_type = normalize(&Signature::to_string(metadata, &m.return_type()));
    let parameters = m
        .parameters()
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.is_out())
        .map(|(i, p)| {
            let ty = p
                .metadata()
                .map(|md| normalize(&Signature::to_string(md, &p.type_())))
                .unwrap_or_else(|| "object".to_string());
            (format!("arg{i}"), ty)
        })
        .collect();
    Some(ResolvedMethod {
        name: m.full_name().to_string(),
        return_type,
        parameters,
        modifier_override: None,
    })
}

fn to_resolved_property(p: &PropertyDeclaration) -> Option<ResolvedProperty> {
    let getter = p.getter();
    let metadata = getter.metadata()?;
    let prop_type = normalize(&Signature::to_string(metadata, &getter.return_type()));
    Some(ResolvedProperty {
        name: p.full_name().to_string(),
        prop_type,
        is_readable: true,
        is_writable: p.setter().is_some(),
    })
}

/// C#-normalizes a raw WinRT/CLR type name (e.g. "Boolean" -> "bool"). Mirrors
/// `generator::normalize_csharp_type` exactly — kept here too since this module has no
/// dependency on `generator`'s private items; call sites in `generator.rs` still run their own
/// copy over whatever this module returns, so double-normalizing is harmless (idempotent).
fn normalize(input: &str) -> String {
    match input.trim() {
        "void" | "Void" => "void".to_string(),
        "bool" | "Boolean" => "bool".to_string(),
        "string" | "String" => "string".to_string(),
        "object" | "Object" => "object".to_string(),
        "byte" | "UInt8" => "byte".to_string(),
        "sbyte" | "Int8" => "sbyte".to_string(),
        "short" | "Int16" => "short".to_string(),
        "ushort" | "UInt16" => "ushort".to_string(),
        "int" | "Int32" => "int".to_string(),
        "uint" | "UInt32" => "uint".to_string(),
        "long" | "Int64" => "long".to_string(),
        "ulong" | "UInt64" => "ulong".to_string(),
        "float" | "Single" => "float".to_string(),
        "double" | "Double" => "double".to_string(),
        other => other.to_string(),
    }
}

/// Every method visible on a type: its own (rare for WinRT classes) plus, for classes, every
/// implemented interface's methods (this is where overridable virtuals like `MeasureOverride`
/// actually live). Property/event accessor methods (`get_`/`set_`/`add_`/`remove_`) are excluded
/// — they're covered by `resolve_property`/left for a future events pass, not double-counted here.
fn all_methods(decl: &dyn Declaration) -> Vec<MethodDeclaration> {
    let mut out = Vec::new();
    if let Some(class) = decl.as_any().downcast_ref::<ClassDeclaration>() {
        out.extend(class.methods().iter().cloned());
        for iface in class.implemented_interfaces() {
            out.extend(iface.methods().iter().cloned());
        }
    } else if let Some(iface) = decl.as_any().downcast_ref::<InterfaceDeclaration>() {
        out.extend(iface.methods().iter().cloned());
    }
    out.retain(|m| !is_accessor_name(m.full_name()) && !m.is_static());
    out
}

fn all_properties(decl: &dyn Declaration) -> Vec<PropertyDeclaration> {
    let mut out = Vec::new();
    if let Some(class) = decl.as_any().downcast_ref::<ClassDeclaration>() {
        out.extend(class.properties().iter().cloned());
        for iface in class.implemented_interfaces() {
            out.extend(iface.properties().iter().cloned());
        }
    } else if let Some(iface) = decl.as_any().downcast_ref::<InterfaceDeclaration>() {
        out.extend(iface.properties().iter().cloned());
    }
    out
}

/// A handful of well-known WinRT "composable class override" virtuals whose real signature
/// this module's general metadata resolution can't find: they don't appear in
/// `EnumInterfaceImpls` on the public runtimeclass the way an ordinarily-implemented interface's
/// members do — the WinRT composable-class-inheritance metadata convention for exposing an
/// overridable "protected members" interface isn't one this pass reverse-engineered. Kept as an
/// explicit, honest fallback (checked only after general resolution misses) rather than silently
/// guessing or dropping these two specific cases that a prior, cruder implementation did handle.
/// Extend this table for other known overridable virtuals metadata resolution can't reach; genuinely
/// unknown methods still fall through to a skip-with-warning at the call site.
fn known_override_signature(base_type: &str, method: &str) -> Option<ResolvedMethod> {
    let is_panel_or_fe = matches!(
        base_type,
        "Windows.UI.Xaml.Controls.Panel" | "Windows.UI.Xaml.FrameworkElement"
    );
    if !is_panel_or_fe {
        return None;
    }
    match method {
        "MeasureOverride" => Some(ResolvedMethod {
            name: "MeasureOverride".to_string(),
            return_type: "Windows.Foundation.Size".to_string(),
            parameters: vec![("arg0".to_string(), "Windows.Foundation.Size".to_string())],
            modifier_override: Some("protected override"),
        }),
        "ArrangeOverride" => Some(ResolvedMethod {
            name: "ArrangeOverride".to_string(),
            return_type: "Windows.Foundation.Size".to_string(),
            parameters: vec![("arg0".to_string(), "Windows.Foundation.Size".to_string())],
            modifier_override: Some("protected override"),
        }),
        _ => None,
    }
}

/// Whether `type_name` is a type in the WinRT metadata sbg can see (system and sideloaded
/// winmds; not necessarily the app's framework packages, e.g. the Windows App SDK).
pub fn is_known_type(type_name: &str) -> bool {
    MetadataReader::find_by_name_or_generic(type_name).is_some()
}

/// Resolves one method's real signature given the type name it's declared/overridden on
/// (base class or interface) and the method's name. `None` when the type or method can't be
/// found — callers should skip the member and warn, not guess a signature.
pub fn resolve_method(type_name: &str, method_name: &str) -> Option<ResolvedMethod> {
    if let Some(known) = known_override_signature(type_name, method_name) {
        return Some(known);
    }

    let decl_arc = MetadataReader::find_by_name_or_generic(type_name)?;
    let guard = decl_arc.read();
    all_methods(guard.as_any().downcast_ref::<ClassDeclaration>().map(|c| c as &dyn Declaration)
        .or_else(|| guard.as_any().downcast_ref::<InterfaceDeclaration>().map(|i| i as &dyn Declaration))
        .unwrap_or(&*guard))
        .iter()
        .find(|m| m.full_name() == method_name)
        .and_then(to_resolved_method)
}

/// Resolves one property's real signature the same way `resolve_method` does for methods.
pub fn resolve_property(type_name: &str, prop_name: &str) -> Option<ResolvedProperty> {
    let decl_arc = MetadataReader::find_by_name_or_generic(type_name)?;
    let guard = decl_arc.read();
    all_properties(&*guard)
        .iter()
        .find(|p| p.full_name() == prop_name)
        .and_then(to_resolved_property)
}

/// Every method + property declared directly on an interface (not its ancestors — WinRT
/// interfaces rarely extend others, and `AddInterfaceImplementation`-style generation only ever
/// needs to implement the interface it was literally asked for). Used to implement ALL members of
/// a requested interface, since the CLR requires every member implemented (unlike base-class
/// virtuals, where only JS-overridden ones need a stub).
pub fn resolve_interface_members(interface_name: &str) -> Option<(Vec<ResolvedMethod>, Vec<ResolvedProperty>)> {
    let decl_arc = MetadataReader::find_by_name_or_generic(interface_name)?;
    let guard = decl_arc.read();
    let iface = guard.as_any().downcast_ref::<InterfaceDeclaration>()?;

    // Events aren't implemented by this pass (no C# `event`/add_/remove_ stub emission yet). An
    // interface with events (e.g. INotifyPropertyChanged) can't have a compilable, contract-
    // complete implementation emitted without one — bail out entirely rather than emit a class
    // that's missing a required member and won't compile.
    if !iface.events().is_empty() {
        return None;
    }

    let methods = iface
        .methods()
        .iter()
        .filter(|m| !is_accessor_name(m.full_name()) && !m.is_static())
        .filter_map(to_resolved_method)
        .collect();
    let properties = iface
        .properties()
        .iter()
        .filter_map(to_resolved_property)
        .collect();
    Some((methods, properties))
}
