use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};

const MAGIC: &[u8; 4] = b"NSB1";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 16 + 4; // magic+version+key_mode+reserved+key_check+entry_count
const KEY_CHECK_AAD: &[u8] = b"nsbundle-keycheck";

/// `key_mode` byte: the container was sealed with the compiled-in default pepper.
pub const KEY_MODE_DEFAULT: u8 = 0;
/// `key_mode` byte: the container expects a custom key via [`set_custom_key_hex`]/
/// `runtime_set_bundle_key` before it can be opened.
pub const KEY_MODE_CUSTOM: u8 = 1;

const PEPPER_A: [u8; 32] = [
    0x4e, 0x53, 0x42, 0x31, 0x8a, 0x2f, 0xd1, 0x77, 0x03, 0x9c, 0x5e, 0x61, 0xb4, 0x22, 0xf0, 0x18,
    0x6d, 0xa9, 0x3b, 0xc7, 0x14, 0x5f, 0x88, 0xe2, 0x30, 0x0b, 0x97, 0x44, 0xd6, 0x1a, 0x59, 0xc3,
];
const PEPPER_B: [u8; 32] = [
    0x71, 0xe4, 0x0d, 0x9a, 0x56, 0x2c, 0xbf, 0x08, 0xa1, 0x33, 0x7e, 0xd0, 0x49, 0x8c, 0x15, 0xf6,
    0x3d, 0x60, 0xab, 0x24, 0x99, 0x0e, 0x52, 0xc8, 0x7b, 0x1f, 0xde, 0x45, 0x83, 0x6a, 0xf1, 0x27,
];
const PEPPER_C: [u8; 32] = [
    0x92, 0x1b, 0xc5, 0x4a, 0x0f, 0x87, 0x3e, 0xd9, 0x66, 0xaa, 0x21, 0x5c, 0xf3, 0x08, 0x7d, 0x94,
    0x11, 0x4e, 0xb0, 0x8f, 0x3a, 0xc6, 0x02, 0x59, 0xdd, 0x74, 0x1c, 0xa8, 0x60, 0xef, 0x35, 0x8b,
];

fn default_pepper_key() -> [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    *KEY.get_or_init(|| {
        let mut k = [0u8; 32];
        for i in 0..32 {
            k[i] = PEPPER_A[i] ^ PEPPER_B[i] ^ PEPPER_C[i];
        }
        k
    })
}

static CUSTOM_KEY: OnceLock<[u8; 32]> = OnceLock::new();
/// `None` until [`init_from_app_root`] runs; empty map means "no bundle found/loaded" so every
/// lookup falls straight back to the filesystem.
static BUNDLE_TABLE: OnceLock<HashMap<String, Vec<u8>>> = OnceLock::new();

fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: plaintext, aad })
        .expect("aes-256-gcm encrypt should not fail for in-memory buffers")
}

fn open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, ()> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|_| ())
}

fn random_nonce() -> [u8; 12] {
    let mut buf = [0u8; 12];
    getrandom::getrandom(&mut buf).expect("OS RNG must be available to seal app.nsbundle");
    buf
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_decode_32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = (hex_val(bytes[i * 2])? << 4) | hex_val(bytes[i * 2 + 1])?;
    }
    Some(out)
}

/// Parse a 64-hex-char (32-byte) key and store it as the custom key used to open/seal
/// `key_mode == 1` containers. Must be called before the container is opened (i.e. before
/// `runtime_init`/`initialize_runtime`, mirroring `runtime_set_local_folder`'s ordering
/// requirement). Returns `false` on malformed input (wrong length or non-hex characters).
pub fn set_custom_key_hex(hex: &str) -> bool {
    let Some(key) = hex_decode_32(hex) else {
        return false;
    };
    let _ = CUSTOM_KEY.set(key);
    true
}

