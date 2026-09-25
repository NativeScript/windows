use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::Declaration;
use metadata::declarations::event_declaration::EventDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::declarations::property_declaration::PropertyDeclaration;
use metadata::meta_data_reader::MetadataReader;
use std::collections::HashSet;

#[cfg(feature = "classic")]
pub(crate) fn split_type_name(type_name: &str) -> (Option<String>, String) {
    match type_name.rsplit_once('.') {
        Some((namespace, class_name)) => (Some(namespace.to_string()), class_name.to_string()),
        None => (None, type_name.to_string()),
    }
}

pub(crate) fn extend_class_methods(
    class_declaration: &ClassDeclaration,
    methods: &mut Vec<MethodDeclaration>,
    seen: &mut HashSet<String>,
) {
    for method in class_declaration.methods() {
        let mut method_name = method.overload_name().to_string();
        if method_name.is_empty() {
            method_name = method.name().to_string();
        }
        if seen.insert(method_name) {
            methods.push(method.clone());
        }
    }

    if let Some(default_interface) = class_declaration.default_interface() {
        for method in default_interface.methods() {
            let mut method_name = method.overload_name().to_string();
            if method_name.is_empty() {
                method_name = method.name().to_string();
            }
            if seen.insert(method_name) {
                methods.push(method.clone());
            }
        }
    }

    for interface in class_declaration.implemented_interfaces() {
        for method in interface.methods() {
            let mut method_name = method.overload_name().to_string();
            if method_name.is_empty() {
                method_name = method.name().to_string();
            }
            if seen.insert(method_name) {
                methods.push(method.clone());
            }
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                extend_class_methods(base_class, methods, seen);
            }
        }
    }
}

/// Like `extend_class_methods`, but keeps every overload under its public name instead of only
/// the first-seen one. The callers that build actually-callable closures (`ns_hostobject`'s host
/// ctor/prototype) need every arity so they can pick the one matching the call's argument count;
/// `extend_class_methods`'s one-per-name view is only correct for enumeration (member listing).
pub(crate) fn extend_class_methods_grouped(
    class_declaration: &ClassDeclaration,
    out: &mut std::collections::HashMap<String, Vec<MethodDeclaration>>,
) {
    // Same method can legitimately appear in both `methods()` and `default_interface()`/an
    // implemented interface (WinRT class methods are usually declared via their interface); a
    // per-name+arity dedup keeps those from producing duplicate candidates without dropping
    // genuinely distinct overloads that happen to share an arity across interfaces (rare, but
    // arity is the only signal callers select on, so name+arity is the right dedup key here).
    let mut push = |key: &str, m: &MethodDeclaration| {
        let bucket = out.entry(key.to_string()).or_default();
        if !bucket.iter().any(|existing: &MethodDeclaration| {
            existing.number_of_parameters() == m.number_of_parameters()
        }) {
            bucket.push(m.clone());
        }
    };
    let mut all: Vec<&MethodDeclaration> = class_declaration.methods().iter().collect();
    if let Some(default_interface) = class_declaration.default_interface() {
        all.extend(default_interface.methods().iter());
    }
    for interface in class_declaration.implemented_interfaces() {
        all.extend(interface.methods().iter());
    }
    // Same-interface overloads carry an `[Overload]` metadata name (`CreateColorBrush(Color)` is
    // `CreateColorBrushWithColor`); that name stays callable as-is. The public name must also
    // reach every overload, or `CreateColorBrush(color)` resolves to the 0-arg method and the
    // argument is silently dropped. Second pass, so a default overload wins an arity tie.
    for m in &all {
        let on = m.overload_name();
        push(if on.is_empty() { m.name() } else { on }, m);
    }
    for m in &all {
        let on = m.overload_name();
        if !on.is_empty() && on != m.name() {
            push(m.name(), m);
        }
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                extend_class_methods_grouped(base_class, out);
            }
        }
    }
}

pub(crate) fn extend_class_properties(
    class_declaration: &ClassDeclaration,
    properties: &mut Vec<PropertyDeclaration>,
    seen: &mut HashSet<String>,
) {
    for property in class_declaration.properties() {
        if seen.insert(property.name().to_string()) {
            properties.push(property.clone());
        }
    }

    if let Some(default_interface) = class_declaration.default_interface() {
        for property in default_interface.properties() {
            if seen.insert(property.name().to_string()) {
                properties.push(property.clone());
            }
        }
    }

    for interface in class_declaration.implemented_interfaces() {
        for property in interface.properties() {
            if seen.insert(property.name().to_string()) {
                properties.push(property.clone());
            }
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                extend_class_properties(base_class, properties, seen);
            }
        }
    }
}

