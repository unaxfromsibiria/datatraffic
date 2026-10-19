//! Plaintext and encrypted message structures for byte-stream transfer.
//! `RawData` is the internal plaintext layout; `ClientData` wraps it in an AES-256-GCM envelope.

use aes_gcm::{aead::{Aead, Generate, KeyInit}, Aes256Gcm, Nonce};
use bytes::{Bytes, BytesMut, BufMut};
use rand::{Rng, RngExt};

/// Errors produced by the encode/decode and encrypt/decrypt paths.
#[derive(Debug, PartialEq)]
pub enum DataError {
    /// Structure is malformed (bad lengths, truncated buffer, bad key).
    Malformed(String),
    /// A text field is not valid UTF-8.
    BadUtf8,
    /// Structure parses but the ciphertext cannot be decrypted with this key.
    BadPayload,
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "invalid message structure: {reason}"),
            Self::BadUtf8 => write!(f, "text field is not valid UTF-8"),
            Self::BadPayload => write!(f, "valid structure but content cannot be decrypted"),
        }
    }
}

impl std::error::Error for DataError {}

/// Internal plaintext message: a byte stream presented as a file plus random padding.
#[derive(Debug, Clone, PartialEq)]
pub struct RawData {
    /// Logical name / label for the payload (the "file name").
    pub name: String,
    /// Short note attached to the payload.
    pub note: String,
    /// Random padding block used to hide the length of `data`.
    pub random: Vec<u8>,
    /// Raw content bytes.
    pub data: Bytes,
}

impl RawData {
    /// Build a plaintext message, generating 8..=32 bytes of random padding.
    pub fn new(name: impl Into<String>, note: impl Into<String>, data: Bytes) -> Self {
        let mut rng = rand::rng();
        let pad_len = rng.random_range(8..=32);
        let mut random = vec![0u8; pad_len];
        rng.fill_bytes(&mut random);
        RawData { name: name.into(), note: note.into(), random, data }
    }

    /// Serialize into the internal wire format.
    /// Layout: `[1B name_len][2B u16le note_len][1B random_len][name][note][random][data]`.
    pub fn encode(&self) -> Result<Bytes, DataError> {
        let name_len = self.name.len();
        let note_len = self.note.len();
        let random_len = self.random.len();
        if name_len > u8::MAX as usize {
            return Err(DataError::Malformed(format!("name too long: {name_len} bytes (max 255)")));
        }
        if note_len > u16::MAX as usize {
            return Err(DataError::Malformed(format!("note too long: {note_len} bytes (max 65535)")));
        }
        if random_len > u8::MAX as usize {
            return Err(DataError::Malformed(format!("random too long: {random_len} bytes (max 255)")));
        }

        let mut buf = BytesMut::with_capacity(4 + name_len + note_len + random_len + self.data.len());
        buf.put_u8(name_len as u8);
        buf.put_u16_le(note_len as u16);
        buf.put_u8(random_len as u8);
        buf.extend_from_slice(self.name.as_bytes());
        buf.extend_from_slice(self.note.as_bytes());
        buf.extend_from_slice(&self.random);
        buf.extend_from_slice(self.data.as_ref());
        Ok(buf.freeze())
    }

    /// Deserialize from bytes produced by `encode`.
    pub fn decode(buf: &[u8]) -> Result<Self, DataError> {
        const HEADER_LEN: usize = 4;
        if buf.len() < HEADER_LEN {
            return Err(DataError::Malformed(format!(
                "buffer too short: {} bytes (need at least {HEADER_LEN})",
                buf.len()
            )));
        }

        let name_len = buf[0] as usize;
        let note_len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
        let random_len = buf[3] as usize;

        let mut pos = HEADER_LEN;
        if pos + name_len > buf.len() {
            return Err(DataError::Malformed("name length exceeds buffer".into()));
        }
        let name_bytes = &buf[pos..pos + name_len];
        pos += name_len;

        if pos + note_len > buf.len() {
            return Err(DataError::Malformed("note length exceeds buffer".into()));
        }
        let note_bytes = &buf[pos..pos + note_len];
        pos += note_len;

        if pos + random_len > buf.len() {
            return Err(DataError::Malformed("random length exceeds buffer".into()));
        }
        let random = buf[pos..pos + random_len].to_vec();
        pos += random_len;

        // `data` is the remainder of the buffer, no length prefix.
        let data = Bytes::copy_from_slice(&buf[pos..]);

        let name = String::from_utf8(name_bytes.to_vec()).map_err(|_| DataError::BadUtf8)?;
        let note = String::from_utf8(note_bytes.to_vec()).map_err(|_| DataError::BadUtf8)?;

        Ok(RawData { name, note, random, data })
    }
}

