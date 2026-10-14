//! Cache keys.

use std::fmt;

/// The longest key accepted, matching `maxLength` in the Gradle `OpenAPI` contract.
pub const MAX_KEY_LEN: usize = 128;

/// A validated cache key: a non-empty lowercase hex string of at most [`MAX_KEY_LEN`] characters.
///
/// Gradle computes keys from task inputs, so a key says nothing about the bytes stored under it.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey(Box<str>);

impl CacheKey {
    /// Parses a key, rejecting anything that is not lowercase hex.
    pub fn parse(s: &str) -> Result<Self, InvalidKey> {
        let valid = !s.is_empty()
            && s.len() <= MAX_KEY_LEN
            && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(s.into()))
        } else {
            Err(InvalidKey)
        }
    }

    /// The key as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CacheKey({})", self.0)
    }
}

/// A key that is not lowercase hex, or is empty or too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidKey;

impl fmt::Display for InvalidKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cache key must be 1 to {MAX_KEY_LEN} lowercase hex characters"
        )
    }
}

impl std::error::Error for InvalidKey {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lowercase_hex() {
        let key = CacheKey::parse("a3f5e1c09b2d4e7f").unwrap();
        assert_eq!(key.as_str(), "a3f5e1c09b2d4e7f");
    }

    #[test]
    fn rejects_everything_else() {
        let too_long = "a".repeat(MAX_KEY_LEN + 1);
        for bad in ["", "A3F5", "xyz", "../etc", "a3 f5", too_long.as_str()] {
            assert_eq!(CacheKey::parse(bad), Err(InvalidKey), "{bad:?}");
        }
    }
}
