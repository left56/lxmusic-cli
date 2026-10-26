//! `lx.utils.crypto` / `zlib`。AES 对齐 Node `createCipheriv` + PKCS7。
//! RSA 对齐 `RSA_NO_PADDING`：输入左侧补零到模数字节数（1024 位密钥即 128 字节）。

use anyhow::{anyhow, bail, Result};
use cipher::block_padding::Pkcs7;
use cipher::{BlockCipher, BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit};
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use md5::{Digest, Md5};
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPublicKey};
use std::io::{Read, Write};

const MAX_INFLATE: usize = 64 * 1024 * 1024;
const MAX_RANDOM: usize = 64 * 1024 * 1024;

pub fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn random_bytes(size: usize) -> Result<Vec<u8>> {
    if size > MAX_RANDOM {
        bail!("randomBytes size exceeds maximum of {MAX_RANDOM} bytes");
    }
    let mut buf = vec![0u8; size];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("randomBytes: {e}"))?;
    Ok(buf)
}

pub fn aes_encrypt(mode: &str, key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mode = mode.trim().to_ascii_lowercase().replace('_', "-");
    let (bits, ecb) = match mode.as_str() {
        "aes-128-ecb" => (128, true),
        "aes-192-ecb" => (192, true),
        "aes-256-ecb" => (256, true),
        "aes-128-cbc" => (128, false),
        "aes-192-cbc" => (192, false),
        "aes-256-cbc" => (256, false),
        other => bail!("unsupported aes mode {other}"),
    };
    let expect = bits / 8;
    if key.len() != expect {
        bail!("aes key length is {}, expected {expect}", key.len());
    }
    if ecb {
        return match bits {
            128 => ecb_encrypt::<aes::Aes128>(key, data),
            192 => ecb_encrypt::<aes::Aes192>(key, data),
            _ => ecb_encrypt::<aes::Aes256>(key, data),
        };
    }
    if iv.len() != 16 {
        bail!("aes-cbc iv must be 16 bytes, got {}", iv.len());
    }
    match bits {
        128 => cbc_encrypt::<aes::Aes128>(key, iv, data),
        192 => cbc_encrypt::<aes::Aes192>(key, iv, data),
        _ => cbc_encrypt::<aes::Aes256>(key, iv, data),
    }
}

fn cbc_encrypt<C>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: BlockEncrypt + BlockEncryptMut + BlockCipher + KeyInit,
    cbc::Encryptor<C>: KeyIvInit + BlockEncryptMut,
{
    let enc = cbc::Encryptor::<C>::new_from_slices(key, iv)
        .map_err(|e| anyhow!("aes-cbc init: {e}"))?;
    Ok(enc.encrypt_padded_vec_mut::<Pkcs7>(data))
}

fn ecb_encrypt<C>(key: &[u8], data: &[u8]) -> Result<Vec<u8>>
where
    C: BlockEncrypt + BlockEncryptMut + BlockCipher + KeyInit,
    ecb::Encryptor<C>: KeyInit + BlockEncryptMut,
{
    let enc = ecb::Encryptor::<C>::new_from_slice(key).map_err(|e| anyhow!("aes-ecb init: {e}"))?;
    Ok(enc.encrypt_padded_vec_mut::<Pkcs7>(data))
}

pub fn aes_decrypt_cbc(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let dec = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
        .map_err(|e| anyhow!("aes-cbc decrypt init: {e}"))?;
    dec.decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|e| anyhow!("aes-cbc decrypt: {e}"))
}

