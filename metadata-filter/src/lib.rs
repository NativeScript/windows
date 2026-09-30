use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

pub const WHITELIST_FILE: &str = "whitelist.mdg";
pub const BLACKLIST_FILE: &str = "blacklist.mdg";

/// Always allowed, as Android always allows `com.tns.gen*`: the runtime's own types and sbg's
/// generated proxies (`NativeScript.Gen.*`).
const ALWAYS_ALLOWED_ROOT: &str = "NativeScript";

/// Roots a JS extension can derive from without WinRT metadata to check it against (the Windows
/// App SDK's `Microsoft.UI.*` lives in a framework package, not system metadata).
const EXTENDABLE_ROOTS: &[&str] = &["Windows", "Microsoft", "System", "NativeScript"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    namespace: String,
    type_name: String,
}

impl Pattern {
    fn matches(&self, namespace: &str, type_name: &str) -> bool {
        (self.namespace.is_empty() || wildcard_match(&self.namespace, namespace))
            && (self.type_name.is_empty() || wildcard_match(&self.type_name, type_name))
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.type_name.is_empty() {
            f.write_str(&self.namespace)
        } else {
            write!(f, "{}:{}", self.namespace, self.type_name)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PatternList(Vec<Pattern>);

impl PatternList {
    pub fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("//"))
            .map(|line| {
                let (namespace, type_name) = line.split_once(':').unwrap_or((line, ""));
                Pattern {
                    namespace: namespace.trim().to_owned(),
                    type_name: type_name.trim().to_owned(),
                }
            })
            .collect();
        Self(patterns)
    }

    /// `None` when there is no such file.
    pub fn from_file(path: &Path) -> io::Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(Self::parse(&text))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn find(&self, namespace: &str, type_name: &str) -> Option<&Pattern> {
        self.0.iter().find(|pattern| pattern.matches(namespace, type_name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict<'a> {
    Allowed,
    NotWhitelisted,
    Blacklisted(&'a Pattern),
}

impl fmt::Display for Verdict<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Allowed => f.write_str("allowed"),
            Verdict::NotWhitelisted => f.write_str("not whitelisted"),
            Verdict::Blacklisted(pattern) => write!(f, "blacklisted by '{pattern}'"),
        }
    }
}

/// The native API an app allows itself (`App_Resources/Windows/native-api-usage.json`, which the CLI
/// writes out as `whitelist.mdg` / `blacklist.mdg`, as for Android and iOS). Each line is a
/// `namespace:type` pattern (`*` and `?` wildcards; a bare `namespace` covers every type in it; `#`
/// and `//` start comments). A whitelist, when there is one, is exclusive; the blacklist wins.
#[derive(Debug, Clone, Default)]
pub struct MetadataFilter {
    whitelist: Option<PatternList>,
    blacklist: PatternList,
}

impl MetadataFilter {
    pub fn new(whitelist: Option<PatternList>, blacklist: PatternList) -> Self {
        Self { whitelist, blacklist }
    }

    /// `whitelist.mdg` and `blacklist.mdg` in `dir`, where present.
    pub fn load(dir: &Path) -> io::Result<Self> {
        Self::from_files(Some(&dir.join(WHITELIST_FILE)), Some(&dir.join(BLACKLIST_FILE)))
    }

    pub fn from_files(whitelist: Option<&Path>, blacklist: Option<&Path>) -> io::Result<Self> {
        let whitelist = match whitelist {
            Some(path) => PatternList::from_file(path)?,
            None => None,
        };
        let blacklist = match blacklist {
            Some(path) => PatternList::from_file(path)?.unwrap_or_default(),
            None => PatternList::default(),
        };
        Ok(Self::new(whitelist, blacklist))
    }

    /// Nothing is filtered.
    pub fn is_empty(&self) -> bool {
        self.whitelist.is_none() && self.blacklist.is_empty()
    }

    /// `full_name`: a type's full name, generic (`` Windows.Foundation.IReference`1 ``) or not.
    pub fn check(&self, full_name: &str) -> Verdict<'_> {
        if self.is_empty() {
            return Verdict::Allowed;
        }
        let (namespace, type_name) = split_type_name(full_name);
        if let Some(pattern) = self.blacklist.find(namespace, type_name) {
            return Verdict::Blacklisted(pattern);
        }
        let whitelisted = match &self.whitelist {
            None => true,
            Some(whitelist) => is_always_allowed(namespace) || whitelist.find(namespace, type_name).is_some(),
        };
        if whitelisted {
            Verdict::Allowed
        } else {
            Verdict::NotWhitelisted
        }
    }

    pub fn allows(&self, full_name: &str) -> bool {
        self.check(full_name) == Verdict::Allowed
    }
}

fn is_always_allowed(namespace: &str) -> bool {
    namespace == ALWAYS_ALLOWED_ROOT
        || namespace
            .strip_prefix(ALWAYS_ALLOWED_ROOT)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Namespace and type name, without generic arity or arguments.
fn split_type_name(full_name: &str) -> (&str, &str) {
    let open = full_name.split('<').next().unwrap_or(full_name);
    let open = open.split('`').next().unwrap_or(open);
    open.rsplit_once('.').unwrap_or(("", open))
}

/// `*` matches any run of characters (none included), `?` any one.
pub fn wildcard_match(pattern: &str, input: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let input: Vec<char> = input.chars().collect();
    let (mut p, mut i) = (0, 0);
    // The last `*` seen and where in `input` it started matching, to backtrack to.
    let mut star: Option<(usize, usize)> = None;
    while i < input.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == input[i]) {
            p += 1;
            i += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, i));
            p += 1;
        } else if let Some((star_p, star_i)) = star {
            p = star_p + 1;
            i = star_i + 1;
            star = Some((star_p, star_i + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

fn is_identifier(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A name that could be a WinRT/.NET type: two or more identifier segments (a generic arity
/// suffix aside). A bundled library's minified class (`Ua.$`) is not.
pub fn is_dotted_identifier(name: &str) -> bool {
    let open = name.split('`').next().unwrap_or(name);
    let mut segments = open.split('.');
    let first_two = segments.next().is_some_and(is_identifier) && segments.next().is_some_and(is_identifier);
    first_two && segments.all(is_identifier)
}

/// Whether a JS class's base can be a WinRT type without metadata to look it up in: a dotted
/// identifier under a root JS extends native types from. Library classes (`u.MaterialDefines`,
/// `Phaser.Utils`) are not; sbg also accepts bases it finds in WinRT metadata.
pub fn plausible_base(name: &str) -> bool {
    is_dotted_identifier(name) && name.split('.').next().is_some_and(|root| EXTENDABLE_ROOTS.contains(&root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(whitelist: Option<&str>, blacklist: &str) -> MetadataFilter {
        MetadataFilter::new(whitelist.map(PatternList::parse), PatternList::parse(blacklist))
    }

    #[test]
    fn parses_lines_like_android_and_ios() {
        let list = PatternList::parse("# comment\n// comment\n\n  Windows.Storage:StorageFile  \nWindows.UI.*\n");
        assert_eq!(list.0.len(), 2);
        assert_eq!(list.0[0].to_string(), "Windows.Storage:StorageFile");
        assert_eq!(list.0[1].to_string(), "Windows.UI.*");
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("*", "Windows"));
        assert!(wildcard_match("Windows.*", "Windows.Storage"));
        assert!(!wildcard_match("Windows.*", "Windows"));
        assert!(wildcard_match("Windows*", "Windows"));
        assert!(wildcard_match("W?ndows", "Windows"));
        assert!(!wildcard_match("W?ndows", "Wndows"));
        assert!(wildcard_match("*.Storage.*", "Windows.Storage.Pickers"));
        assert!(wildcard_match("a*b*c", "axxbyyc"));
        assert!(!wildcard_match("a*b*c", "axxbyy"));
        assert!(!wildcard_match("Storage", "StorageFile"));
        // A long mismatch stays linear-ish instead of blowing up.
        assert!(!wildcard_match("*a*a*a*a*a*a*a*b", &"a".repeat(200)));
    }

    #[test]
    fn no_files_allow_everything() {
        let f = MetadataFilter::default();
        assert!(f.is_empty());
        assert!(f.allows("Windows.Storage.StorageFile"));
    }

    #[test]
    fn a_whitelist_is_exclusive() {
        let f = filter(Some("Windows.Storage:StorageFile\nMicrosoft.UI.Xaml*"), "");
        assert!(f.allows("Windows.Storage.StorageFile"));
        assert!(f.allows("Microsoft.UI.Xaml.Controls.Button"));
        assert_eq!(f.check("Windows.Storage.StorageFolder"), Verdict::NotWhitelisted);
    }

    #[test]
    fn the_blacklist_wins() {
        let f = filter(Some("Windows.Storage*"), "Windows.Storage.Pickers");
        assert!(f.allows("Windows.Storage.StorageFile"));
        let verdict = f.check("Windows.Storage.Pickers.FileOpenPicker");
        assert_eq!(verdict.to_string(), "blacklisted by 'Windows.Storage.Pickers'");
    }

    #[test]
    fn generic_names_match_their_open_type() {
        let f = filter(None, "Windows.Foundation:IReference");
        assert!(!f.allows("Windows.Foundation.IReference`1"));
        assert!(!f.allows("Windows.Foundation.IReference`1<Int32>"));
        assert!(f.allows("Windows.Foundation.Uri"));
    }

    #[test]
    fn the_runtimes_own_types_pass_a_whitelist_but_not_the_blacklist() {
        let f = filter(Some("Windows.Storage*"), "");
        assert!(f.allows("NativeScript.Gen.Button_1"));
        assert!(f.allows("NativeScript.Widgets.StackLayout"));
        assert!(!f.allows("NativeScriptish.Thing"));
        let f = filter(Some("Windows.Storage*"), "NativeScript.Widgets");
        assert!(!f.allows("NativeScript.Widgets.StackLayout"));
    }

    #[test]
    fn plausible_bases() {
        assert!(plausible_base("Microsoft.UI.Xaml.Controls.Button"));
        assert!(plausible_base("Windows.Foundation.Collections.IVector`1"));
        assert!(!plausible_base("Ua.$"));
        assert!(!plausible_base("u.MaterialDefines"));
        assert!(!plausible_base("Phaser.Utils"));
        assert!(!plausible_base("Windows"));
        assert!(is_dotted_identifier("CommunityToolkit.WinUI.Controls.Segmented"));
        assert!(!is_dotted_identifier("Ua.$"));
    }
}
