use super::*;

#[derive(Clone)]
enum M {
    Undefined,
    Str(String),
    Num(f64),
    Bool(bool),
    Bytes(Vec<u8>),
    Obj(Vec<(String, usize)>),
    Arr(Vec<usize>),
    Key(Key),
    Json(Value),
}

#[derive(Default)]
struct Mock(Vec<M>);

impl Mock {
    fn add(&mut self, v: M) -> usize {
        self.0.push(v);
        self.0.len() - 1
    }
    fn s(&mut self, v: &str) -> usize {
        self.add(M::Str(v.into()))
    }
    fn n(&mut self, v: f64) -> usize {
        self.add(M::Num(v))
    }
    fn b(&mut self, v: &[u8]) -> usize {
        self.add(M::Bytes(v.to_vec()))
    }
    fn t(&mut self, v: bool) -> usize {
        self.add(M::Bool(v))
    }
    fn obj(&mut self, members: &[(&str, usize)]) -> usize {
        self.add(M::Obj(members.iter().map(|(k, v)| (k.to_string(), *v)).collect()))
    }
    fn usages(&mut self, list: &[&str]) -> usize {
        let items = list.iter().map(|u| self.s(u)).collect();
        self.add(M::Arr(items))
    }
    fn call(&mut self, op: &str, args: &[usize]) -> Result<usize> {
        dispatch(self, op, args)
    }
    fn bytes_of(&self, v: usize) -> Vec<u8> {
        match &self.0[v] {
            M::Bytes(b) => b.clone(),
            _ => panic!("not bytes"),
        }
    }
    fn key_of(&self, v: usize) -> Key {
        match &self.0[v] {
            M::Key(k) => k.clone(),
            _ => panic!("not a key"),
        }
    }
}

