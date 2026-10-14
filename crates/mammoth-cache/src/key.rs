//! Cache keys.
//!
//! Every key carries its protocol's namespace (`gradle/<hex key>`, `maven/<path>`), so a Maven path
//! can never collide with a Gradle hash in any tier (design doc 0003).

use std::fmt;

/// The longest Gradle key accepted, matching `maxLength` in the Gradle `OpenAPI` contract.
pub const MAX_KEY_LEN: usize = 128;

/// The longest Maven path accepted, matching `maxLength` in the Maven `OpenAPI` contract.
pub const MAX_PATH_LEN: usize = 512;

/// The most segments a Maven path may have.
pub const MAX_PATH_SEGMENTS: usize = 16;

/// The protocol a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Namespace {
    /// Gradle's `/cache/{key}`: content-derived hex keys, immutable entries.
    Gradle,
    /// The Apache Maven Build Cache Extension's `/maven/{path}`: named files that a store may
    /// replace, because they are not content-addressed.
    Maven,
}

impl Namespace {
    /// The namespace's prefix in a stored key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gradle => "gradle",
            Self::Maven => "maven",
        }
    }

    /// Whether a stored entry can never change. A store to an existing immutable key writes
    /// nothing; a store to an existing mutable key replaces it.
    #[must_use]
    pub fn is_immutable(self) -> bool {
        matches!(self, Self::Gradle)
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated cache key: a namespace and a name that is safe to use as a file or object path.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey {
    namespace: Namespace,
    /// `<namespace>/<name>`, so stores can use it as is.
    full: Box<str>,
}

impl CacheKey {
    /// Parses a Gradle key: 1 to [`MAX_KEY_LEN`] lowercase hex characters.
    pub fn gradle(key: &str) -> Result<Self, InvalidKey> {
        let valid = !key.is_empty()
            && key.len() <= MAX_KEY_LEN
            && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self::new(Namespace::Gradle, key))
        } else {
            Err(InvalidKey::Gradle)
        }
    }

    /// Parses a Maven path: up to [`MAX_PATH_SEGMENTS`] `/`-separated segments of letters,
    /// digits, `.`, `_` and `-`, at most [`MAX_PATH_LEN`] characters, with no `.` or `..` segment.
    pub fn maven(path: &str) -> Result<Self, InvalidKey> {
        let segment_ok = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        let valid = path.len() <= MAX_PATH_LEN
            && path.split('/').count() <= MAX_PATH_SEGMENTS
            && path.split('/').all(segment_ok);
        if valid {
            Ok(Self::new(Namespace::Maven, path))
        } else {
            Err(InvalidKey::Maven)
        }
    }

    fn new(namespace: Namespace, name: &str) -> Self {
        Self {
            namespace,
            full: format!("{namespace}/{name}").into(),
        }
    }

    /// The protocol the key belongs to.
    #[must_use]
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// The key as the client sent it, without the namespace.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.full[self.namespace.as_str().len() + 1..]
    }

    /// The key as stored: `<namespace>/<name>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.full
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CacheKey({})", self.full)
    }
}

/// A key or path that does not meet its namespace's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidKey {
    /// Not 1 to [`MAX_KEY_LEN`] lowercase hex characters.
    Gradle,
    /// Not a valid Maven path.
    Maven,
}

impl fmt::Display for InvalidKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gradle => write!(
                f,
                "cache key must be 1 to {MAX_KEY_LEN} lowercase hex characters"
            ),
            Self::Maven => f.write_str(
                "path segments may contain only letters, digits, \".\", \"_\" and \"-\"",
            ),
        }
    }
}

impl std::error::Error for InvalidKey {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradle_accepts_lowercase_hex() {
        let key = CacheKey::gradle("a3f5e1c09b2d4e7f").unwrap();
        assert_eq!(key.namespace(), Namespace::Gradle);
        assert_eq!(key.name(), "a3f5e1c09b2d4e7f");
        assert_eq!(key.as_str(), "gradle/a3f5e1c09b2d4e7f");
    }

    #[test]
    fn gradle_rejects_everything_else() {
        let too_long = "a".repeat(MAX_KEY_LEN + 1);
        for bad in ["", "A3F5", "xyz", "../etc", "a3 f5", too_long.as_str()] {
            assert_eq!(CacheKey::gradle(bad), Err(InvalidKey::Gradle), "{bad:?}");
        }
    }

    #[test]
    fn maven_accepts_extension_paths() {
        let path = "v1.2/com.example/app/0123456789abcdef/app-tests.jar";
        let key = CacheKey::maven(path).unwrap();
        assert_eq!(key.name(), path);
        assert_eq!(key.as_str(), format!("maven/{path}"));
    }

    #[test]
    fn maven_rejects_traversal_and_odd_characters() {
        let too_deep = vec!["a"; MAX_PATH_SEGMENTS + 1].join("/");
        let too_long = "a".repeat(MAX_PATH_LEN + 1);
        for bad in [
            "",
            "..",
            "v1/../etc",
            "v1/./x",
            "/v1/x",
            "v1//x",
            "v1/x/",
            "v1/a b",
            "v1/a%2f",
            too_deep.as_str(),
            too_long.as_str(),
        ] {
            assert_eq!(CacheKey::maven(bad), Err(InvalidKey::Maven), "{bad:?}");
        }
    }

    #[test]
    fn namespaces_never_collide() {
        assert_ne!(
            CacheKey::gradle("abc").unwrap(),
            CacheKey::maven("abc").unwrap()
        );
    }
}
