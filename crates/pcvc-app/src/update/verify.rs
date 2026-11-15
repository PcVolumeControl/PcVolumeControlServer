//! Checking a download against the release's `SHA256SUMS`, and that file
//! against an optional minisign signature.

use minisign_verify::{PublicKey, Signature};

/// The minisign public key (the base64 line of `minisign.pub`) that release
/// checksums must be signed with. With `None`, installs rely on the checksum
/// alone. See docs/RELEASING.md for making a key.
const UPDATE_PUBLIC_KEY: Option<&str> = None;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("SHA256SUMS is malformed (line {0})")]
    MalformedSums(usize),
    #[error("SHA256SUMS has no entry for {0}")]
    MissingEntry(String),
    #[error("SHA256SUMS lists {0} more than once")]
    DuplicateEntry(String),
    #[error("the download does not match its checksum")]
    ChecksumMismatch,
    #[error("the release has no signature for its checksums")]
    MissingSignature,
    #[error("the checksums' signature is not valid")]
    BadSignature,
    #[error("the built-in update signing key is invalid")]
    BadKey,
}

/// The key installs must be signed with, if one is built in.
pub fn signing_key() -> Result<Option<PublicKey>, VerifyError> {
    UPDATE_PUBLIC_KEY
        .map(|key| PublicKey::from_base64(key.trim()).map_err(|_| VerifyError::BadKey))
        .transpose()
}

/// The SHA-256 that `sums` (as written by `sha256sum`, text or binary mode)
/// lists for `name`. Every line must be well formed, and `name` must appear
/// exactly once.
pub fn digest_for(sums: &str, name: &str) -> Result<[u8; 32], VerifyError> {
    let mut found = None;
    for (index, line) in sums.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let (digest, file) = parse_line(line).ok_or(VerifyError::MalformedSums(index + 1))?;
        if file == name {
            if found.is_some() {
                return Err(VerifyError::DuplicateEntry(name.to_owned()));
            }
            found = Some(digest);
        }
    }
    found.ok_or_else(|| VerifyError::MissingEntry(name.to_owned()))
}

/// `<64 hex digits>  <name>` or `<64 hex digits> *<name>`.
fn parse_line(line: &str) -> Option<([u8; 32], &str)> {
    let (hex, rest) = line.split_at_checked(64)?;
    let name = rest
        .strip_prefix("  ")
        .or_else(|| rest.strip_prefix(" *"))?;
    if name.is_empty() {
        return None;
    }
    Some((parse_hex(hex)?, name))
}

fn parse_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Checks a minisign signature (`minisig`, the `.minisig` file) over
/// `data`. Only prehashed signatures, minisign's default, are accepted.
pub fn verify_signature(key: &PublicKey, data: &[u8], minisig: &[u8]) -> Result<(), VerifyError> {
    let text = std::str::from_utf8(minisig).map_err(|_| VerifyError::BadSignature)?;
    let signature = Signature::decode(text).map_err(|_| VerifyError::BadSignature)?;
    key.verify(data, &signature, false)
        .map_err(|_| VerifyError::BadSignature)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn bytes(hex: &str) -> [u8; 32] {
        parse_hex(hex).unwrap()
    }

    #[test]
    fn reads_text_and_binary_mode_lines() {
        let sums = format!("{A}  PcVolumeControl-1.2.3-setup.exe\n{B} *SHA256SUMS.minisig\n");
        assert_eq!(
            digest_for(&sums, "PcVolumeControl-1.2.3-setup.exe"),
            Ok(bytes(A))
        );
        assert_eq!(digest_for(&sums, "SHA256SUMS.minisig"), Ok(bytes(B)));
        let upper = format!("{}  x\n", A.to_uppercase());
        assert_eq!(digest_for(&upper, "x"), Ok(bytes(A)));
    }

    #[test]
    fn tolerates_crlf_and_blank_lines() {
        let sums = format!("{B}  other\r\n\r\n{A}  x\r\n");
        assert_eq!(digest_for(&sums, "x"), Ok(bytes(A)));
        assert_eq!(digest_for(&format!("{A}  x"), "x"), Ok(bytes(A)));
    }

    #[test]
    fn names_must_match_exactly() {
        let sums = format!("{A}  x.exe\n{B}  ./y.exe\n");
        assert_eq!(
            digest_for(&sums, "X.EXE"),
            Err(VerifyError::MissingEntry("X.EXE".into()))
        );
        assert!(digest_for(&sums, "y.exe").is_err());
        assert!(digest_for(&sums, "x.exe ").is_err());
    }

    #[test]
    fn refuses_duplicates_and_malformed_lines() {
        let duplicate = format!("{A}  x\n{A}  x\n");
        assert_eq!(
            digest_for(&duplicate, "x"),
            Err(VerifyError::DuplicateEntry("x".into()))
        );
        let conflicting = format!("{A}  x\n{B} *x\n");
        assert!(matches!(
            digest_for(&conflicting, "x"),
            Err(VerifyError::DuplicateEntry(_))
        ));
        for bad in [
            format!("{A} x"),
            format!("{A}\tx"),
            format!("{A}  "),
            format!("{}  x", &A[..63]),
            format!("{}g  x", &A[..63]),
            format!("{A}0  x"),
            "\u{e9}".repeat(40),
        ] {
            assert_eq!(
                digest_for(&format!("{B}  ok\n{bad}\n"), "ok"),
                Err(VerifyError::MalformedSums(2)),
                "{bad:?}"
            );
        }
        assert_eq!(
            digest_for("", "x"),
            Err(VerifyError::MissingEntry("x".into()))
        );
    }

    /// A throwaway minisign key pair and a signer for it.
    pub struct TestKey {
        pair: minisign::KeyPair,
        pub public: PublicKey,
    }

    impl TestKey {
        pub fn new() -> Self {
            let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            let public = PublicKey::from_base64(&pair.pk.to_base64()).unwrap();
            Self { pair, public }
        }

        pub fn sign(&self, data: &[u8]) -> Vec<u8> {
            minisign::sign(Some(&self.pair.pk), &self.pair.sk, data, None, None)
                .unwrap()
                .to_string()
                .into_bytes()
        }
    }

    #[test]
    fn signatures() {
        let key = TestKey::new();
        let data = format!("{A}  x\n").into_bytes();
        let signature = key.sign(&data);
        assert_eq!(verify_signature(&key.public, &data, &signature), Ok(()));

        let mut tampered = data.clone();
        tampered[0] = b'1';
        assert_eq!(
            verify_signature(&key.public, &tampered, &signature),
            Err(VerifyError::BadSignature)
        );
        let other = TestKey::new();
        assert_eq!(
            verify_signature(&other.public, &data, &signature),
            Err(VerifyError::BadSignature)
        );
        for garbage in [&b""[..], b"not a signature", &[0xff, 0xfe]] {
            assert_eq!(
                verify_signature(&key.public, &data, garbage),
                Err(VerifyError::BadSignature)
            );
        }
    }

    #[test]
    fn built_in_key_is_valid_if_set() {
        assert!(signing_key().is_ok());
    }
}
