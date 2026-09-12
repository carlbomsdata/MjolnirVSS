//! BLAKE3 digests of chunk contents.
//!
//! A digest is always taken over the *uncompressed* bytes. That makes the
//! identity of a chunk independent of the compression level, so a store written
//! at one level still deduplicates against chunks written at another.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Length of a BLAKE3 digest in bytes.
pub const DIGEST_BYTES: usize = 32;

/// Length of a digest written as lowercase hexadecimal.
pub const DIGEST_HEX_LEN: usize = DIGEST_BYTES * 2;

/// A BLAKE3 digest of a chunk's uncompressed contents.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkHash([u8; DIGEST_BYTES]);

impl ChunkHash {
    /// Computes the digest of `data`.
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    /// Lowercase hexadecimal form, as stored in the manifest and as a filename.
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(DIGEST_HEX_LEN);
        for b in self.0 {
            s.push(char::from_digit(u32::from(b >> 4), 16).expect("nibble"));
            s.push(char::from_digit(u32::from(b & 0x0f), 16).expect("nibble"));
        }
        s
    }

    /// The first `n` hex characters, used as the chunk store fanout directory.
    pub fn hex_prefix(self, n: usize) -> String {
        self.to_hex().chars().take(n).collect()
    }

    /// Parses a lowercase hexadecimal digest.
    ///
    /// Uppercase is rejected so that a digest has exactly one spelling; two
    /// spellings would mean two filenames for one chunk on a case insensitive
    /// filesystem, and a dedup table that disagrees with the directory.
    pub fn parse_hex(text: &str) -> Result<Self, HashParseError> {
        if text.len() != DIGEST_HEX_LEN {
            return Err(HashParseError::BadLength { len: text.len() });
        }
        let bytes = text.as_bytes();
        let mut out = [0u8; DIGEST_BYTES];
        for (i, out_byte) in out.iter_mut().enumerate() {
            let hi = hex_nibble(bytes[i * 2])?;
            let lo = hex_nibble(bytes[i * 2 + 1])?;
            *out_byte = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_nibble(c: u8) -> Result<u8, HashParseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(HashParseError::BadCharacter { ch: c as char }),
    }
}

/// Why a digest string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashParseError {
    /// The string was not [`DIGEST_HEX_LEN`] characters long.
    BadLength {
        /// The length that was offered.
        len: usize,
    },
    /// The string contained something other than `0-9` or `a-f`.
    BadCharacter {
        /// The offending character.
        ch: char,
    },
}

impl fmt::Display for HashParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashParseError::BadLength { len } => write!(
                f,
                "a chunk digest must be {DIGEST_HEX_LEN} hexadecimal characters, this one is {len}"
            ),
            HashParseError::BadCharacter { ch } => write!(
                f,
                "a chunk digest may only contain 0-9 and lowercase a-f, found {ch:?}"
            ),
        }
    }
}

impl std::error::Error for HashParseError {}

impl fmt::Display for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkHash({})", self.to_hex())
    }
}

impl Serialize for ChunkHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ChunkHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        // A digest becomes a path component, so it is validated here rather
        // than trusted from the manifest.
        ChunkHash::parse_hex(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_the_blake3_test_vector() {
        // BLAKE3 of the empty input, from the reference implementation.
        assert_eq!(
            ChunkHash::of(b"").to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            ChunkHash::of(b"abc").to_hex(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn hex_round_trips() {
        let h = ChunkHash::of(b"mjolnir");
        assert_eq!(ChunkHash::parse_hex(&h.to_hex()).unwrap(), h);
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            ChunkHash::parse_hex("abcd").unwrap_err(),
            HashParseError::BadLength { len: 4 }
        );
    }

    #[test]
    fn rejects_uppercase_and_non_hex() {
        let upper = ChunkHash::of(b"x").to_hex().to_uppercase();
        assert!(matches!(
            ChunkHash::parse_hex(&upper),
            Err(HashParseError::BadCharacter { .. })
        ));
        let mut bad = ChunkHash::of(b"x").to_hex();
        bad.replace_range(0..1, "z");
        assert!(matches!(
            ChunkHash::parse_hex(&bad),
            Err(HashParseError::BadCharacter { ch: 'z' })
        ));
    }

    #[test]
    fn rejects_path_traversal_in_a_digest_field() {
        let err =
            serde_json::from_str::<ChunkHash>("\"../../../../windows/system32\"").unwrap_err();
        assert!(err.to_string().contains("hexadecimal"), "{err}");
    }

    #[test]
    fn prefix_is_taken_from_the_front() {
        let h = ChunkHash::of(b"");
        assert_eq!(h.hex_prefix(2), "af");
        assert_eq!(h.hex_prefix(0), "");
    }

    #[test]
    fn different_inputs_give_different_digests() {
        assert_ne!(ChunkHash::of(b"a"), ChunkHash::of(b"b"));
        assert_ne!(ChunkHash::of(&[0u8; 4096]), ChunkHash::of(&[0u8; 4097]));
    }

    #[test]
    fn serialises_as_a_plain_string() {
        let h = ChunkHash::of(b"");
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            "\"af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262\""
        );
    }
}