pub(crate) fn collect_class_methods(
    class_declaration: &ClassDeclaration,
) -> Vec<MethodDeclaration> {
    let mut methods = Vec::new();
    let mut seen = HashSet::new();
    extend_class_methods(class_declaration, &mut methods, &mut seen);
    methods
}

pub(crate) fn collect_class_properties(
    class_declaration: &ClassDeclaration,
) -> Vec<PropertyDeclaration> {
    let mut properties = Vec::new();
    let mut seen = HashSet::new();
    extend_class_properties(class_declaration, &mut properties, &mut seen);
    properties
}

/// Look up a property by name across the class, its default interface,
/// implemented interfaces, and base-class chain — returning as soon as one
/// matches. Used on every property write, so it skips the `Vec` allocation
/// and full hierarchy walk that `collect_class_properties` does.
pub(crate) fn find_class_property(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<PropertyDeclaration> {
    if let Some(p) = class_declaration
        .properties()
        .iter()
        .find(|p| p.name() == name)
    {
        return Some(p.clone());
    }
    if let Some(di) = class_declaration.default_interface() {
        if let Some(p) = di.properties().iter().find(|p| p.name() == name) {
            return Some(p.clone());
        }
    }
    for iface in class_declaration.implemented_interfaces() {
        if let Some(p) = iface.properties().iter().find(|p| p.name() == name) {
            return Some(p.clone());
        }
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                return find_class_property(base_class, name);
            }
        }
    }
    None
}

/// Look up a method by name. Matches the overload name when present, falling
/// back to the plain name. Walks the class hierarchy and returns the first
/// match. Same hot-path benefit as `find_class_property`.
pub(crate) fn find_class_method(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<MethodDeclaration> {
    let matches = |m: &MethodDeclaration| {
        let on = m.overload_name();
        (!on.is_empty() && on == name) || m.name() == name
    };
    if let Some(m) = class_declaration.methods().iter().find(|m| matches(m)) {
        return Some(m.clone());
    }
    if let Some(di) = class_declaration.default_interface() {
        if let Some(m) = di.methods().iter().find(|m| matches(m)) {
            return Some(m.clone());
        }
    }
    for iface in class_declaration.implemented_interfaces() {
        if let Some(m) = iface.methods().iter().find(|m| matches(m)) {
            return Some(m.clone());
        }
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                return find_class_method(base_class, name);
            }
        }
    }
    None
}

/// Collect every method named `name` (matching the overload name when present, else the plain
/// name) across the class's own methods, default interface, implemented interfaces, and base
/// class chain. Unlike `find_class_method`, does not stop at the first match: WinRT overloads
/// sharing one public name (e.g. `Launcher.LaunchUriAsync(uri)` / `(uri, options)` /
/// `(uri, options, data)`) are each a distinct ABI method on a distinct interface, so a caller
/// needs every candidate to pick the one matching the arguments actually supplied.
pub(crate) fn find_class_methods(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Vec<MethodDeclaration> {
    let matches = |m: &MethodDeclaration| {
        let on = m.overload_name();
        (!on.is_empty() && on == name) || m.name() == name
    };
    let mut out: Vec<MethodDeclaration> = Vec::new();
    out.extend(
        class_declaration
            .methods()
            .iter()
            .filter(|m| matches(m))
            .cloned(),
    );
    if let Some(di) = class_declaration.default_interface() {
        out.extend(di.methods().iter().filter(|m| matches(m)).cloned());
    }
    for iface in class_declaration.implemented_interfaces() {
        out.extend(iface.methods().iter().filter(|m| matches(m)).cloned());
    }
    if !out.is_empty() {
        return out;
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_declaration) =
            MetadataReader::find_by_name(class_declaration.base_full_name())
        {
            let base_lock = base_declaration.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                return find_class_methods(base_class, name);
            }
        }
    }
    out
}

