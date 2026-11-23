//! Portable local contract primitives. No trust authority is implied.
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path};

pub const JSON_INTEGER_MAX: u64 = 9_007_199_254_740_991;

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContractError {
    #[error("invalid identifier: {0}")]
    Id(String),
    #[error("invalid digest: {0}")]
    Digest(String),
    #[error("invalid relative path: {0}")]
    RelativePath(String),
    #[error("unsupported request: {0}")]
    Unsupported(String),
    #[error("invalid configuration: {0}")]
    Configuration(String),
    #[error("JSON integer out of range: {0}")]
    OutOfRange(u64),
}

/// # Errors
/// Validation rejects malformed primitive values without normalising them.
pub trait Validate {
    /// Reports stable syntax or wire rejections with `ContractError` rather than silently correcting input.
    ///
    /// # Errors
    /// The value violates the declared versioned primitive or structure boundary.
    fn validate(&self) -> Result<(), ContractError>;
}
pub(crate) fn format_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut encoded, byte| {
            for nibble in [byte >> 4, byte & 0x0f] {
                encoded.push(char::from(b"0123456789abcdef"[usize::from(nibble)]));
            }
            encoded
        },
    )
}

fn valid_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i != 0 && matches!(b, b'.' | b'_' | b'-')))
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(String);
impl Id {
    /// Rejects empty, malformed or non-ASCII identifiers.
    ///
    /// # Errors
    /// Invalid syntax is reported without trimming or normalizing the caller value.
    pub fn new(text: impl Into<String>) -> Result<Self, ContractError> {
        let text = text.into();
        if valid_id(&text) {
            Ok(Self(text))
        } else {
            Err(ContractError::Id(text))
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Validate for Id {
    fn validate(&self) -> Result<(), ContractError> {
        Self::new(&self.0).map(|_| ())
    }
}
impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Id").field(&self.0).finish()
    }
}
impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl Serialize for Id {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        if !s.is_ascii() {
            return Err(serde::de::Error::custom("Ids must be ASCII"));
        }
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DigestAlgorithm {
    Blake3,
    Sha256,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    algorithm: DigestAlgorithm,
    hex: String,
}
impl Digest {
    /// Rejects missing algorithm prefix, uppercase, malformed-length/nonhex digest characters.
    ///
    /// # Errors
    /// The text must start `b3:`/`sha256:` and contain exactly 64 low hex digits.
    pub fn new(text: &str) -> Result<Self, ContractError> {
        let (algorithm, hex) = text
            .strip_prefix("b3:")
            .map(|v| (DigestAlgorithm::Blake3, v))
            .or_else(|| {
                text.strip_prefix("sha256:")
                    .map(|v| (DigestAlgorithm::Sha256, v))
            })
            .ok_or_else(|| ContractError::Digest(text.into()))?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ContractError::Digest(text.into()));
        }
        Ok(Self {
            algorithm,
            hex: hex.into(),
        })
    }
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.hex
    }
    #[must_use]
    pub fn algorithm(&self) -> &DigestAlgorithm {
        &self.algorithm
    }
    fn verify_domain(domain: &str) -> Result<(), ContractError> {
        if domain.is_empty() || domain.contains('\0') {
            Err(ContractError::Digest("invalid domain".into()))
        } else {
            Ok(())
        }
    }
    /// # Errors
    /// Empty or NUL-containing domains are refused.
    pub fn blake3(domain: &str, data: &[u8]) -> Result<Self, ContractError> {
        Self::verify_domain(domain)?;
        let mut h = blake3::Hasher::new();
        h.update(domain.as_bytes());
        h.update(&[0]);
        h.update(data);
        Ok(Self {
            algorithm: DigestAlgorithm::Blake3,
            hex: h.finalize().to_hex().to_string(),
        })
    }
    /// Hashes helper equality with explicit SHA-256.
    ///
    /// # Errors
    /// Rejects domains that contain no separator or a NUL.
    pub fn sha256(domain: &str, data: &[u8]) -> Result<Self, ContractError> {
        use sha2::Digest as _;
        Self::verify_domain(domain)?;
        let mut h = sha2::Sha256::new();
        h.update(domain.as_bytes());
        h.update([0]);
        h.update(data);
        Ok(Self {
            algorithm: DigestAlgorithm::Sha256,
            hex: format!("{:x}", h.finalize()),
        })
    }
}
impl Validate for Digest {
    fn validate(&self) -> Result<(), ContractError> {
        Self::new(&self.to_string()).map(|_| ())
    }
}
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.algorithm {
            DigestAlgorithm::Blake3 => "b3:",
            DigestAlgorithm::Sha256 => "sha256:",
        })?;
        f.write_str(&self.hex)
    }
}
impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest(\"{self}\")")
    }
}
impl Serialize for Digest {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelPath(String);
impl RelPath {
    /// Rejects empty, malformed or non-ASCII identifiers.
    ///
    /// # Errors
    /// Invalid syntax is reported without trimming or normalizing the caller value.
    pub fn new(text: impl Into<String>) -> Result<Self, ContractError> {
        let text = text.into();
        if text.is_empty()
            || text.starts_with('/')
            || text.starts_with('~')
            || text.contains('\\')
            || text.contains(':')
            || text.ends_with('/')
            || text.chars().any(char::is_control)
            || Path::new(&text).is_absolute()
            || text
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return Err(ContractError::RelativePath(text));
        }
        Ok(Self(text))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Validate for RelPath {
    fn validate(&self) -> Result<(), ContractError> {
        Self::new(&self.0).map(|_| ())
    }
}
impl AsRef<str> for RelPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl Serialize for RelPath {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub digest: Digest,
    pub size_bytes: u64,
    pub media_type: String,
}
impl Validate for BlobRef {
    fn validate(&self) -> Result<(), ContractError> {
        check_range(self.size_bytes)?;
        self.digest.validate()?;
        if self.media_type.is_empty()
            || !self.media_type.is_ascii()
            || self.media_type.chars().any(char::is_control)
        {
            return Err(ContractError::Configuration(
                "blob media type invalid".into(),
            ));
        }
        Ok(())
    }
}

/// Checks that an integer is accepted by the wire.
///
/// # Errors
/// Values over the defined portable JSON range are refused.
pub fn check_range(value: u64) -> Result<(), ContractError> {
    if value <= JSON_INTEGER_MAX {
        Ok(())
    } else {
        Err(ContractError::OutOfRange(value))
    }
}
/// Produces an exact local document digest, not a signature.
///
/// # Errors
/// Invalid digest domains fail without reading any secret or environment input.
pub fn exact_digest(domain: &str, data: &[u8]) -> Result<Digest, ContractError> {
    Digest::blake3(domain, data)
}

impl std::str::FromStr for RelPath {
    type Err = ContractError;
    fn from_str(source: &str) -> Result<Self, Self::Err> {
        Self::new(source)
    }
}