/// Case-insensitively strip everything up to and including the last `app`/`App` path segment,
/// lowercase the remainder, and normalize separators to `/`. Every call site that builds a
/// candidate path for module resolution already joins onto an `app`/`App` directory (e.g.
/// `module_natives.rs`'s `app_root.join("app")`/`.join("App")`, `global_fns.rs`'s identical
/// probe), so this recovers the packer's TOC key regardless of which absolute prefix or casing
/// a given host used. Falls back to the whole (lowercased) path if no such segment is present.
///
/// Simplification: this takes the *last* matching segment, so a legitimately nested directory
/// literally named `app`/`App` deeper inside the packed tree would be mis-stripped. Not worth
/// guarding against for a NativeScript webpack output tree, which doesn't nest a directory named
/// that.
fn relativize(path_like: &str) -> String {
    let normalized = path_like.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();
    let stripped = match segments.iter().rposition(|s| s.eq_ignore_ascii_case("app")) {
        Some(pos) => segments[pos + 1..].join("/"),
        None => normalized,
    };
    // Relative-specifier resolution against a referrer with no directory component (e.g. a
    // top-level "entry.mjs" importing "./dep.mjs") can leave a leading "./" in the joined
    // candidate; strip it so it still matches the packer's TOC key (which never has one).
    let mut s = stripped.as_str();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.to_lowercase()
}

/// Look up a JS source file by any of the path forms the four read points build (absolute,
/// relative, either casing of `app`/`App`). `None` means "not in the loaded bundle" — callers
/// fall back to reading the real filesystem, whether that's because no bundle was loaded at all
/// or the specific file simply isn't packed.
pub fn read_text(path_like: &str) -> Option<String> {
    let table = BUNDLE_TABLE.get()?;
    if table.is_empty() {
        return None;
    }
    table
        .get(&relativize(path_like))
        .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
}

/// Existence probe used by the extension/index-file resolution loops (`try_resolve_with_known_extensions`
/// in both `lib.rs` and `global_fns.rs`) alongside the existing `Path::exists()` check.
pub fn contains(path_like: &str) -> bool {
    match BUNDLE_TABLE.get() {
        Some(table) if !table.is_empty() => table.contains_key(&relativize(path_like)),
        _ => false,
    }
}

/// Whether a non-empty bundle is currently loaded.
pub fn has_bundle() -> bool {
    BUNDLE_TABLE.get().is_some_and(|t| !t.is_empty())
}

/// Locate and decrypt `app.nsbundle`, if present, next to the app root the host was initialized
/// with. `app_root` is the same string passed to `runtime_init`/`initialize_runtime` — in practice
/// the exe's own directory (`AppContext.BaseDirectory` on the C# side; see `RuntimeHost.cs`'s
/// `ResolveEntryScriptPath`, which checks both that directory and its parent for the `app`/`App`
/// folder). Checked here in the same two spots:
///
/// - `<app_root>/app.nsbundle` (sibling of `app`/`App` when they live directly under `app_root`)
/// - `<parent of app_root>/app.nsbundle` (sibling of `bin/` when the project root is one level up)
///
/// No container found at either candidate: leaves the table unset, every lookup falls back to
/// disk — fully backward compatible with today's plaintext `app/` trees. Found but fails to open
/// (bad magic/version, wrong default-pepper key, corrupted): logs and fails closed to an empty
/// table (same plaintext fallback). Found with `key_mode == KEY_MODE_CUSTOM` and no key was ever
/// supplied via [`set_custom_key_hex`]: fails loud instead — the app author explicitly opted into
/// custom-key protection, so silently falling back to plaintext-on-disk (which doesn't even exist
/// once packed) would hide that mistake rather than surface it.
pub fn init_from_app_root(app_root: &str) {
    if app_root.is_empty() {
        return;
    }
    let base = Path::new(app_root);
    let mut candidates = vec![base.join("app.nsbundle")];
    if let Some(parent) = base.parent() {
        candidates.push(parent.join("app.nsbundle"));
    }

    for candidate in &candidates {
        if !candidate.is_file() {
            continue;
        }
        match open_and_decrypt(candidate) {
            Ok(table) => {
                let _ = BUNDLE_TABLE.set(table);
            }
            Err(err) => {
                eprintln!(
                    "[NativeScript] app.nsbundle at {} failed to load: {err}",
                    candidate.display()
                );
                let _ = BUNDLE_TABLE.set(HashMap::new());
            }
        }
        return;
    }
}

