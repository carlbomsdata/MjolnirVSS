//! Turning a password into keys, and sealing blocks with them.
//!
//! **Nothing in this crate is a cryptographic design.** The password becomes a
//! key with Argon2id and blocks are sealed with AES-256-GCM, both through the
//! RustCrypto crates, used the way their documentation says to use them. What
//! this crate contributes is the plumbing around them: which key is used for
//! what, what is written down so a backup can still be opened in ten years, and
//! what is deliberately *not* written down.
//!
//! ## What is protected, and what is not
//!
//! Only the contents of a backup are encrypted. The documents beside them —
//! which machine it came from, when, how the disk was laid out, how big each
//! partition was — stay readable.
//!
//! That is a deliberate trade, not an oversight. Somebody standing in front of a
//! broken computer with an external drive and three backups on it has to be able
//! to tell which one is theirs before they can be asked for a password. A backup
//! that cannot identify itself is a backup that cannot be chosen in the one
//! situation it exists for.
//!
//! So an attacker holding the drive learns that you have a backup of a machine
//! called `DESKTOP-1A2B`, taken on a particular day, with a 62 GiB Windows
//! partition. They do not learn a single byte of what is in it.
//!
//! ## The password is not stored
//!
//! Not in the backup, not in the registry, not in a file beside it. There is no
//! recovery mechanism and there is not meant to be one: a backup you can open
//! without the password is a backup anybody can open. **If the password is lost
//! the backup is lost**, and MjolnirVSS says so before it starts rather than
//! afterwards.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

// `password` needs one unsafe call to turn console echo off.
#![warn(missing_docs)]

pub mod password;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Bytes in the salt Argon2id is given.
pub const SALT_BYTES: usize = 16;

/// Bytes in the nonce AES-GCM is given for each block.
pub const NONCE_BYTES: usize = 12;

/// Bytes of key material derived from the password.
///
/// Two independent keys: one seals blocks, one names them.
const DERIVED_BYTES: usize = 64;

/// How hard the password is to turn into a key.
///
/// Written into the backup so that a future version, or a future computer, can
/// still open it. Reading these from the backup rather than assuming today's
/// values is the difference between a format that lasts and one that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Memory in kibibytes.
    pub memory_kib: u32,
    /// Passes over that memory.
    pub passes: u32,
    /// How many lanes may be worked in parallel.
    pub lanes: u32,
}

impl Default for KdfParams {
    /// What a backup taken today uses.
    ///
    /// 64 MiB and three passes takes a noticeable fraction of a second on an
    /// ordinary desktop, which is the point: it is the cost an attacker pays
    /// for every password they try.
    fn default() -> Self {
        Self {
            memory_kib: 64 * 1024,
            passes: 3,
            lanes: 1,
        }
    }
}

impl KdfParams {
    /// Whether these are values this version is willing to use.
    ///
    /// A backup asking for a gigabyte of memory, or for one pass, is either
    /// damaged or was written by something that is not MjolnirVSS. Either way
    /// it is refused rather than obeyed.
    pub fn check(&self) -> Result<()> {
        if self.passes == 0 || self.lanes == 0 {
            return Err(refuse(
                "the backup asks for key derivation settings that are not usable",
                format!("it records {} passes and {} lanes", self.passes, self.lanes),
            ));
        }
        if self.memory_kib < 8 * 1024 {
            return Err(refuse(
                "the backup asks for weaker key derivation than this version accepts",
                format!(
                    "it records {} KiB of memory, and the least this version will use is {} KiB",
                    self.memory_kib,
                    8 * 1024
                ),
            ));
        }
        if self.memory_kib > 4 * 1024 * 1024 {
            return Err(refuse(
                "the backup asks for more memory to open than this version will allocate",
                format!(
                    "it records {} KiB, which is more than the {} KiB limit",
                    self.memory_kib,
                    4 * 1024 * 1024
                ),
            ));
        }
        Ok(())
    }
}

/// What a backup writes down about its encryption.
///
/// Everything here is public by design. None of it helps anybody open the
/// backup: it is what is needed to *try*, which an attacker holding the drive
/// could work out anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionInfo {
    /// How blocks are sealed. Only `aes-256-gcm` exists today.
    pub cipher: String,
    /// How the password becomes a key. Only `argon2id` exists today.
    pub kdf: String,
    /// The settings that key derivation used.
    pub kdf_params: KdfParams,
    /// The salt, as lowercase hex.
    pub salt_hex: String,
    /// A value that proves a password is the right one without revealing it.
    ///
    /// Sealing a fixed phrase with the derived key. Anybody can try to open it;
    /// only the right password succeeds. It exists so that a wrong password is
    /// answered with "that password is wrong" rather than with a failure to
    /// read some block deep in the restore.
    pub check_hex: String,
}

