//! The ciphers GameStream uses, all keyed with AES-128:
//! - pairing: AES-128-ECB with no padding, under a key from the salt and PIN;
//! - the control stream, encrypted RTSP and video: AES-128-GCM with a 12-byte IV and a
//!   16-byte tag, under the launch key (`rikey`);
//! - audio: AES-128-CBC with PKCS#7 padding, under the launch key, IV from `rikeyid`.

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use anyhow::{Result, anyhow, bail};
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};

pub type Key = [u8; 16];

/// AES-128-ECB over whole blocks (pairing). `data.len()` must be a multiple of 16.
pub fn ecb_encrypt(key: &Key, data: &[u8]) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        bail!("ECB input of {} bytes isn't whole blocks", data.len());
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(out)
}

/// The inverse of [`ecb_encrypt`].
pub fn ecb_decrypt(key: &Key, data: &[u8]) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        bail!("ECB input of {} bytes isn't whole blocks", data.len());
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(out)
}

/// AES-128-GCM encryption in place, no associated data. Returns the tag.
pub fn gcm_seal(key: &Key, iv: &[u8; 12], data: &mut [u8]) -> [u8; 16] {
    let cipher = Aes128Gcm::new(GenericArray::from_slice(key));
    let tag = cipher.encrypt_in_place_detached(Nonce::from_slice(iv), &[], data).expect("GCM input within limits");
    tag.into()
}

/// AES-128-GCM decryption in place, checking `tag`.
pub fn gcm_open(key: &Key, iv: &[u8; 12], data: &mut [u8], tag: &[u8; 16]) -> Result<()> {
    let cipher = Aes128Gcm::new(GenericArray::from_slice(key));
    cipher
        .decrypt_in_place_detached(Nonce::from_slice(iv), &[], data, Tag::from_slice(tag))
        .map_err(|_| anyhow!("GCM tag mismatch"))
}

/// A 12-byte GCM IV in GameStream's layout: the 32-bit sequence number little-endian in
/// bytes 0..4, zeros, then two marker bytes that say who sent it and on which stream
/// (`H`/`C` for host or client, then `C` control, `R` RTSP).
pub fn marked_iv(seq: u32, who: u8, stream: u8) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = who;
    iv[11] = stream;
    iv
}

/// AES-128-CBC with PKCS#7 padding (audio). The output is the input rounded up to the next
/// whole block (a full block of padding when the input is already whole blocks).
pub fn cbc_encrypt(key: &Key, iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<Aes128>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(data)
}

#[cfg(test)]
pub fn cbc_decrypt(key: &Key, iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    use cbc::cipher::BlockDecryptMut;
    cbc::Decryptor::<Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| anyhow!("bad CBC padding"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_fips197_vector() {
        // FIPS-197 appendix C.1: AES-128.
        let key: Key = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap().try_into().unwrap();
        let pt = hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let ct = ecb_encrypt(&key, &pt).unwrap();
        assert_eq!(hex::encode(&ct), "69c4e0d86a7b0430d8cdb78070b4c55a");
        assert_eq!(ecb_decrypt(&key, &ct).unwrap(), pt);
        assert!(ecb_encrypt(&key, &[0; 15]).is_err());
    }

    #[test]
    fn gcm_roundtrip_and_tamper() {
        let key = [7u8; 16];
        let iv = marked_iv(5, b'H', b'C');
        assert_eq!(iv, [5, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'H', b'C']);
        let mut data = b"hello control stream".to_vec();
        let tag = gcm_seal(&key, &iv, &mut data);
        assert_ne!(&data, b"hello control stream");
        let mut copy = data.clone();
        gcm_open(&key, &iv, &mut copy, &tag).unwrap();
        assert_eq!(&copy, b"hello control stream");
        data[0] ^= 1;
        assert!(gcm_open(&key, &iv, &mut data, &tag).is_err());
    }

    #[test]
    fn gcm_known_vector() {
        // McGrew & Viega GCM test case 2: zero key, zero IV, one zero block.
        let key = [0u8; 16];
        let iv = [0u8; 12];
        let mut data = [0u8; 16];
        let tag = gcm_seal(&key, &iv, &mut data);
        assert_eq!(hex::encode(data), "0388dace60b6a392f328c2b971b2fe78");
        assert_eq!(hex::encode(tag), "ab6e47d42cec13bdf53a67b21257bddf");
    }

    #[test]
    fn cbc_pads_to_blocks() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        for len in [0usize, 1, 15, 16, 17, 100] {
            let data: Vec<u8> = (0..len as u8).collect();
            let ct = cbc_encrypt(&key, &iv, &data);
            assert_eq!(ct.len(), (len / 16 + 1) * 16);
            assert_eq!(cbc_decrypt(&key, &iv, &ct).unwrap(), data);
        }
    }
}