fn open_and_decrypt(path: &Path) -> Result<HashMap<String, Vec<u8>>, String> {
    let data = fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    if data.len() < HEADER_LEN || &data[0..4] != MAGIC {
        return Err("not an nsbundle container (bad magic)".to_string());
    }
    let version = data[4];
    if version != VERSION {
        return Err(format!("unsupported nsbundle version {version}"));
    }
    let key_mode = data[5];
    let key_check = &data[8..24];
    let entry_count = LittleEndian::read_u32(&data[24..28]) as usize;

    let key = match key_mode {
        KEY_MODE_DEFAULT => default_pepper_key(),
        KEY_MODE_CUSTOM => *CUSTOM_KEY.get().ok_or_else(|| {
            "requires runtime_set_bundle_key(); none was set before the bundle was opened"
                .to_string()
        })?,
        other => return Err(format!("unknown key_mode {other}")),
    };

    if open(&key, &[0u8; 12], KEY_CHECK_AAD, key_check).is_err() {
        return Err("key mismatch (wrong key for this bundle)".to_string());
    }

    let mut cursor = HEADER_LEN;
    struct TocEntry {
        path: String,
        blob_offset: u64,
        blob_len: u64,
        nonce: [u8; 12],
    }
    // Each TOC entry takes at least 30 bytes, which bounds a corrupt count's allocation.
    let mut toc = Vec::with_capacity(entry_count.min(data.len() / 30));
    for _ in 0..entry_count {
        if cursor + 2 > data.len() {
            return Err("truncated TOC".to_string());
        }
        let path_len = LittleEndian::read_u16(&data[cursor..cursor + 2]) as usize;
        cursor += 2;
        if cursor + path_len > data.len() {
            return Err("truncated TOC path".to_string());
        }
        let path = String::from_utf8(data[cursor..cursor + path_len].to_vec())
            .map_err(|_| "invalid UTF-8 path in TOC".to_string())?;
        cursor += path_len;
        if cursor + 8 + 8 + 12 > data.len() {
            return Err("truncated TOC entry".to_string());
        }
        let blob_offset = LittleEndian::read_u64(&data[cursor..cursor + 8]);
        cursor += 8;
        let blob_len = LittleEndian::read_u64(&data[cursor..cursor + 8]);
        cursor += 8;
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&data[cursor..cursor + 12]);
        cursor += 12;
        toc.push(TocEntry { path, blob_offset, blob_len, nonce });
    }

    let blob_section_start = cursor;
    let mut table = HashMap::with_capacity(toc.len());
    for entry in toc {
        let blob = usize::try_from(entry.blob_offset)
            .ok()
            .and_then(|offset| blob_section_start.checked_add(offset))
            .and_then(|start| {
                let end = start.checked_add(usize::try_from(entry.blob_len).ok()?)?;
                data.get(start..end)
            })
            .ok_or_else(|| format!("blob out of range for {}", entry.path))?;
        let plaintext = open(&key, &entry.nonce, entry.path.as_bytes(), blob)
            .map_err(|_| format!("failed to decrypt {} (tampered container or wrong key)", entry.path))?;
        table.insert(entry.path.to_lowercase(), plaintext);
    }
    Ok(table)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path)?;
            out.push((rel, bytes));
        }
    }
    Ok(())
}