/// The phrase sealed to make the check value.
const CHECK_PHRASE: &[u8] = b"MjolnirVSS encrypted backup";

/// Keys derived from a password. Wiped when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Keys {
    /// Seals and opens block contents.
    sealing: [u8; 32],
    /// Names blocks, so the names do not reveal the contents.
    ///
    /// Blocks are content addressed, and an unkeyed digest of the contents
    /// would let anybody holding the drive test whether a file they already
    /// have is in the backup, just by computing its name. Keying the digest
    /// removes that, and still lets identical blocks within one backup be
    /// stored once.
    naming: [u8; 32],
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material, not even by accident in a log line.
        f.write_str("Keys { .. }")
    }
}

impl Keys {
    /// Derives keys from a password and a salt.
    pub fn derive(password: &str, salt: &[u8], params: KdfParams) -> Result<Self> {
        params.check()?;
        if password.is_empty() {
            return Err(refuse(
                "a password is needed to open this backup",
                "an empty password was given".to_owned(),
            ));
        }
        if salt.len() != SALT_BYTES {
            return Err(refuse(
                "this backup's encryption details are damaged",
                format!(
                    "its salt is {} bytes and should be {SALT_BYTES}",
                    salt.len()
                ),
            ));
        }

        let argon = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(
                params.memory_kib,
                params.passes,
                params.lanes,
                Some(DERIVED_BYTES),
            )
            .map_err(|e| {
                refuse(
                    "the key derivation settings were refused",
                    format!("argon2 reported: {e}"),
                )
            })?,
        );

        let mut derived = [0u8; DERIVED_BYTES];
        argon
            .hash_password_into(password.as_bytes(), salt, &mut derived)
            .map_err(|e| {
                refuse(
                    "the password could not be turned into a key",
                    format!("argon2 reported: {e}"),
                )
            })?;

        let mut sealing = [0u8; 32];
        let mut naming = [0u8; 32];
        sealing.copy_from_slice(&derived[..32]);
        naming.copy_from_slice(&derived[32..]);
        derived.zeroize();

        Ok(Self { sealing, naming })
    }

    /// The name a block of `plaintext` is stored under.
    pub fn name_of(&self, plaintext: &[u8]) -> [u8; 32] {
        *blake3::keyed_hash(&self.naming, plaintext).as_bytes()
    }

    /// Seals a block. The nonce is returned with it and is not a secret.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.sealing));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let sealed = cipher.encrypt(&nonce, plaintext).map_err(|_| {
            refuse(
                "a block could not be encrypted",
                "the cipher refused the block".to_owned(),
            )
        })?;

        let mut out = Vec::with_capacity(NONCE_BYTES + sealed.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Opens a block sealed by [`Keys::seal`].
    ///
    /// Fails if the password is wrong, or if a single bit of the block has
    /// changed. There is no way to get part of a damaged block back: that is
    /// what authenticated encryption means, and it is what makes silent
    /// corruption impossible rather than merely unlikely.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() <= NONCE_BYTES {
            return Err(corrupt(
                "a stored block is too short to be a sealed block",
                format!(
                    "it is {} bytes and the nonce alone is {NONCE_BYTES}",
                    sealed.len()
                ),
            ));
        }
        let (nonce, body) = sealed.split_at(NONCE_BYTES);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.sealing));
        cipher.decrypt(Nonce::from_slice(nonce), body).map_err(|_| {
            corrupt(
                "a stored block could not be opened",
                "either the password is wrong or the block has been changed since it was written"
                    .to_owned(),
            )
        })
    }

    /// Makes the value that proves a password without revealing it.
    pub fn check_value(&self) -> Result<Vec<u8>> {
        self.seal(CHECK_PHRASE)
    }

    /// Whether this key opens `check`.
    pub fn opens(&self, check: &[u8]) -> bool {
        matches!(self.open(check), Ok(phrase) if phrase == CHECK_PHRASE)
    }
}

/// Everything needed to start encrypting a new backup.
pub struct NewEncryption {
    /// The keys to seal with.
    pub keys: Keys,
    /// What to write into the backup.
    pub info: EncryptionInfo,
}

/// Starts encryption for a new backup, with a fresh random salt.
pub fn begin(password: &str, params: KdfParams) -> Result<NewEncryption> {
    let mut salt = [0u8; SALT_BYTES];
    fill_random(&mut salt)?;

    let keys = Keys::derive(password, &salt, params)?;
    let check = keys.check_value()?;

    Ok(NewEncryption {
        info: EncryptionInfo {
            cipher: "aes-256-gcm".to_owned(),
            kdf: "argon2id".to_owned(),
            kdf_params: params,
            salt_hex: to_hex(&salt),
            check_hex: to_hex(&check),
        },
        keys,
    })
}

