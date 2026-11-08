//! `crypto.subtle`: digest, HMAC, AES-GCM, PBKDF2, HKDF. Primitives are in `backend`.

mod backend;

use serde_json::{json, Map, Value};

#[derive(Debug, PartialEq)]
pub struct Error {
    pub name: &'static str,
    pub message: String,
}

type Result<T> = std::result::Result<T, Error>;

fn err<T>(name: &'static str, message: impl Into<String>) -> Result<T> {
    Err(Error { name, message: message.into() })
}

fn operation_failed() -> Error {
    Error { name: "OperationError", message: "The operation failed for an operation-specific reason".into() }
}

fn not_supported<T>(what: &str) -> Result<T> {
    err("NotSupportedError", format!("{what} is not supported"))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn parse(name: &str) -> Result<Hash> {
        Ok(match name.to_ascii_uppercase().as_str() {
            "SHA-1" => Hash::Sha1,
            "SHA-256" => Hash::Sha256,
            "SHA-384" => Hash::Sha384,
            "SHA-512" => Hash::Sha512,
            _ => return not_supported(&format!("Hash '{name}'")),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Hash::Sha1 => "SHA-1",
            Hash::Sha256 => "SHA-256",
            Hash::Sha384 => "SHA-384",
            Hash::Sha512 => "SHA-512",
        }
    }

    fn block_bits(self) -> u32 {
        match self {
            Hash::Sha1 | Hash::Sha256 => 512,
            Hash::Sha384 | Hash::Sha512 => 1024,
        }
    }

    fn jwk_hmac(self) -> &'static str {
        match self {
            Hash::Sha1 => "HS1",
            Hash::Sha256 => "HS256",
            Hash::Sha384 => "HS384",
            Hash::Sha512 => "HS512",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Algorithm {
    Hmac { hash: Hash, length: u32 },
    AesGcm { length: u32 },
    Pbkdf2,
    Hkdf,
}

impl Algorithm {
    fn name(&self) -> &'static str {
        match self {
            Algorithm::Hmac { .. } => "HMAC",
            Algorithm::AesGcm { .. } => "AES-GCM",
            Algorithm::Pbkdf2 => "PBKDF2",
            Algorithm::Hkdf => "HKDF",
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Algorithm::Hmac { hash, length } => {
                json!({ "name": "HMAC", "hash": { "name": hash.name() }, "length": length })
            }
            Algorithm::AesGcm { length } => json!({ "name": "AES-GCM", "length": length }),
            _ => json!({ "name": self.name() }),
        }
    }

    fn allowed_usages(&self) -> &'static [&'static str] {
        match self {
            Algorithm::Hmac { .. } => &["sign", "verify"],
            Algorithm::AesGcm { .. } => &["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            Algorithm::Pbkdf2 | Algorithm::Hkdf => &["deriveBits", "deriveKey"],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Key {
    pub algorithm: Algorithm,
    pub extractable: bool,
    pub usages: Vec<String>,
    secret: Vec<u8>,
}

impl Key {
    pub fn kind(&self) -> &'static str {
        "secret"
    }

    /// algorithm JSON, NUL, secret
    pub fn encode(&self) -> Vec<u8> {
        let header = json!({ "algorithm": self.algorithm.to_json(), "extractable": self.extractable, "usages": self.usages });
        let mut out = header.to_string().into_bytes();
        out.push(0);
        out.extend_from_slice(&self.secret);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Key> {
        let split = bytes.iter().position(|b| *b == 0)?;
        let header: Value = serde_json::from_slice(&bytes[..split]).ok()?;
        let alg = &header["algorithm"];
        let algorithm = match alg["name"].as_str()? {
            "HMAC" => Algorithm::Hmac {
                hash: Hash::parse(alg["hash"]["name"].as_str()?).ok()?,
                length: alg["length"].as_u64()? as u32,
            },
            "AES-GCM" => Algorithm::AesGcm { length: alg["length"].as_u64()? as u32 },
            "PBKDF2" => Algorithm::Pbkdf2,
            "HKDF" => Algorithm::Hkdf,
            _ => return None,
        };
        Some(Key {
            algorithm,
            extractable: header["extractable"].as_bool()?,
            usages: serde_json::from_value(header["usages"].clone()).ok()?,
            secret: bytes[split + 1..].to_vec(),
        })
    }

    fn require(&self, algorithm: &str, usage: &str) -> Result<()> {
        if self.algorithm.name() != algorithm {
            return err("InvalidAccessError", format!("The key is not a {algorithm} key"));
        }
        if !self.usages.iter().any(|u| u == usage) {
            return err("InvalidAccessError", format!("The key does not support '{usage}'"));
        }
        Ok(())
    }
}

pub trait Js {
    type V: Copy;
    fn undefined(&mut self) -> Self::V;
    fn is_nullish(&mut self, v: Self::V) -> bool;
    fn string(&mut self, v: Self::V) -> Option<String>;
    fn number(&mut self, v: Self::V) -> Option<f64>;
    fn boolean(&mut self, v: Self::V) -> bool;
    fn bytes(&mut self, v: Self::V) -> Option<Vec<u8>>;
    /// undefined for non-objects
    fn get(&mut self, v: Self::V, name: &str) -> Self::V;
    fn array(&mut self, v: Self::V) -> Option<Vec<Self::V>>;
    fn stringify(&mut self, v: Self::V) -> Option<String>;
    fn key(&mut self, v: Self::V) -> Option<Key>;

    fn array_buffer(&mut self, bytes: Vec<u8>) -> Self::V;
    fn bool_value(&mut self, b: bool) -> Self::V;
    fn json(&mut self, value: &Value) -> Self::V;
    fn crypto_key(&mut self, key: Key) -> Self::V;
}

fn type_error<T>(message: impl Into<String>) -> Result<T> {
    err("TypeError", message)
}

fn arg<J: Js>(args: &[J::V], js: &mut J, i: usize) -> J::V {
    args.get(i).copied().unwrap_or_else(|| js.undefined())
}

fn data<J: Js>(js: &mut J, v: J::V, what: &str) -> Result<Vec<u8>> {
    js.bytes(v).map_or_else(|| type_error(format!("{what} is not a BufferSource")), Ok)
}

fn algorithm_name<J: Js>(js: &mut J, v: J::V) -> Result<String> {
    if let Some(name) = js.string(v) {
        return Ok(name);
    }
    let name = js.get(v, "name");
    js.string(name).map_or_else(|| type_error("Algorithm: name is missing"), Ok)
}

fn canonical(name: &str) -> String {
    name.to_ascii_uppercase()
}

fn member_hash<J: Js>(js: &mut J, alg: J::V) -> Result<Hash> {
    let hash = js.get(alg, "hash");
    if js.is_nullish(hash) {
        return type_error("Algorithm: hash is missing");
    }
    Hash::parse(&algorithm_name(js, hash)?)
}

fn member_bytes<J: Js>(js: &mut J, alg: J::V, name: &str) -> Result<Option<Vec<u8>>> {
    let v = js.get(alg, name);
    if js.is_nullish(v) {
        return Ok(None);
    }
    data(js, v, &format!("Algorithm: {name}")).map(Some)
}

fn member_u32<J: Js>(js: &mut J, alg: J::V, name: &str) -> Result<Option<u32>> {
    let v = js.get(alg, name);
    if js.is_nullish(v) {
        return Ok(None);
    }
    match js.number(v) {
        Some(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => Ok(Some(n as u32)),
        _ => type_error(format!("Algorithm: {name} is not a valid unsigned long")),
    }
}

fn usages<J: Js>(js: &mut J, v: J::V, allowed: &[&str]) -> Result<Vec<String>> {
    let Some(items) = js.array(v) else {
        return type_error("keyUsages is not a sequence");
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let usage = js.string(item).unwrap_or_default();
        if !["encrypt", "decrypt", "sign", "verify", "deriveKey", "deriveBits", "wrapKey", "unwrapKey"]
            .contains(&usage.as_str())
        {
            return type_error(format!("'{usage}' is not a valid KeyUsage"));
        }
        if !allowed.contains(&usage.as_str()) {
            return err("SyntaxError", format!("Usage '{usage}' is not allowed for this key"));
        }
        if !out.contains(&usage) {
            out.push(usage);
        }
    }
    Ok(out)
}

fn random(len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0; len];
    backend::random(&mut out)
        .map_err(|_| Error { name: "OperationError", message: "the random source failed".into() })?;
    Ok(out)
}

fn aes_length(bits: u32) -> Result<u32> {
    match bits {
        128 | 256 => Ok(bits),
        192 => not_supported("AES-192"),
        _ => err("OperationError", format!("AES key length must be 128 or 256 bits, got {bits}")),
    }
}

fn hmac_length<J: Js>(js: &mut J, alg: J::V) -> Result<Option<u32>> {
    match member_u32(js, alg, "length")? {
        Some(0) => type_error("HMAC: length must not be zero"),
        length => Ok(length),
    }
}

fn key_algorithm<J: Js>(js: &mut J, alg: J::V, op: &str) -> Result<(Algorithm, Option<u32>)> {
    let name = canonical(&algorithm_name(js, alg)?);
    Ok(match name.as_str() {
        "HMAC" => {
            let hash = member_hash(js, alg)?;
            let length = hmac_length(js, alg)?;
            (Algorithm::Hmac { hash, length: length.unwrap_or(hash.block_bits()) }, length)
        }
        "AES-GCM" => {
            let length = member_u32(js, alg, "length")?;
            let length = match (length, op) {
                (Some(n), _) => Some(aes_length(n)?),
                (None, "importKey") => None,
                (None, _) => return type_error("AES-GCM: length is missing"),
            };
            (Algorithm::AesGcm { length: length.unwrap_or(0) }, length)
        }
        "PBKDF2" if op == "importKey" => (Algorithm::Pbkdf2, None),
        "HKDF" if op == "importKey" => (Algorithm::Hkdf, None),
        _ => return not_supported(&format!("{op} with '{name}'")),
    })
}

fn generate_key<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let alg = arg(args, js, 0);
    let (algorithm, _) = key_algorithm(js, alg, "generateKey")?;
    let extractable = {
        let v = arg(args, js, 1);
        js.boolean(v)
    };
    let usages_v = arg(args, js, 2);
    let usages = usages(js, usages_v, algorithm.allowed_usages())?;
    if usages.is_empty() {
        return err("SyntaxError", "keyUsages must not be empty");
    }
    let bits = match &algorithm {
        Algorithm::Hmac { length, .. } | Algorithm::AesGcm { length } => *length,
        _ => unreachable!(),
    };
    let secret = random(bits.div_ceil(8) as usize)?;
    Ok(js.crypto_key(Key { algorithm, extractable, usages, secret }))
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(BASE64URL[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn from_base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        acc = acc << 6 | BASE64URL.iter().position(|b| *b == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn import_key<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let format_v = arg(args, js, 0);
    let format = js.string(format_v).unwrap_or_default();
    let key_data = arg(args, js, 1);
    let alg = arg(args, js, 2);
    let (mut algorithm, length) = key_algorithm(js, alg, "importKey")?;
    let extractable = {
        let v = arg(args, js, 3);
        js.boolean(v)
    };
    let usages_v = arg(args, js, 4);
    let usages = usages(js, usages_v, algorithm.allowed_usages())?;

    let secret = match format.as_str() {
        "raw" => data(js, key_data, "keyData")?,
        "jwk" => {
            if matches!(algorithm, Algorithm::Pbkdf2 | Algorithm::Hkdf) {
                return not_supported(&format!("jwk import of {}", algorithm.name()));
            }
            let text = js.stringify(key_data).unwrap_or_default();
            let jwk: Value = serde_json::from_str(&text).map_err(|_| Error {
                name: "DataError",
                message: "keyData is not a JsonWebKey".into(),
            })?;
            jwk_secret(&jwk, &algorithm, &usages, extractable)?
        }
        "spki" | "pkcs8" => return not_supported(&format!("'{format}' for secret keys")),
        _ => return type_error(format!("'{format}' is not a valid KeyFormat")),
    };
    if matches!(algorithm, Algorithm::Pbkdf2 | Algorithm::Hkdf) && extractable {
        return err("SyntaxError", format!("{} keys can't be extractable", algorithm.name()));
    }
    let bits = secret.len() as u32 * 8;
    match &mut algorithm {
        Algorithm::Hmac { length: l, .. } => {
            if bits == 0 {
                return err("DataError", "HMAC key data must not be empty");
            }
            match length {
                Some(n) if n > bits || n <= bits - 8 => {
                    return err("DataError", "HMAC length doesn't match the key data")
                }
                Some(n) => *l = n,
                None => *l = bits,
            }
        }
        Algorithm::AesGcm { length: l } => {
            aes_length(bits).map_err(|_| Error {
                name: "DataError",
                message: format!("AES key data must be 16 or 32 bytes, got {}", secret.len()),
            })?;
            if length.is_some_and(|n| n != bits) {
                return err("DataError", "AES-GCM length doesn't match the key data");
            }
            *l = bits;
        }
        _ => {}
    }
    if usages.is_empty() {
        return err("SyntaxError", "keyUsages must not be empty");
    }
    Ok(js.crypto_key(Key { algorithm, extractable, usages, secret }))
}

fn jwk_secret(jwk: &Value, algorithm: &Algorithm, usages: &[String], extractable: bool) -> Result<Vec<u8>> {
    let data_error = |m: &str| Error { name: "DataError", message: m.into() };
    if jwk["kty"] != "oct" {
        return Err(data_error("JWK kty must be 'oct'"));
    }
    let k = jwk["k"].as_str().ok_or_else(|| data_error("JWK k is missing"))?;
    let secret = from_base64url(k).ok_or_else(|| data_error("JWK k is not base64url"))?;
    let expected = match algorithm {
        Algorithm::Hmac { hash, .. } => hash.jwk_hmac().to_string(),
        Algorithm::AesGcm { .. } => format!("A{}GCM", secret.len() * 8),
        _ => unreachable!(),
    };
    if let Some(alg) = jwk.get("alg").and_then(Value::as_str) {
        if alg != expected {
            return Err(data_error(&format!("JWK alg '{alg}' doesn't match {expected}")));
        }
    }
    if let Some(use_) = jwk.get("use").and_then(Value::as_str) {
        let expected = if matches!(algorithm, Algorithm::Hmac { .. }) { "sig" } else { "enc" };
        if !usages.is_empty() && use_ != expected {
            return Err(data_error(&format!("JWK use must be '{expected}'")));
        }
    }
    if let Some(ops) = jwk.get("key_ops").and_then(Value::as_array) {
        if usages.iter().any(|u| !ops.iter().any(|o| o == u.as_str())) {
            return Err(data_error("keyUsages are not a subset of the JWK key_ops"));
        }
    }
    if extractable && jwk.get("ext") == Some(&Value::Bool(false)) {
        return Err(data_error("The JWK is not extractable"));
    }
    Ok(secret)
}

fn key_arg<J: Js>(js: &mut J, args: &[J::V], i: usize) -> Result<Key> {
    let v = arg(args, js, i);
    js.key(v).map_or_else(|| type_error("key is not a CryptoKey"), Ok)
}

fn export_key<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let format_v = arg(args, js, 0);
    let format = js.string(format_v).unwrap_or_default();
    let key = key_arg(js, args, 1)?;
    if !key.extractable {
        return err("InvalidAccessError", "The key is not extractable");
    }
    if matches!(key.algorithm, Algorithm::Pbkdf2 | Algorithm::Hkdf) {
        return not_supported(&format!("Exporting {} keys", key.algorithm.name()));
    }
    match format.as_str() {
        "raw" => Ok(js.array_buffer(key.secret)),
        "jwk" => {
            let alg = match key.algorithm {
                Algorithm::Hmac { hash, .. } => hash.jwk_hmac().to_string(),
                Algorithm::AesGcm { length } => format!("A{length}GCM"),
                _ => unreachable!(),
            };
            let mut jwk = Map::new();
            jwk.insert("kty".into(), "oct".into());
            jwk.insert("k".into(), base64url(&key.secret).into());
            jwk.insert("alg".into(), alg.into());
            jwk.insert("key_ops".into(), key.usages.clone().into());
            jwk.insert("ext".into(), true.into());
            Ok(js.json(&Value::Object(jwk)))
        }
        "spki" | "pkcs8" => not_supported(&format!("'{format}' for secret keys")),
        _ => type_error(format!("'{format}' is not a valid KeyFormat")),
    }
}

fn hmac_hash(key: &Key) -> Hash {
    let Algorithm::Hmac { hash, .. } = key.algorithm else { unreachable!() };
    hash
}

fn sign<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let alg = arg(args, js, 0);
    let name = canonical(&algorithm_name(js, alg)?);
    if name != "HMAC" {
        return not_supported(&format!("sign with '{name}'"));
    }
    let key = key_arg(js, args, 1)?;
    key.require("HMAC", "sign")?;
    let d = arg(args, js, 2);
    let message = data(js, d, "data")?;
    let tag = backend::hmac_sign(hmac_hash(&key), &key.secret, &message).map_err(|_| operation_failed())?;
    Ok(js.array_buffer(tag))
}

fn verify<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let alg = arg(args, js, 0);
    let name = canonical(&algorithm_name(js, alg)?);
    if name != "HMAC" {
        return not_supported(&format!("verify with '{name}'"));
    }
    let key = key_arg(js, args, 1)?;
    key.require("HMAC", "verify")?;
    let s = arg(args, js, 2);
    let signature = data(js, s, "signature")?;
    let d = arg(args, js, 3);
    let message = data(js, d, "data")?;
    let ok = backend::hmac_verify(hmac_hash(&key), &key.secret, &message, &signature);
    Ok(js.bool_value(ok))
}

fn aes_gcm<J: Js>(js: &mut J, args: &[J::V], encrypt: bool) -> Result<J::V> {
    let op = if encrypt { "encrypt" } else { "decrypt" };
    let alg = arg(args, js, 0);
    let name = canonical(&algorithm_name(js, alg)?);
    if name != "AES-GCM" {
        return not_supported(&format!("{op} with '{name}'"));
    }
    let key = key_arg(js, args, 1)?;
    key.require("AES-GCM", op)?;
    let Some(iv) = member_bytes(js, alg, "iv")? else {
        return type_error("AES-GCM: iv is missing");
    };
    let aad = member_bytes(js, alg, "additionalData")?.unwrap_or_default();
    match member_u32(js, alg, "tagLength")? {
        None | Some(128) => {}
        Some(32 | 64 | 96 | 104 | 112 | 120) => return not_supported("An AES-GCM tagLength other than 128"),
        Some(n) => return err("OperationError", format!("{n} is not a valid AES-GCM tagLength")),
    }
    if iv.len() != 12 {
        return not_supported("An AES-GCM iv that isn't 12 bytes");
    }
    let d = arg(args, js, 2);
    let input = data(js, d, "data")?;
    let output = if encrypt {
        backend::aes_gcm_seal(&key.secret, &iv, &aad, &input)
    } else {
        backend::aes_gcm_open(&key.secret, &iv, &aad, &input)
    };
    let buffer = output.map_err(|_| operation_failed())?;
    Ok(js.array_buffer(buffer))
}

fn derive<J: Js>(js: &mut J, alg: J::V, key: &Key, usage: &str, bits: Option<u32>) -> Result<Vec<u8>> {
    let name = canonical(&algorithm_name(js, alg)?);
    let Some(bits) = bits else {
        return err("OperationError", "length is required");
    };
    if bits % 8 != 0 {
        return err("OperationError", "length must be a multiple of 8");
    }
    if bits == 0 {
        return err("OperationError", "length must not be zero");
    }
    let mut out = vec![0; bits as usize / 8];
    match name.as_str() {
        "PBKDF2" => {
            key.require("PBKDF2", usage)?;
            let hash = member_hash(js, alg)?;
            let Some(salt) = member_bytes(js, alg, "salt")? else {
                return type_error("PBKDF2: salt is missing");
            };
            let Some(iterations) = member_u32(js, alg, "iterations")? else {
                return type_error("PBKDF2: iterations is missing");
            };
            if iterations == 0 {
                return err("OperationError", "PBKDF2: iterations must not be zero");
            }
            backend::pbkdf2(hash, iterations, &salt, &key.secret, &mut out)
                .map_err(|_| Error { name: "OperationError", message: "PBKDF2 failed".into() })?;
        }
        "HKDF" => {
            key.require("HKDF", usage)?;
            let hash = member_hash(js, alg)?;
            let Some(salt) = member_bytes(js, alg, "salt")? else {
                return type_error("HKDF: salt is missing");
            };
            let Some(info) = member_bytes(js, alg, "info")? else {
                return type_error("HKDF: info is missing");
            };
            backend::hkdf(hash, &salt, &key.secret, &info, &mut out)
                .map_err(|_| Error { name: "OperationError", message: "HKDF: length is too large".into() })?;
        }
        _ => return not_supported(&format!("deriving with '{name}'")),
    }
    Ok(out)
}

fn derive_bits<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let alg = arg(args, js, 0);
    let key = key_arg(js, args, 1)?;
    let length_v = arg(args, js, 2);
    let bits = if js.is_nullish(length_v) {
        None
    } else {
        js.number(length_v).filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u32)
    };
    let out = derive(js, alg, &key, "deriveBits", bits)?;
    Ok(js.array_buffer(out))
}

fn derive_key<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let alg = arg(args, js, 0);
    let key = key_arg(js, args, 1)?;
    let derived = arg(args, js, 2);
    let (mut algorithm, length) = key_algorithm(js, derived, "deriveKey")?;
    let extractable = {
        let v = arg(args, js, 3);
        js.boolean(v)
    };
    let usages_v = arg(args, js, 4);
    let usages = usages(js, usages_v, algorithm.allowed_usages())?;
    if usages.is_empty() {
        return err("SyntaxError", "keyUsages must not be empty");
    }
    let bits = match &mut algorithm {
        Algorithm::Hmac { hash, length: l } => {
            *l = length.unwrap_or(hash.block_bits());
            *l
        }
        Algorithm::AesGcm { length } => *length,
        _ => unreachable!(),
    };
    let secret = derive(js, alg, &key, "deriveKey", Some(bits.div_ceil(8) * 8))?;
    Ok(js.crypto_key(Key { algorithm, extractable, usages, secret }))
}

fn digest_op<J: Js>(js: &mut J, args: &[J::V]) -> Result<J::V> {
    let alg = arg(args, js, 0);
    let hash = Hash::parse(&algorithm_name(js, alg)?)?;
    let d = arg(args, js, 1);
    let message = data(js, d, "data")?;
    let out = backend::digest(hash, &message).map_err(|_| operation_failed())?;
    Ok(js.array_buffer(out))
}

pub fn dispatch<J: Js>(js: &mut J, op: &str, args: &[J::V]) -> Result<J::V> {
    match op {
        "digest" => digest_op(js, args),
        "generateKey" => generate_key(js, args),
        "importKey" => import_key(js, args),
        "exportKey" => export_key(js, args),
        "sign" => sign(js, args),
        "verify" => verify(js, args),
        "encrypt" => aes_gcm(js, args, true),
        "decrypt" => aes_gcm(js, args, false),
        "deriveBits" => derive_bits(js, args),
        "deriveKey" => derive_key(js, args),
        _ => not_supported(&format!("crypto.subtle.{op}")),
    }
}

#[cfg(test)]
mod tests;