/// 左补零后做无填充 RSA。1024 位公钥的模数是 128 字节，与觅音固定补到 128 的结果相同。
pub fn rsa_encrypt_no_padding(pem_or_key: &str, data: &[u8]) -> Result<Vec<u8>> {
    let pem = normalize_pem(pem_or_key);
    let key = RsaPublicKey::from_public_key_pem(&pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(&pem))
        .map_err(|e| anyhow!("Failed to parse RSA public key: {e}"))?;
    let n = key.n().clone();
    let k = ((n.bits() + 7) / 8) as usize;
    if k == 0 {
        bail!("invalid RSA modulus");
    }
    if data.len() > k {
        bail!("rsaEncrypt input exceeds RSA block size of {k} bytes");
    }
    // 觅音始终先补到 128。模数正好 128 时这就是 Node 要的块；其它长度则补到模数，
    // 否则 Node 的 RSA_NO_PADDING 会直接拒绝。
    let pad_to = if k == 128 { 128 } else { k };
    if data.len() > pad_to {
        bail!("rsaEncrypt input exceeds RSA block size of {pad_to} bytes");
    }
    let mut padded = vec![0u8; pad_to - data.len()];
    padded.extend_from_slice(data);
    if padded.len() != k {
        bail!("rsaEncrypt padded length {} != modulus {k}", padded.len());
    }
    let m = BigUint::from_bytes_be(&padded);
    let c = m.modpow(key.e(), &n);
    let cb = c.to_bytes_be();
    if cb.len() > k {
        bail!("rsa ciphertext overflow");
    }
    let mut out = vec![0u8; k - cb.len()];
    out.extend_from_slice(&cb);
    Ok(out)
}

fn normalize_pem(raw: &str) -> String {
    let t = raw.trim().trim_matches('"').trim();
    if t.contains("-----BEGIN") && t.contains("\\n") {
        t.replace("\\n", "\n")
    } else {
        t.to_string()
    }
}

pub fn zlib_deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

pub fn zlib_inflate(data: &[u8]) -> Result<Vec<u8>> {
    if let Ok(v) = read_limited(ZlibDecoder::new(data)) {
        return Ok(v);
    }
    if let Ok(v) = read_limited(GzDecoder::new(data)) {
        return Ok(v);
    }
    read_limited(DeflateDecoder::new(data)).map_err(|e| anyhow!("inflate failed: {e}"))
}

fn read_limited<R: Read>(mut reader: R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    reader
        .by_ref()
        .take(MAX_INFLATE as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > MAX_INFLATE {
        bail!("inflate output exceeds maximum of {MAX_INFLATE} bytes");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hello() {
        assert_eq!(md5_hex(b"hello"), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn aes_cbc_pkcs7_roundtrip_and_ecb_ignores_iv() {
        let key = b"0123456789abcdef";
        let iv = b"abcdef0123456789";
        let plain = b"hello lx-music!!";
        let ct = aes_encrypt("AES-128-CBC", key, iv, plain).unwrap();
        assert_eq!(ct.len() % 16, 0);
        assert_eq!(aes_decrypt_cbc(key, iv, &ct).unwrap(), plain);

        let ecb1 = aes_encrypt("aes-128-ecb", key, b"", plain).unwrap();
        let ecb2 = aes_encrypt("aes-128-ecb", key, iv, plain).unwrap();
        assert_eq!(ecb1, ecb2);
        assert_ne!(ecb1, ct);
    }

    #[test]
    fn zlib_roundtrip() {
        let raw = b"hello world hello world";
        let c = zlib_deflate(raw).unwrap();
        assert_eq!(zlib_inflate(&c).unwrap(), raw);
    }

    #[test]
    fn rsa_no_padding_is_raw_modexp_on_1024() {
        use rand::rngs::OsRng;
        use rsa::pkcs8::{EncodePublicKey, LineEnding};
        use rsa::traits::PrivateKeyParts;
        use rsa::RsaPrivateKey;

        let privk = RsaPrivateKey::new(&mut OsRng, 1024).expect("keygen");
        let pem = rsa::RsaPublicKey::from(&privk)
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        let data = b"hello-lx";
        let ct = rsa_encrypt_no_padding(&pem, data).unwrap();
        assert_eq!(ct.len(), 128);
        let m = BigUint::from_bytes_be(&ct).modpow(privk.d(), privk.n());
        let mb = m.to_bytes_be();
        let mut padded = vec![0u8; 128 - mb.len()];
        padded.extend_from_slice(&mb);
        assert_eq!(&padded[128 - data.len()..], data);
        assert!(padded[..128 - data.len()].iter().all(|b| *b == 0));
    }
}