/// Opens an existing encrypted backup with a password.
///
/// The wrong password is reported as the wrong password, immediately, rather
/// than as a failure to read something later on.
pub fn unlock(info: &EncryptionInfo, password: &str) -> Result<Keys> {
    if info.cipher != "aes-256-gcm" {
        return Err(unsupported(format!(
            "this backup is sealed with {}, which this version does not know",
            info.cipher
        )));
    }
    if info.kdf != "argon2id" {
        return Err(unsupported(format!(
            "this backup derives its key with {}, which this version does not know",
            info.kdf
        )));
    }

    let salt = from_hex(&info.salt_hex, "salt")?;
    let check = from_hex(&info.check_hex, "check value")?;
    let keys = Keys::derive(password, &salt, info.kdf_params)?;

    if !keys.opens(&check) {
        return Err(Error::new(
            ExitCode::Failure,
            "that password does not open this backup",
            "the password was turned into a key and the key did not open the backup's check value"
                .to_owned(),
            "try the password again; there is no way to recover a backup whose password is lost, and MjolnirVSS never stored it",
        ));
    }
    Ok(keys)
}

/// Fills `out` with random bytes from the operating system.
fn fill_random(out: &mut [u8]) -> Result<()> {
    use aes_gcm::aead::rand_core::RngCore;
    let mut rng = OsRng;
    rng.try_fill_bytes(out).map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "the operating system would not provide random bytes",
            format!("the random number source reported: {e}"),
            "this is not something MjolnirVSS can work around; restart the computer and try again",
        )
    })
}

/// Lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Bytes from lowercase hex, naming what was being read if it is wrong.
pub fn from_hex(text: &str, what: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 {
        return Err(corrupt(
            format!("this backup's {what} is damaged"),
            format!(
                "it is {} characters, which is not a whole number of bytes",
                text.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = hex_digit(pair[0], what)?;
        let lo = hex_digit(pair[1], what)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(c: u8, what: &str) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(corrupt(
            format!("this backup's {what} is damaged"),
            format!("it contains {:?}, which is not a hex digit", c as char),
        )),
    }
}

fn refuse(what: impl Into<String>, why: impl Into<String>) -> Error {
    Error::new(
        ExitCode::Failure,
        what,
        why,
        "check the password and try again",
    )
}

fn unsupported(why: impl Into<String>) -> Error {
    Error::new(
        ExitCode::Unsupported,
        "this backup cannot be opened by this version",
        why,
        "use the version of MjolnirVSS that wrote it",
    )
}

fn corrupt(what: impl Into<String>, why: impl Into<String>) -> Error {
    Error::new(
        ExitCode::CorruptBackup,
        what,
        why,
        "this backup cannot be used; take a new one, and check the drive it was on",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fast settings, so the tests do not each spend a second grinding.
    /// Everything they check is true of the real settings too.
    fn quick() -> KdfParams {
        KdfParams {
            memory_kib: 8 * 1024,
            passes: 1,
            lanes: 1,
        }
    }

    #[test]
    fn a_sealed_block_comes_back_exactly() {
        let started = begin("correct horse battery staple", quick()).unwrap();
        let plaintext = b"the contents of a block".to_vec();
        let sealed = started.keys.seal(&plaintext).unwrap();

        assert_ne!(
            sealed, plaintext,
            "the stored form must not be the contents"
        );
        assert_eq!(started.keys.open(&sealed).unwrap(), plaintext);
    }

    #[test]
    fn the_same_block_sealed_twice_looks_different() {
        // A fresh nonce every time, so identical contents do not produce
        // identical stored blocks. Otherwise the drive would show which parts
        // of a disk are the same as each other.
        let started = begin("a password", quick()).unwrap();
        let a = started.keys.seal(b"identical").unwrap();
        let b = started.keys.seal(b"identical").unwrap();
        assert_ne!(a, b);
        assert_eq!(
            started.keys.open(&a).unwrap(),
            started.keys.open(&b).unwrap()
        );
    }

    #[test]
    fn the_wrong_password_is_refused_immediately() {
        let started = begin("the right one", quick()).unwrap();
        let err = unlock(&started.info, "the wrong one").unwrap_err();
        assert!(
            err.what().contains("does not open"),
            "the message should say the password is wrong: {}",
            err.what()
        );
        assert!(
            err.next_step().contains("never stored it"),
            "and should say the password cannot be recovered: {}",
            err.next_step()
        );
    }

    #[test]
    fn the_right_password_opens_it_again() {
        let started = begin("the right one", quick()).unwrap();
        let sealed = started.keys.seal(b"something").unwrap();

        let reopened = unlock(&started.info, "the right one").unwrap();
        assert_eq!(reopened.open(&sealed).unwrap(), b"something".to_vec());
    }

    #[test]
    fn a_changed_block_will_not_open() {
        let started = begin("a password", quick()).unwrap();
        let mut sealed = started.keys.seal(b"important contents").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;

        let err = started.keys.open(&sealed).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
    }

    /// Flipping a bit of the nonce must fail too: it is not a secret, but it is
    /// covered by the authentication.
    #[test]
    fn a_changed_nonce_will_not_open() {
        let started = begin("a password", quick()).unwrap();
        let mut sealed = started.keys.seal(b"important contents").unwrap();
        sealed[0] ^= 0x80;
        assert!(started.keys.open(&sealed).is_err());
    }

    #[test]
    fn a_truncated_block_will_not_open() {
        let started = begin("a password", quick()).unwrap();
        let sealed = started.keys.seal(b"important contents").unwrap();
        assert!(started.keys.open(&sealed[..sealed.len() - 1]).is_err());
        assert!(started.keys.open(&sealed[..NONCE_BYTES]).is_err());
        assert!(started.keys.open(&[]).is_err());
    }

    /// The name a block is stored under must not be derivable by somebody who
    /// has the same file but not the password. Otherwise the names alone say
    /// what is in the backup.
    #[test]
    fn block_names_depend_on_the_password() {
        let one = begin("password one", quick()).unwrap();
        let two = begin("password two", quick()).unwrap();
        let block = b"a file somebody might already have";

        assert_ne!(one.keys.name_of(block), two.keys.name_of(block));
        assert_ne!(
            one.keys.name_of(block),
            *blake3::hash(block).as_bytes(),
            "the plain digest must not be the name"
        );
    }

    /// Identical blocks within one backup must still share a name, or nothing
    /// would ever be stored once.
    #[test]
    fn identical_blocks_share_a_name_within_one_backup() {
        let started = begin("a password", quick()).unwrap();
        assert_eq!(started.keys.name_of(b"same"), started.keys.name_of(b"same"));
        assert_ne!(
            started.keys.name_of(b"same"),
            started.keys.name_of(b"other")
        );
    }

    #[test]
    fn every_backup_gets_its_own_salt() {
        let a = begin("a password", quick()).unwrap();
        let b = begin("a password", quick()).unwrap();
        assert_ne!(
            a.info.salt_hex, b.info.salt_hex,
            "two backups with the same password must not share a salt"
        );
        // And so the same password gives different keys.
        assert_ne!(a.keys.name_of(b"x"), b.keys.name_of(b"x"));
    }

    #[test]
    fn an_empty_password_is_refused() {
        assert!(begin("", quick()).is_err());
    }

    #[test]
    fn settings_that_would_weaken_a_backup_are_refused() {
        let weak = KdfParams {
            memory_kib: 64,
            passes: 1,
            lanes: 1,
        };
        assert!(weak.check().is_err(), "too little memory must be refused");

        let none = KdfParams {
            passes: 0,
            ..KdfParams::default()
        };
        assert!(none.check().is_err(), "no passes must be refused");

        let huge = KdfParams {
            memory_kib: 64 * 1024 * 1024,
            ..KdfParams::default()
        };
        assert!(huge.check().is_err(), "an absurd demand must be refused");

        KdfParams::default().check().expect("today's settings work");
    }

    #[test]
    fn a_cipher_this_version_does_not_know_is_refused_by_name() {
        let started = begin("a password", quick()).unwrap();
        let mut info = started.info.clone();
        info.cipher = "rot13".to_owned();
        let err = unlock(&info, "a password").unwrap_err();
        assert_eq!(err.exit(), ExitCode::Unsupported);
        assert!(err.why().contains("rot13"), "{}", err.why());
    }

    #[test]
    fn damaged_encryption_details_are_reported_as_damage() {
        let started = begin("a password", quick()).unwrap();
        let mut info = started.info.clone();
        info.salt_hex = "not hex at all".to_owned();
        assert!(unlock(&info, "a password").is_err());

        let mut short = started.info.clone();
        short.salt_hex = "aabb".to_owned();
        assert!(unlock(&short, "a password").is_err());
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00u8, 0x0f, 0xa5, 0xff];
        assert_eq!(to_hex(&bytes), "000fa5ff");
        assert_eq!(from_hex("000fa5ff", "test").unwrap(), bytes.to_vec());
        assert!(from_hex("abc", "test").is_err());
        assert!(from_hex("zz", "test").is_err());
    }

    /// Key material must not reach a log by being printed.
    #[test]
    fn keys_do_not_print_themselves() {
        let started = begin("a memorable password", quick()).unwrap();
        let shown = format!("{:?}", started.keys);
        assert_eq!(shown, "Keys { .. }");
        assert!(!shown.contains("password"));
    }
}