/// Encrypted variant of a plaintext message (AES-256-GCM ciphertext + nonce).
#[derive(Debug, Clone)]
pub struct ClientData {
    /// Ciphertext including the authentication tag.
    pub encrypted: Bytes,
    /// Nonce used for encryption.
    pub nonce: Vec<u8>,
}

impl ClientData {
    /// Encrypt a plaintext message into an encrypted one using the 32-byte key.
    pub fn encrypt(raw: &RawData, key: &[u8; 32]) -> Result<Self, DataError> {
        let plaintext = raw.encode()?;
        let cipher = Aes256Gcm::new_from_slice(key).map_err(
            |e| DataError::Malformed(format!("invalid AES-256-GCM key: {e}"))
        )?;
        let nonce = Nonce::generate();
        let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).map_err(
            |_| DataError::BadPayload
        )?;
        Ok(ClientData { encrypted: Bytes::from(ciphertext), nonce: nonce.to_vec() })
    }

    /// Decrypt back to a plaintext message using the 32-byte key.
    pub fn decrypt(&self, key: &[u8; 32]) -> Result<RawData, DataError> {
        let cipher = Aes256Gcm::new_from_slice(key).map_err(
            |e| DataError::Malformed(format!("invalid AES-256-GCM key: {e}"))
        )?;
        let nonce = Nonce::try_from(self.nonce.as_slice()).map_err(
            |_| DataError::Malformed("invalid nonce length".into())
        )?;
        let plaintext = cipher.decrypt(&nonce, self.encrypted.as_ref()).map_err(
            |_| DataError::BadPayload
        )?;
        RawData::decode(&plaintext)
    }

    /// Serialize into the external wire format.
    /// Layout: `[encrypted][nonce][4B u32le len(encrypted)]`.
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.encrypted.len() + self.nonce.len() + 4);
        buf.extend_from_slice(self.encrypted.as_ref());
        buf.extend_from_slice(&self.nonce);
        buf.put_u32_le(self.encrypted.len() as u32);
        buf.freeze()
    }

    /// Deserialize from bytes produced by `encode`.
    pub fn decode(buf: &[u8]) -> Result<Self, DataError> {
        const TRAILER_LEN: usize = 4;
        // Need the trailer plus at least one nonce byte.
        if buf.len() < TRAILER_LEN + 1 {
            return Err(DataError::Malformed(format!("buffer too short: {} bytes", buf.len())));
        }

        let encrypted_len = u32::from_le_bytes([
            buf[buf.len() - 4],
            buf[buf.len() - 3],
            buf[buf.len() - 2],
            buf[buf.len() - 1],
        ]) as usize;

        // Nonce occupies [encrypted_len .. len-TRAILER_LEN]; it must be non-empty.
        if encrypted_len + TRAILER_LEN >= buf.len() {
            return Err(DataError::Malformed("empty nonce".into()));
        }

        let encrypted = Bytes::copy_from_slice(&buf[..encrypted_len]);
        let nonce = buf[encrypted_len..buf.len() - TRAILER_LEN].to_vec();
        Ok(ClientData { encrypted, nonce })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: [u8; 32] = [0xa5; 32];
    const KEY_B: [u8; 32] = [0x1b; 32];

    #[test]
    fn rawdata_roundtrip() {
        let raw = RawData::new("report.pdf", "Q3 results", Bytes::from_static(b"file-content"));
        let encoded = raw.encode().unwrap();
        let decoded = RawData::decode(&encoded).unwrap();
        assert_eq!(decoded.name, raw.name);
        assert_eq!(decoded.note, raw.note);
        assert_eq!(decoded.random, raw.random);
        assert_eq!(decoded.data, raw.data);
    }

    #[test]
    fn rawdata_roundtrip_empty_fields() {
        let raw = RawData::new("", "", Bytes::new());
        let decoded = RawData::decode(&raw.encode().unwrap()).unwrap();
        assert_eq!(decoded.name, "");
        assert_eq!(decoded.note, "");
        assert!(decoded.data.is_empty());
    }

    #[test]
    fn rawdata_roundtrip_unicode() {
        let raw = RawData::new("отчёт.pdf", "примечание", Bytes::from_static(b"\x00\x01\x02"));
        let decoded = RawData::decode(&raw.encode().unwrap()).unwrap();
        assert_eq!(decoded.name, "отчёт.pdf");
        assert_eq!(decoded.note, "примечание");
    }

    #[test]
    fn rawdata_encode_name_too_long() {
        let mut raw = RawData::new("x", "", Bytes::new());
        raw.name = "a".repeat(256); // exceeds u8 max
        assert!(raw.encode().is_err());
    }

    #[test]
    fn rawdata_encode_note_too_long() {
        let mut raw = RawData::new("x", "", Bytes::new());
        raw.note = "a".repeat(65536); // exceeds u16 max
        assert!(raw.encode().is_err());
    }

    #[test]
    fn rawdata_decode_too_short() {
        assert!(RawData::decode(&[0u8; 2]).is_err()); // less than the 4-byte header
    }

    #[test]
    fn rawdata_decode_truncated_name() {
        let mut buf = Vec::new();
        buf.push(10u8); // name_len says 10 but only 3 bytes follow
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.push(0u8);
        buf.extend_from_slice(b"abc");
        assert!(RawData::decode(&buf).is_err());
    }

    #[test]
    fn rawdata_decode_bad_utf8() {
        let mut buf = Vec::new();
        buf.push(2u8); // name_len = 2
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.push(0u8);
        buf.extend_from_slice(&[0xff, 0xfe]); // invalid UTF-8 for name
        assert_eq!(RawData::decode(&buf), Err(DataError::BadUtf8));
    }

    #[test]
    fn clientdata_roundtrip_valid_key() {
        let raw = RawData::new("secret.txt", "top secret", Bytes::from_static(b"payload-bytes"));
        let client = ClientData::encrypt(&raw, &KEY_A).unwrap();
        assert!(!client.nonce.is_empty());
        // ciphertext must differ from the plaintext encoding
        assert_ne!(client.encrypted, raw.encode().unwrap());
        let decoded = client.decrypt(&KEY_A).unwrap();
        assert_eq!(decoded.name, raw.name);
        assert_eq!(decoded.note, raw.note);
        assert_eq!(decoded.data, raw.data);
    }

    #[test]
    fn clientdata_wrong_key_is_bad_payload() {
        let raw = RawData::new("secret.txt", "", Bytes::from_static(b"payload"));
        let client = ClientData::encrypt(&raw, &KEY_A).unwrap();
        assert_eq!(client.decrypt(&KEY_B), Err(DataError::BadPayload));
    }

    #[test]
    fn invalid_hex_key_is_init_error() {
        // A non-hex or wrong-length key cannot be turned into the 32-byte cipher key.
        assert!(crate::config::decode_key("zzz").is_err());
        assert!(crate::config::decode_key(&"a".repeat(60)).is_err());
    }

    #[test]
    fn clientdata_nonce_unique() {
        let raw = RawData::new("same.txt", "", Bytes::from_static(b"repeated"));
        let c1 = ClientData::encrypt(&raw, &KEY_A).unwrap();
        let c2 = ClientData::encrypt(&raw, &KEY_A).unwrap();
        assert_ne!(c1.nonce, c2.nonce);
        // different nonces yield different ciphertexts for identical plaintext
        assert_ne!(c1.encrypted, c2.encrypted);
    }

    #[test]
    fn clientdata_encode_decode_roundtrip() {
        let raw = RawData::new("file.bin", "note", Bytes::from_static(b"some-data"));
        let client = ClientData::encrypt(&raw, &KEY_A).unwrap();
        let wire = client.encode();
        let decoded = ClientData::decode(&wire).unwrap();
        assert_eq!(decoded.encrypted, client.encrypted);
        assert_eq!(decoded.nonce, client.nonce);
    }

    #[test]
    fn clientdata_decode_too_short() {
        // less than trailer + one nonce byte
        assert!(ClientData::decode(&[0u8; 4]).is_err());
    }

    #[test]
    fn clientdata_decode_empty_nonce() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[1u8, 2u8]); // "encrypted" (2 bytes)
        buf.extend_from_slice(&(2u32).to_le_bytes()); // len(encrypted)=2 -> no room for nonce
        assert!(ClientData::decode(&buf).is_err());
    }

    #[test]
    fn transport_overhead_exceeds_source() {
        let mut rng = rand::rng();
        for _ in 0..50 {
            let data_len = rng.random_range(128..=4096);
            let mut data = vec![0u8; data_len];
            rng.fill_bytes(&mut data);
            let raw = RawData::new("blob.bin", "bulk", Bytes::from(data));
            let client = ClientData::encrypt(&raw, &KEY_A).unwrap();
            assert!(client.encode().len() > raw.data.len());
        }
    }
}
