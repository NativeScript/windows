//! CNG (bcrypt) primitives.

use super::Hash;
use windows::core::PCWSTR;
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Security::Cryptography::*;

fn ok(status: NTSTATUS) -> Result<(), ()> {
    status.is_ok().then_some(()).ok_or(())
}

fn hash_len(hash: Hash) -> usize {
    match hash {
        Hash::Sha1 => 20,
        Hash::Sha256 => 32,
        Hash::Sha384 => 48,
        Hash::Sha512 => 64,
    }
}

pub fn random(out: &mut [u8]) -> Result<(), ()> {
    ok(unsafe { BCryptGenRandom(None, out, BCRYPT_USE_SYSTEM_PREFERRED_RNG) })
}

pub fn digest(hash: Hash, data: &[u8]) -> Result<Vec<u8>, ()> {
    let algorithm = match hash {
        Hash::Sha1 => BCRYPT_SHA1_ALG_HANDLE,
        Hash::Sha256 => BCRYPT_SHA256_ALG_HANDLE,
        Hash::Sha384 => BCRYPT_SHA384_ALG_HANDLE,
        Hash::Sha512 => BCRYPT_SHA512_ALG_HANDLE,
    };
    let mut out = vec![0; hash_len(hash)];
    ok(unsafe { BCryptHash(algorithm, None, data, &mut out) })?;
    Ok(out)
}

pub fn hmac_sign(hash: Hash, secret: &[u8], data: &[u8]) -> Result<Vec<u8>, ()> {
    let algorithm = match hash {
        Hash::Sha1 => BCRYPT_HMAC_SHA1_ALG_HANDLE,
        Hash::Sha256 => BCRYPT_HMAC_SHA256_ALG_HANDLE,
        Hash::Sha384 => BCRYPT_HMAC_SHA384_ALG_HANDLE,
        Hash::Sha512 => BCRYPT_HMAC_SHA512_ALG_HANDLE,
    };
    let mut out = vec![0; hash_len(hash)];
    ok(unsafe { BCryptHash(algorithm, Some(secret), data, &mut out) })?;
    Ok(out)
}

pub fn hmac_verify(hash: Hash, secret: &[u8], data: &[u8], signature: &[u8]) -> bool {
    let Ok(expected) = hmac_sign(hash, secret, data) else {
        return false;
    };
    expected.len() == signature.len()
        && expected.iter().zip(signature).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

struct AesKey(BCRYPT_KEY_HANDLE);

impl AesKey {
    fn new(secret: &[u8]) -> Result<AesKey, ()> {
        let mut key = BCRYPT_KEY_HANDLE::default();
        ok(unsafe { BCryptGenerateSymmetricKey(BCRYPT_AES_GCM_ALG_HANDLE, &mut key, None, secret, 0) })?;
        Ok(AesKey(key))
    }
}

impl Drop for AesKey {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyKey(self.0);
        }
    }
}

fn mode_info(iv: &mut [u8], aad: &mut [u8], tag: &mut [u8]) -> BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
    BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
        dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
        pbNonce: iv.as_mut_ptr(),
        cbNonce: iv.len() as u32,
        pbAuthData: if aad.is_empty() { std::ptr::null_mut() } else { aad.as_mut_ptr() },
        cbAuthData: aad.len() as u32,
        pbTag: tag.as_mut_ptr(),
        cbTag: tag.len() as u32,
        ..Default::default()
    }
}

/// ciphertext followed by the 16-byte tag
pub fn aes_gcm_seal(secret: &[u8], iv: &[u8], aad: &[u8], data: &[u8]) -> Result<Vec<u8>, ()> {
    let key = AesKey::new(secret)?;
    let (mut iv, mut aad, mut tag) = (iv.to_vec(), aad.to_vec(), [0u8; 16]);
    let info = mode_info(&mut iv, &mut aad, &mut tag);
    let mut out = vec![0; data.len()];
    let mut written = 0u32;
    ok(unsafe {
        BCryptEncrypt(key.0, Some(data), Some(&info as *const _ as *const _), None, Some(&mut out), &mut written, BCRYPT_FLAGS(0))
    })?;
    out.truncate(written as usize);
    out.extend_from_slice(&tag);
    Ok(out)
}

pub fn aes_gcm_open(secret: &[u8], iv: &[u8], aad: &[u8], data: &[u8]) -> Result<Vec<u8>, ()> {
    let Some(split) = data.len().checked_sub(16) else {
        return Err(());
    };
    let key = AesKey::new(secret)?;
    let (mut iv, mut aad) = (iv.to_vec(), aad.to_vec());
    let mut tag = data[split..].to_vec();
    let info = mode_info(&mut iv, &mut aad, &mut tag);
    let mut out = vec![0; split];
    let mut written = 0u32;
    ok(unsafe {
        BCryptDecrypt(key.0, Some(&data[..split]), Some(&info as *const _ as *const _), None, Some(&mut out), &mut written, BCRYPT_FLAGS(0))
    })?;
    out.truncate(written as usize);
    Ok(out)
}

pub fn pbkdf2(hash: Hash, iterations: u32, salt: &[u8], secret: &[u8], out: &mut [u8]) -> Result<(), ()> {
    let id = match hash {
        Hash::Sha1 => BCRYPT_SHA1_ALGORITHM,
        Hash::Sha256 => BCRYPT_SHA256_ALGORITHM,
        Hash::Sha384 => BCRYPT_SHA384_ALGORITHM,
        Hash::Sha512 => BCRYPT_SHA512_ALGORITHM,
    };
    let mut prf = BCRYPT_ALG_HANDLE::default();
    ok(unsafe { BCryptOpenAlgorithmProvider(&mut prf, id, PCWSTR::null(), BCRYPT_ALG_HANDLE_HMAC_FLAG) })?;
    let status = unsafe { BCryptDeriveKeyPBKDF2(prf, Some(secret), Some(salt), iterations as u64, out, 0) };
    unsafe {
        let _ = BCryptCloseAlgorithmProvider(prf, 0);
    }
    ok(status)
}

/// RFC 5869 over CNG HMAC
pub fn hkdf(hash: Hash, salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) -> Result<(), ()> {
    let len = hash_len(hash);
    if out.len() > 255 * len {
        return Err(());
    }
    let zeros = vec![0; len];
    let prk = hmac_sign(hash, if salt.is_empty() { &zeros } else { salt }, ikm)?;
    let mut t = Vec::new();
    for (i, chunk) in out.chunks_mut(len).enumerate() {
        let mut input = std::mem::take(&mut t);
        input.extend_from_slice(info);
        input.push(i as u8 + 1);
        t = hmac_sign(hash, &prk, &input)?;
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    Ok(())
}