/// Pick the overload matching the supplied argument count from every method named `name`.
/// Falls back to the first name match when no candidate's arity matches exactly (e.g. the
/// metadata-reported arity is off, or `name` isn't a method at all) so callers can still surface
/// a normal WinRT-call error instead of silently doing nothing.
#[cfg(feature = "classic")]
pub(crate) fn find_class_method_by_arity(
    class_declaration: &ClassDeclaration,
    name: &str,
    argc: usize,
) -> Option<MethodDeclaration> {
    let candidates = find_class_methods(class_declaration, name);
    candidates
        .iter()
        .find(|m| m.number_of_parameters() == argc)
        .or_else(|| candidates.first())
        .cloned()
}

thread_local! {
    // Bound method (metadata scope, token) → every overload sharing its public name, for methods
    // whose overloads differ in arity. See `register_overload_siblings`.
    static OVERLOAD_SIBLINGS: std::cell::RefCell<ahash::AHashMap<(usize, i32), std::rc::Rc<[MethodDeclaration]>>> =
        std::cell::RefCell::new(ahash::AHashMap::new());
}

#[cfg(feature = "classic")]
fn method_identity(m: &MethodDeclaration) -> (usize, i32) {
    use windows::core::Interface;
    (m.metadata().map(|md| md.as_raw() as usize).unwrap_or(0), m.token().0)
}

/// The classic engine binds one `MethodDeclaration` per JS function when a member is first read,
/// before any arguments exist. WinRT same-interface overloads carry distinct `[Overload]` names
/// (`CreateColorBrush(Color)` is `CreateColorBrushWithColor`), so a lookup of the public name binds
/// only one of them. Record the others so the call can switch arity (`overload_for_argc`).
/// Only public-name lookups register; an `[Overload]`-name lookup stays bound to exactly that method.
#[cfg(feature = "classic")]
pub(crate) fn register_overload_siblings(
    class_declaration: &ClassDeclaration,
    js_name: &str,
    bound: &MethodDeclaration,
) {
    if bound.name() != js_name {
        return;
    }
    let key = method_identity(bound);
    if OVERLOAD_SIBLINGS.with(|m| m.borrow().contains_key(&key)) {
        return;
    }
    let mut siblings: Vec<MethodDeclaration> = vec![bound.clone()];
    for m in find_class_methods(class_declaration, js_name) {
        if m.name() == js_name
            && m.is_static() == bound.is_static()
            && !siblings
                .iter()
                .any(|s| s.number_of_parameters() == m.number_of_parameters())
        {
            siblings.push(m);
        }
    }
    if siblings.len() > 1 {
        OVERLOAD_SIBLINGS.with(|m| m.borrow_mut().insert(key, siblings.into()));
    }
}

/// The overload of `bound` matching `argc`, when `bound` itself doesn't (see
/// `register_overload_siblings`). `None` means call `bound` as-is.
#[cfg(feature = "classic")]
#[inline]
pub(crate) fn overload_for_argc(bound: &MethodDeclaration, argc: usize) -> Option<MethodDeclaration> {
    if bound.number_of_parameters() == argc {
        return None;
    }
    OVERLOAD_SIBLINGS.with(|m| {
        m.borrow()
            .get(&method_identity(bound))
            .and_then(|s| s.iter().find(|c| c.number_of_parameters() == argc).cloned())
    })
}

pub(crate) fn class_method_matches(class_declaration: &ClassDeclaration, name: &str) -> bool {
    let method_match = |m: &MethodDeclaration| {
        let on = m.overload_name();
        (!on.is_empty() && on == name) || m.name() == name
    };

    if class_declaration.methods().iter().any(method_match) {
        return true;
    }

    if let Some(di) = class_declaration.default_interface() {
        if di.methods().iter().any(method_match) {
            return true;
        }
    }

    for iface in class_declaration.implemented_interfaces() {
        if iface.methods().iter().any(method_match) {
            return true;
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                return class_method_matches(base, name);
            }
        }
    }
    false
}

#[cfg(feature = "classic")]
pub(crate) fn class_property_matches(class_declaration: &ClassDeclaration, name: &str) -> bool {
    if class_declaration
        .properties()
        .iter()
        .any(|p| p.name() == name)
    {
        return true;
    }

    if let Some(di) = class_declaration.default_interface() {
        if di.properties().iter().any(|p| p.name() == name) {
            return true;
        }
    }

    for iface in class_declaration.implemented_interfaces() {
        if iface.properties().iter().any(|p| p.name() == name) {
            return true;
        }
    }

    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                return class_property_matches(base, name);
            }
        }
    }
    false
}

