//! Cache keys.
//!
//! Every key carries its protocol's namespace (`gradle/<hex key>`, `maven/<path>`,
//! `bazel-ac/<sha256>`, `bazel-cas/<sha256>`), so a Maven path can never collide with a Gradle hash
//! in any tier (design doc 0003).

use std::fmt;

/// The longest Gradle key accepted, matching `maxLength` in the Gradle `OpenAPI` contract.
pub const MAX_KEY_LEN: usize = 128;

/// The longest Maven path accepted, matching `maxLength` in the Maven `OpenAPI` contract.
pub const MAX_PATH_LEN: usize = 512;

/// The most segments a Maven path may have.
pub const MAX_PATH_SEGMENTS: usize = 16;

/// The length of a Bazel digest: a SHA-256 hash in lowercase hex.
pub const BAZEL_DIGEST_LEN: usize = 64;

/// The protocol a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Namespace {
    /// Gradle's `/cache/{key}`: content-derived hex keys, immutable entries.
    Gradle,
    /// The Apache Maven Build Cache Extension's `/maven/{path}`: named files that a store may
    /// replace, because they are not content-addressed.
    Maven,
    /// Bazel's action cache, `/bazel/ac/{digest}`: action results keyed by the action's digest. A
    /// store replaces the entry, as in bazel-remote, because a non-hermetic action can produce a
    /// different result for the same digest.
    BazelAc,
    /// Bazel's content-addressed store, `/bazel/cas/{digest}`: blobs keyed by their own SHA-256,
    /// immutable.
    BazelCas,
}

impl Namespace {
    /// The namespace's prefix in a stored key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gradle => "gradle",
            Self::Maven => "maven",
            Self::BazelAc => "bazel-ac",
            Self::BazelCas => "bazel-cas",
        }
    }

    /// Whether a stored entry can never change. A store to an existing immutable key writes
    /// nothing; a store to an existing mutable key replaces it.
    #[must_use]
    pub fn is_immutable(self) -> bool {
        matches!(self, Self::Gradle | Self::BazelCas)
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

    /// Parses a Bazel digest for the action cache ([`Namespace::BazelAc`]) or the
    /// content-addressed store ([`Namespace::BazelCas`]): [`BAZEL_DIGEST_LEN`] lowercase hex
    /// characters.
    pub fn bazel(namespace: Namespace, digest: &str) -> Result<Self, InvalidKey> {
        let valid = matches!(namespace, Namespace::BazelAc | Namespace::BazelCas)
            && digest.len() == BAZEL_DIGEST_LEN
            && digest
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self::new(namespace, digest))
        } else {
            Err(InvalidKey::Bazel)
        }
    }

    /// Parses a key as stored, `<namespace>/<name>` (the inverse of [`CacheKey::as_str`]), with
    /// the same validation as the constructors. For tiers that rebuild their index from what
    /// they hold.
    #[must_use]
    pub fn from_stored(stored: &str) -> Option<Self> {
        let (namespace, name) = stored.split_once('/')?;
        match namespace {
            "gradle" => Self::gradle(name).ok(),
            "maven" => Self::maven(name).ok(),
            "bazel-ac" => Self::bazel(Namespace::BazelAc, name).ok(),
            "bazel-cas" => Self::bazel(Namespace::BazelCas, name).ok(),
            _ => None,
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
    /// Not [`BAZEL_DIGEST_LEN`] lowercase hex characters.
    Bazel,
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
            Self::Bazel => write!(
                f,
                "digest must be a SHA-256 hash: {BAZEL_DIGEST_LEN} lowercase hex characters"
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
    fn bazel_accepts_sha256_digests() {
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let ac = CacheKey::bazel(Namespace::BazelAc, digest).unwrap();
        let cas = CacheKey::bazel(Namespace::BazelCas, digest).unwrap();
        assert_eq!(ac.as_str(), format!("bazel-ac/{digest}"));
        assert_eq!(cas.as_str(), format!("bazel-cas/{digest}"));
        assert_eq!(cas.name(), digest);
        assert_ne!(ac, cas);
        assert!(cas.namespace().is_immutable() && !ac.namespace().is_immutable());
    }

    #[test]
    fn bazel_rejects_everything_else() {
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let upper = digest.to_uppercase();
        for bad in [
            "",
            "abc",
            &digest[1..],
            upper.as_str(),
            &format!("{digest}0"),
        ] {
            assert_eq!(
                CacheKey::bazel(Namespace::BazelCas, bad),
                Err(InvalidKey::Bazel),
                "{bad:?}"
            );
        }
        assert_eq!(
            CacheKey::bazel(Namespace::Gradle, digest),
            Err(InvalidKey::Bazel)
        );
    }

    #[test]
    fn every_namespace_round_trips_through_its_stored_form() {
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let keys = [
            Namespace::Gradle,
            Namespace::Maven,
            Namespace::BazelAc,
            Namespace::BazelCas,
        ]
        .map(|namespace| {
            // Exhaustive, so a new namespace fails to compile until it is added here.
            match namespace {
                Namespace::Gradle => CacheKey::gradle("abc123").unwrap(),
                Namespace::Maven => CacheKey::maven("v1.1/g/a/cs/buildinfo.xml").unwrap(),
                Namespace::BazelAc | Namespace::BazelCas => {
                    CacheKey::bazel(namespace, digest).unwrap()
                }
            }
        });
        for key in keys {
            assert_eq!(
                CacheKey::from_stored(key.as_str()),
                Some(key.clone()),
                "{key}"
            );
        }
        for bad in ["", "gradle", "gradle/XYZ", "nope/abc", "bazel-cas/abc"] {
            assert_eq!(CacheKey::from_stored(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn namespaces_never_collide() {
        assert_ne!(
            CacheKey::gradle("abc").unwrap(),
            CacheKey::maven("abc").unwrap()
        );
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #[test]
            fn gradle_accepts_exactly_lowercase_hex(key in "[0-9a-zA-Z./%-]{0,140}") {
                let expected = !key.is_empty()
                    && key.len() <= MAX_KEY_LEN
                    && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                prop_assert_eq!(CacheKey::gradle(&key).is_ok(), expected);
            }

            #[test]
            fn maven_never_accepts_a_path_that_escapes(path in "[a-z./%-]{0,40}(/[a-z./%-]{0,8}){0,4}") {
                if let Ok(key) = CacheKey::maven(&path) {
                    prop_assert_eq!(key.name(), path.as_str());
                    prop_assert!(!path.starts_with('/'));
                    prop_assert!(path.split('/').all(|s| !s.is_empty() && s != "." && s != ".."));
                    prop_assert!(!path.contains('%'));
                }
            }
        }
    }
}