impl Js for Mock {
    type V = usize;
    fn undefined(&mut self) -> usize {
        self.add(M::Undefined)
    }
    fn is_nullish(&mut self, v: usize) -> bool {
        matches!(self.0[v], M::Undefined)
    }
    fn string(&mut self, v: usize) -> Option<String> {
        match &self.0[v] {
            M::Str(s) => Some(s.clone()),
            _ => None,
        }
    }
    fn number(&mut self, v: usize) -> Option<f64> {
        match self.0[v] {
            M::Num(n) => Some(n),
            _ => None,
        }
    }
    fn boolean(&mut self, v: usize) -> bool {
        matches!(self.0[v], M::Bool(true))
    }
    fn bytes(&mut self, v: usize) -> Option<Vec<u8>> {
        match &self.0[v] {
            M::Bytes(b) => Some(b.clone()),
            _ => None,
        }
    }
    fn get(&mut self, v: usize, name: &str) -> usize {
        let found = match &self.0[v] {
            M::Obj(members) => members.iter().find(|(k, _)| k == name).map(|(_, v)| *v),
            _ => None,
        };
        found.unwrap_or_else(|| self.undefined())
    }
    fn array(&mut self, v: usize) -> Option<Vec<usize>> {
        match &self.0[v] {
            M::Arr(items) => Some(items.clone()),
            _ => None,
        }
    }
    fn stringify(&mut self, v: usize) -> Option<String> {
        match &self.0[v] {
            M::Json(j) => Some(j.to_string()),
            _ => None,
        }
    }
    fn key(&mut self, v: usize) -> Option<Key> {
        match &self.0[v] {
            M::Key(k) => Some(k.clone()),
            _ => None,
        }
    }
    fn array_buffer(&mut self, bytes: Vec<u8>) -> usize {
        self.add(M::Bytes(bytes))
    }
    fn bool_value(&mut self, b: bool) -> usize {
        self.add(M::Bool(b))
    }
    fn json(&mut self, value: &Value) -> usize {
        self.add(M::Json(value.clone()))
    }
    fn crypto_key(&mut self, key: Key) -> usize {
        self.add(M::Key(key))
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn digest_sha_family() {
    let mut js = Mock::default();
    let data = js.b(b"abc");
    for (name, expected) in [
        ("SHA-1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        ("sha-256", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        ("SHA-384", "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"),
        ("SHA-512", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
    ] {
        let alg = js.s(name);
        let out = js.call("digest", &[alg, data]).unwrap();
        assert_eq!(to_hex(&js.bytes_of(out)), expected, "{name}");
    }
    let dict = {
        let n = js.s("SHA-256");
        js.obj(&[("name", n)])
    };
    assert!(js.call("digest", &[dict, data]).is_ok());
    let bad = js.s("MD5");
    assert_eq!(js.call("digest", &[bad, data]).unwrap_err().name, "NotSupportedError");
}

fn hmac_alg(js: &mut Mock, hash: &str) -> usize {
    let name = js.s("HMAC");
    let hash = js.s(hash);
    js.obj(&[("name", name), ("hash", hash)])
}

#[test]
fn hmac_rfc4231_case_2() {
    let mut js = Mock::default();
    let format = js.s("raw");
    let secret = js.b(b"Jefe");
    let alg = hmac_alg(&mut js, "SHA-256");
    let extractable = js.t(true);
    let usages = js.usages(&["sign", "verify"]);
    let key = js.call("importKey", &[format, secret, alg, extractable, usages]).unwrap();
    assert_eq!(js.key_of(key).algorithm, Algorithm::Hmac { hash: Hash::Sha256, length: 32 });
    let data = js.b(b"what do ya want for nothing?");
    let sig = js.call("sign", &[alg, key, data]).unwrap();
    assert_eq!(
        to_hex(&js.bytes_of(sig)),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    let ok = js.call("verify", &[alg, key, sig, data]).unwrap();
    assert!(matches!(js.0[ok], M::Bool(true)));
    let other = js.b(b"tampered");
    let bad = js.call("verify", &[alg, key, sig, other]).unwrap();
    assert!(matches!(js.0[bad], M::Bool(false)));
}

#[test]
fn hmac_generate_and_jwk_round_trip() {
    let mut js = Mock::default();
    let alg = hmac_alg(&mut js, "SHA-512");
    let extractable = js.t(true);
    let usages = js.usages(&["sign"]);
    let key = js.call("generateKey", &[alg, extractable, usages]).unwrap();
    assert_eq!(js.key_of(key).secret.len(), 128);
    let jwk_format = js.s("jwk");
    let jwk = js.call("exportKey", &[jwk_format, key]).unwrap();
    let M::Json(value) = js.0[jwk].clone() else { panic!() };
    assert_eq!(value["alg"], "HS512");
    let imported = js.call("importKey", &[jwk_format, jwk, alg, extractable, usages]).unwrap();
    assert_eq!(js.key_of(imported).secret, js.key_of(key).secret);
}

#[test]
fn usages_are_checked() {
    let mut js = Mock::default();
    let alg = hmac_alg(&mut js, "SHA-256");
    let extractable = js.t(false);
    let usages = js.usages(&["encrypt"]);
    assert_eq!(js.call("generateKey", &[alg, extractable, usages]).unwrap_err().name, "SyntaxError");
    let none = js.usages(&[]);
    assert_eq!(js.call("generateKey", &[alg, extractable, none]).unwrap_err().name, "SyntaxError");
    let sign_only = js.usages(&["sign"]);
    let key = js.call("generateKey", &[alg, extractable, sign_only]).unwrap();
    let data = js.b(b"x");
    assert_eq!(js.call("verify", &[alg, key, data, data]).unwrap_err().name, "InvalidAccessError");
    let raw = js.s("raw");
    assert_eq!(js.call("exportKey", &[raw, key]).unwrap_err().name, "InvalidAccessError");
}

fn aes_alg(js: &mut Mock, iv: &[u8], aad: Option<&[u8]>) -> usize {
    let name = js.s("AES-GCM");
    let iv = js.b(iv);
    match aad {
        Some(a) => {
            let a = js.b(a);
            js.obj(&[("name", name), ("iv", iv), ("additionalData", a)])
        }
        None => js.obj(&[("name", name), ("iv", iv)]),
    }
}

#[test]
fn aes_gcm_nist_vector() {
    // GCM spec test case 4
    let mut js = Mock::default();
    let raw = js.s("raw");
    let secret = js.b(&hex("feffe9928665731c6d6a8f9467308308"));
    let name = js.s("AES-GCM");
    let import_alg = js.obj(&[("name", name)]);
    let extractable = js.t(false);
    let usages = js.usages(&["encrypt", "decrypt"]);
    let key = js.call("importKey", &[raw, secret, import_alg, extractable, usages]).unwrap();
    let alg = aes_alg(
        &mut js,
        &hex("cafebabefacedbaddecaf888"),
        Some(&hex("feedfacedeadbeeffeedfacedeadbeefabaddad2")),
    );
    let plain = hex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39");
    let data = js.b(&plain);
    let sealed = js.call("encrypt", &[alg, key, data]).unwrap();
    assert_eq!(
        to_hex(&js.bytes_of(sealed)),
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091\
5bc94fbc3221a5db94fae95ae7121a47"
    );
    let opened = js.call("decrypt", &[alg, key, sealed]).unwrap();
    assert_eq!(js.bytes_of(opened), plain);
    let mut tampered = js.bytes_of(sealed);
    tampered[0] ^= 1;
    let tampered = js.b(&tampered);
    assert_eq!(js.call("decrypt", &[alg, key, tampered]).unwrap_err().name, "OperationError");
}

#[test]
fn aes_gcm_limits() {
    let mut js = Mock::default();
    let name = js.s("AES-GCM");
    let l192 = js.n(192.0);
    let alg = js.obj(&[("name", name), ("length", l192)]);
    let extractable = js.t(true);
    let usages = js.usages(&["encrypt"]);
    assert_eq!(js.call("generateKey", &[alg, extractable, usages]).unwrap_err().name, "NotSupportedError");
    let l100 = js.n(100.0);
    let alg = js.obj(&[("name", name), ("length", l100)]);
    assert_eq!(js.call("generateKey", &[alg, extractable, usages]).unwrap_err().name, "OperationError");
    let l256 = js.n(256.0);
    let alg = js.obj(&[("name", name), ("length", l256)]);
    let key = js.call("generateKey", &[alg, extractable, usages]).unwrap();
    let short_iv = aes_alg(&mut js, &[0; 8], None);
    let data = js.b(b"x");
    assert_eq!(js.call("encrypt", &[short_iv, key, data]).unwrap_err().name, "NotSupportedError");
    let iv = aes_alg(&mut js, &[0; 12], None);
    assert_eq!(js.call("decrypt", &[iv, key, data]).unwrap_err().name, "InvalidAccessError");
}

fn base_key(js: &mut Mock, name: &str, secret: &[u8]) -> usize {
    let raw = js.s("raw");
    let secret = js.b(secret);
    let alg = js.s(name);
    let extractable = js.t(false);
    let usages = js.usages(&["deriveBits", "deriveKey"]);
    js.call("importKey", &[raw, secret, alg, extractable, usages]).unwrap()
}

#[test]
fn pbkdf2_rfc6070() {
    let mut js = Mock::default();
    let key = base_key(&mut js, "PBKDF2", b"password");
    let name = js.s("PBKDF2");
    let hash = js.s("SHA-1");
    let salt = js.b(b"salt");
    let iterations = js.n(4096.0);
    let alg = js.obj(&[("name", name), ("hash", hash), ("salt", salt), ("iterations", iterations)]);
    let length = js.n(160.0);
    let out = js.call("deriveBits", &[alg, key, length]).unwrap();
    assert_eq!(to_hex(&js.bytes_of(out)), "4b007901b765489abead49d926f721d065a429c1");
}

#[test]
fn hkdf_rfc5869_case_1() {
    let mut js = Mock::default();
    let key = base_key(&mut js, "HKDF", &hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b"));
    let name = js.s("HKDF");
    let hash = js.s("SHA-256");
    let salt = js.b(&hex("000102030405060708090a0b0c"));
    let info = js.b(&hex("f0f1f2f3f4f5f6f7f8f9"));
    let alg = js.obj(&[("name", name), ("hash", hash), ("salt", salt), ("info", info)]);
    let length = js.n(42.0 * 8.0);
    let out = js.call("deriveBits", &[alg, key, length]).unwrap();
    assert_eq!(
        to_hex(&js.bytes_of(out)),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
}

#[test]
fn derive_key_makes_an_aes_key() {
    let mut js = Mock::default();
    let key = base_key(&mut js, "PBKDF2", b"password");
    let name = js.s("PBKDF2");
    let hash = js.s("SHA-256");
    let salt = js.b(b"salt");
    let iterations = js.n(1000.0);
    let alg = js.obj(&[("name", name), ("hash", hash), ("salt", salt), ("iterations", iterations)]);
    let aes = js.s("AES-GCM");
    let length = js.n(256.0);
    let derived_type = js.obj(&[("name", aes), ("length", length)]);
    let extractable = js.t(true);
    let usages = js.usages(&["encrypt", "decrypt"]);
    let derived = js.call("deriveKey", &[alg, key, derived_type, extractable, usages]).unwrap();
    let derived = js.key_of(derived);
    assert_eq!(derived.algorithm, Algorithm::AesGcm { length: 256 });
    assert_eq!(derived.secret.len(), 32);
}

#[test]
fn base64url_round_trip() {
    for len in 0..8 {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 250) as u8).collect();
        assert_eq!(from_base64url(&base64url(&bytes)), Some(bytes));
    }
    assert_eq!(base64url(b"Jefe"), "SmVmZQ");
    assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
}

#[test]
fn unsupported_operations() {
    let mut js = Mock::default();
    let rsa = js.s("RSA-OAEP");
    assert_eq!(js.call("encrypt", &[rsa]).unwrap_err().name, "NotSupportedError");
    assert_eq!(js.call("wrapKey", &[]).unwrap_err().name, "NotSupportedError");
    let ecdsa = js.s("ECDSA");
    let extractable = js.t(true);
    let usages = js.usages(&["sign"]);
    assert_eq!(js.call("generateKey", &[ecdsa, extractable, usages]).unwrap_err().name, "NotSupportedError");
}