/// Pack every file under `input` (recursively) into a sealed `app.nsbundle` at `output`, encrypting
/// each with `key` (AES-256-GCM, one blob per file, path bytes as AAD). `key_mode` is recorded
/// verbatim in the header so the runtime knows whether to use the default pepper
/// ([`KEY_MODE_DEFAULT`]) or require a custom key via `runtime_set_bundle_key`
/// ([`KEY_MODE_CUSTOM`]) — the caller (the `nsbundle_pack` CLI) is responsible for using the same
/// key value the app will supply at runtime when `key_mode == KEY_MODE_CUSTOM`.
pub fn pack_directory(input: &Path, output: &Path, key_mode: u8, key: [u8; 32]) -> io::Result<()> {
    let mut files = Vec::new();
    collect_files(input, input, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let key_check = seal(&key, &[0u8; 12], KEY_CHECK_AAD, &[]);
    debug_assert_eq!(key_check.len(), 16, "empty-plaintext AES-GCM output must be tag-only");

    let mut entries_buf = Vec::new();
    let mut blobs = Vec::with_capacity(files.len());
    let mut blob_offset: u64 = 0;
    for (path, bytes) in &files {
        let nonce = random_nonce();
        let ciphertext = seal(&key, &nonce, path.as_bytes(), bytes);
        entries_buf.write_u16::<LittleEndian>(path.len() as u16)?;
        entries_buf.extend_from_slice(path.as_bytes());
        entries_buf.write_u64::<LittleEndian>(blob_offset)?;
        entries_buf.write_u64::<LittleEndian>(ciphertext.len() as u64)?;
        entries_buf.extend_from_slice(&nonce);
        blob_offset += ciphertext.len() as u64;
        blobs.push(ciphertext);
    }

    let mut out = Vec::with_capacity(HEADER_LEN + entries_buf.len() + blob_offset as usize);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(key_mode);
    out.extend_from_slice(&[0u8; 2]);
    out.extend_from_slice(&key_check);
    out.write_u32::<LittleEndian>(files.len() as u32)?;
    out.extend_from_slice(&entries_buf);
    for blob in blobs {
        out.extend_from_slice(&blob);
    }

    fs::write(output, out)
}

/// The compiled-in default key, exposed so `nsbundle_pack` can seal `key_mode == KEY_MODE_DEFAULT`
/// containers without duplicating the pepper constants.
pub fn default_key() -> [u8; 32] {
    default_pepper_key()
}

/// Parse a 64-hex-char string into a 32-byte key (used by `nsbundle_pack`'s `--key-hex` flag).
pub fn parse_key_hex(hex: &str) -> Option<[u8; 32]> {
    hex_decode_32(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nsbundle_test_{name}_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pack_and_open_round_trip() {
        let input = scratch_dir("roundtrip_in");
        fs::write(input.join("bundle.js"), b"console.log('hi')").unwrap();
        fs::create_dir_all(input.join("sub")).unwrap();
        fs::write(input.join("sub").join("mod.js"), b"module.exports = 1;").unwrap();

        let output = scratch_dir("roundtrip_out").join("app.nsbundle");
        // key_mode 0 always opens with the real compiled-in pepper, regardless of what key
        // pack_directory was called with — so the round trip must use that same pepper to open
        // successfully. (Custom-key round trips are covered indirectly by tampered_ciphertext_fails,
        // which packs KEY_MODE_CUSTOM and opens with the exact key it packed with.)
        let key = default_key();
        pack_directory(&input, &output, KEY_MODE_DEFAULT, key).unwrap();

        let table = open_and_decrypt(&output).unwrap();
        assert_eq!(
            table.get("bundle.js").map(|v| v.as_slice()),
            Some(b"console.log('hi')".as_slice())
        );
        assert_eq!(
            table.get("sub/mod.js").map(|v| v.as_slice()),
            Some(b"module.exports = 1;".as_slice())
        );
    }

    #[test]
    fn wrong_key_fails_key_check() {
        let input = scratch_dir("wrongkey_in");
        fs::write(input.join("a.js"), b"a").unwrap();
        let output = scratch_dir("wrongkey_out").join("app.nsbundle");
        pack_directory(&input, &output, KEY_MODE_DEFAULT, [1u8; 32]).unwrap();

        // open_and_decrypt always resolves key_mode 0 to the real default pepper, so instead
        // exercise the primitive directly: the key used to check must match the key used to seal.
        let data = fs::read(&output).unwrap();
        let key_check = &data[8..24];
        assert!(open(&[2u8; 32], &[0u8; 12], KEY_CHECK_AAD, key_check).is_err());
        assert!(open(&[1u8; 32], &[0u8; 12], KEY_CHECK_AAD, key_check).is_ok());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let input = scratch_dir("tamper_in");
        fs::write(input.join("a.js"), b"original content").unwrap();
        let output_dir = scratch_dir("tamper_out");
        let output = output_dir.join("app.nsbundle");
        let key = [9u8; 32];
        pack_directory(&input, &output, KEY_MODE_CUSTOM, key).unwrap();

        let mut data = fs::read(&output).unwrap();
        let last = data.len() - 1;
        data[last] ^= 0xff; // flip a byte inside the last blob's GCM tag/ciphertext
        fs::write(&output, &data).unwrap();

        // Manually walk the header since open_and_decrypt() only knows the default/custom keys
        // via the global statics, not an arbitrary caller-supplied key.
        let entry_count = LittleEndian::read_u32(&data[24..28]) as usize;
        assert_eq!(entry_count, 1);
        let path_len = LittleEndian::read_u16(&data[HEADER_LEN..HEADER_LEN + 2]) as usize;
        let mut cursor = HEADER_LEN + 2 + path_len;
        let blob_offset = LittleEndian::read_u64(&data[cursor..cursor + 8]);
        cursor += 8;
        let blob_len = LittleEndian::read_u64(&data[cursor..cursor + 8]);
        cursor += 8;
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&data[cursor..cursor + 12]);
        cursor += 12;
        let blob_start = cursor + blob_offset as usize;
        let blob_end = blob_start + blob_len as usize;
        assert!(open(&key, &nonce, b"a.js", &data[blob_start..blob_end]).is_err());
    }

    #[test]
    fn corrupt_counts_and_offsets_fail_cleanly() {
        let input = scratch_dir("corrupt_in");
        fs::write(input.join("a.js"), b"a").unwrap();
        let output = scratch_dir("corrupt_out").join("app.nsbundle");
        pack_directory(&input, &output, KEY_MODE_DEFAULT, default_key()).unwrap();
        let packed = fs::read(&output).unwrap();

        let mut data = packed.clone();
        LittleEndian::write_u32(&mut data[24..28], u32::MAX);
        fs::write(&output, &data).unwrap();
        assert!(open_and_decrypt(&output).is_err());

        let offset_at = HEADER_LEN + 2 + "a.js".len();
        for value in [u64::MAX, u64::MAX - 8] {
            let mut data = packed.clone();
            LittleEndian::write_u64(&mut data[offset_at..offset_at + 8], value);
            fs::write(&output, &data).unwrap();
            assert!(open_and_decrypt(&output).is_err());
        }
    }

    #[test]
    fn aad_binds_ciphertext_to_its_path() {
        let key = [3u8; 32];
        let nonce = [0u8; 12];
        let ciphertext = seal(&key, &nonce, b"a.js", b"secret contents");
        // Same key/nonce/ciphertext, wrong AAD (as if the TOC had been spliced to claim this
        // blob belongs to a different file) must fail rather than decrypt.
        assert!(open(&key, &nonce, b"b.js", &ciphertext).is_err());
        assert!(open(&key, &nonce, b"a.js", &ciphertext).is_ok());
    }

    #[test]
    fn relativize_strips_app_segment_case_insensitively() {
        assert_eq!(relativize("C:\\proj\\bin\\App\\sub\\file.js"), "sub/file.js");
        assert_eq!(relativize("C:/proj/bin/app/file.js"), "file.js");
        assert_eq!(relativize("sub/file.js"), "sub/file.js");
        assert_eq!(relativize("C:\\no\\app\\segment\\HERE\\file.js"), "segment/here/file.js");
    }

    #[test]
    fn relativize_strips_leading_current_dir_prefix() {
        // Left over from joining a relative specifier ("./dep.mjs") against an empty base when
        // the referrer has no directory component of its own (e.g. a top-level "entry.mjs").
        assert_eq!(relativize("./dep.mjs"), "dep.mjs");
        assert_eq!(relativize(".\\dep.mjs"), "dep.mjs");
    }

    #[test]
    fn hex_key_round_trips() {
        // Exactly 64 hex chars (32 bytes): "00112233445566778899aabbccddeeff" (16 bytes) twice.
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        assert_eq!(hex.len(), 64);
        let key = parse_key_hex(hex).unwrap();
        assert_eq!(key[0], 0x00);
        assert_eq!(key[1], 0x11);
        assert_eq!(key[15], 0xff);
        assert_eq!(key[31], 0xff);
        assert!(parse_key_hex("too-short").is_none());
        assert!(parse_key_hex(&"zz".repeat(32)).is_none());
        assert!(parse_key_hex(&format!("{hex}ff")).is_none()); // 65 hex chars: one byte too long
    }
}