#[cfg(feature = "classic")]
pub(crate) fn class_has_member_named(class_declaration: &ClassDeclaration, name: &str) -> bool {
    class_method_matches(class_declaration, name) || class_property_matches(class_declaration, name)
}

/// Collects all properties in the class hierarchy, each paired with the full
/// name of the WinRT class that declares that property. For static properties
/// this lets callers retrieve the correct activation factory (e.g. UIElement's
/// factory, not Panel's, for `UIElement.PointerPressedEvent`).
#[cfg(feature = "classic")]
pub(crate) fn collect_class_properties_with_declaring(
    class_declaration: &ClassDeclaration,
) -> Vec<(PropertyDeclaration, String)> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    extend_properties_with_declaring(
        class_declaration,
        class_declaration.full_name(),
        &mut result,
        &mut seen,
    );
    result
}

#[cfg(feature = "classic")]
fn extend_properties_with_declaring(
    class_declaration: &ClassDeclaration,
    declaring_name: &str,
    result: &mut Vec<(PropertyDeclaration, String)>,
    seen: &mut HashSet<String>,
) {
    for property in class_declaration.properties() {
        if seen.insert(property.name().to_string()) {
            result.push((property.clone(), declaring_name.to_string()));
        }
    }
    if let Some(di) = class_declaration.default_interface() {
        for property in di.properties() {
            if seen.insert(property.name().to_string()) {
                result.push((property.clone(), declaring_name.to_string()));
            }
        }
    }
    for iface in class_declaration.implemented_interfaces() {
        for property in iface.properties() {
            if seen.insert(property.name().to_string()) {
                result.push((property.clone(), declaring_name.to_string()));
            }
        }
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let base_lock = base_decl.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                extend_properties_with_declaring(base_class, base_class.full_name(), result, seen);
            }
        }
    }
}

/// Walk the class hierarchy to find which WinRT class actually declares a given
/// static property. Returns the full class name (e.g. "Windows.UI.Xaml.UIElement").
#[cfg(feature = "classic")]
pub(crate) fn find_static_property_declaring_class(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<String> {
    if class_declaration
        .properties()
        .iter()
        .any(|p| p.name() == name && p.is_static())
    {
        return Some(class_declaration.full_name().to_string());
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let base_lock = base_decl.read();
            if let Some(base_class) = base_lock.as_any().downcast_ref::<ClassDeclaration>() {
                return find_static_property_declaring_class(base_class, name);
            }
        }
    }
    None
}

pub(crate) fn find_event_methods(
    class_declaration: &ClassDeclaration,
    name: &str,
) -> Option<(MethodDeclaration, MethodDeclaration)> {
    let check = |events: &[EventDeclaration]| -> Option<(MethodDeclaration, MethodDeclaration)> {
        events
            .iter()
            .find(|e| e.name() == name)
            .map(|e| (e.add_method().clone(), e.remove_method().clone()))
    };

    if let Some(m) = check(class_declaration.events()) {
        return Some(m);
    }
    if let Some(di) = class_declaration.default_interface() {
        if let Some(m) = check(di.events()) {
            return Some(m);
        }
    }
    for iface in class_declaration.implemented_interfaces() {
        if let Some(m) = check(iface.events()) {
            return Some(m);
        }
    }
    if !class_declaration.base_full_name().is_empty() {
        if let Some(base_decl) = MetadataReader::find_by_name(class_declaration.base_full_name()) {
            let lock = base_decl.read();
            if let Some(base) = lock.as_any().downcast_ref::<ClassDeclaration>() {
                return find_event_methods(base, name);
            }
        }
    }
    None
}

#[cfg(feature = "classic")]
pub(crate) fn find_interface_event_methods(
    declaration: &dyn Declaration,
    name: &str,
) -> Option<(MethodDeclaration, MethodDeclaration)> {
    let events: &[EventDeclaration] = if let Some(iface) = declaration
        .as_any()
        .downcast_ref::<metadata::declarations::interface_declaration::InterfaceDeclaration>()
    {
        iface.events()
    } else if let Some(iface) = declaration
        .as_any()
        .downcast_ref::<metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration>()
    {
        iface.events()
    } else {
        return None;
    };
    events
        .iter()
        .find(|e| e.name() == name)
        .map(|e| (e.add_method().clone(), e.remove_method().clone()))
}
