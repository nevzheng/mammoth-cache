//! REAPI digests and `ByteStream` resource names, mapped onto the Bazel HTTP cache's keys, so both
//! adapters share entries (design doc 0016).
//!
//! Instance names are accepted and ignored: every instance shares one cache, as in bazel-remote.

use tonic::Status;

use super::{
    proto::build::bazel::remote::execution::v2::{Digest, digest_function},
    status,
};
use crate::key::{CacheKey, Namespace};

/// The SHA-256 of nothing. A client may assume the empty blob is always present, so it is never
/// stored or looked up.
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A validated digest: the cache key it maps to and the size it declares.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Blob {
    pub(crate) key: CacheKey,
    pub(crate) size: u64,
}

impl Blob {
    /// A CAS blob.
    pub(crate) fn cas(digest: Option<&Digest>) -> Result<Self, Status> {
        Self::new(Namespace::BazelCas, digest)
    }

    /// An action cache entry, keyed by the action's digest.
    pub(crate) fn action(digest: Option<&Digest>) -> Result<Self, Status> {
        Self::new(Namespace::BazelAc, digest)
    }

    fn new(namespace: Namespace, digest: Option<&Digest>) -> Result<Self, Status> {
        let digest = digest
            .ok_or_else(|| status::invalid_argument("INVALID_DIGEST", "The digest is missing."))?;
        let size = u64::try_from(digest.size_bytes).map_err(|_| {
            status::invalid_argument("INVALID_DIGEST", "The digest's size is negative.")
        })?;
        Self::parse(namespace, &digest.hash, size)
    }

    fn parse(namespace: Namespace, hash: &str, size: u64) -> Result<Self, Status> {
        let key = CacheKey::bazel(namespace, hash).map_err(|_| {
            status::invalid_argument(
                "INVALID_DIGEST",
                "A digest's hash must be a SHA-256 in 64 lowercase hex characters.",
            )
        })?;
        if namespace == Namespace::BazelCas && (size == 0) != (hash == EMPTY_SHA256) {
            return Err(status::invalid_argument(
                "INVALID_DIGEST",
                "Only the empty blob has size 0.",
            ));
        }
        Ok(Self { key, size })
    }

    /// Whether this is the empty blob, which is always present.
    pub(crate) fn is_empty(&self) -> bool {
        self.size == 0 && self.key.namespace() == Namespace::BazelCas
    }

    /// The digest as the client sent it.
    pub(crate) fn digest(&self) -> Digest {
        Digest {
            hash: self.key.name().to_owned(),
            size_bytes: i64::try_from(self.size).unwrap_or(i64::MAX),
        }
    }

    /// The 32 bytes of the hash.
    pub(crate) fn sha256(&self) -> [u8; 32] {
        let nibble = |b: u8| match b {
            b'0'..=b'9' => b - b'0',
            _ => b - b'a' + 10,
        };
        let mut out = [0_u8; 32];
        for (byte, [high, low]) in out
            .iter_mut()
            .zip(self.key.name().as_bytes().as_chunks::<2>().0)
        {
            *byte = (nibble(*high) << 4) | nibble(*low);
        }
        out
    }
}

/// Refuses a request for a digest function other than SHA-256. Unset means the server's default,
/// which is SHA-256.
pub(crate) fn check_function(function: i32) -> Result<(), Status> {
    match digest_function::Value::try_from(function) {
        Ok(digest_function::Value::Unknown | digest_function::Value::Sha256) => Ok(()),
        _ => Err(status::invalid_argument(
            "UNSUPPORTED_DIGEST_FUNCTION",
            "Only SHA-256 digests are supported.",
        )),
    }
}

/// Parses a `ByteStream.Read` resource name: `[{instance_name}/]blobs/{hash}/{size}`.
pub(crate) fn read_resource(name: &str) -> Result<Blob, Status> {
    let segments: Vec<&str> = name.split('/').collect();
    let at = segments
        .iter()
        .position(|s| *s == "blobs" || *s == "compressed-blobs")
        .ok_or_else(|| invalid_resource(name))?;
    match &segments[at..] {
        ["blobs", hash, size] => Blob::parse(Namespace::BazelCas, hash, parse_size(name, size)?),
        ["compressed-blobs", ..] => Err(compressed()),
        _ => Err(invalid_resource(name)),
    }
}

/// Parses a `ByteStream.Write` resource name:
/// `[{instance_name}/]uploads/{uuid}/blobs/{hash}/{size}[/{metadata}]`.
pub(crate) fn write_resource(name: &str) -> Result<Blob, Status> {
    let segments: Vec<&str> = name.split('/').collect();
    let at = segments
        .iter()
        .position(|s| *s == "uploads")
        .ok_or_else(|| invalid_resource(name))?;
    match &segments[at..] {
        ["uploads", uuid, "blobs", hash, size, ..] if !uuid.is_empty() => {
            Blob::parse(Namespace::BazelCas, hash, parse_size(name, size)?)
        }
        ["uploads", _, "compressed-blobs", ..] => Err(compressed()),
        _ => Err(invalid_resource(name)),
    }
}

fn parse_size(name: &str, size: &str) -> Result<u64, Status> {
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid_resource(name));
    }
    size.parse().map_err(|_| invalid_resource(name))
}

fn invalid_resource(name: &str) -> Status {
    let shown: String = name.chars().take(200).collect();
    status::invalid_argument(
        "INVALID_RESOURCE_NAME",
        format!("{shown:?} is not a valid blob resource name."),
    )
}

fn compressed() -> Status {
    status::invalid_argument(
        "UNSUPPORTED_COMPRESSOR",
        "Compressed blobs are not supported; GetCapabilities lists no compressors.",
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    const HASH: &str = "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";

    fn digest(hash: &str, size: i64) -> Digest {
        Digest {
            hash: hash.to_owned(),
            size_bytes: size,
        }
    }

    #[test]
    fn digests_map_to_bazel_keys() {
        let blob = Blob::cas(Some(&digest(HASH, 3))).unwrap();
        assert_eq!(blob.key.as_str(), format!("bazel-cas/{HASH}"));
        assert_eq!(blob.size, 3);
        assert_eq!(blob.digest(), digest(HASH, 3));
        let action = Blob::action(Some(&digest(HASH, 3))).unwrap();
        assert_eq!(action.key.as_str(), format!("bazel-ac/{HASH}"));
        assert_eq!(hex(&blob.sha256()), HASH);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    #[test]
    fn rejects_bad_digests() {
        assert!(Blob::cas(None).is_err());
        assert!(Blob::cas(Some(&digest(HASH, -1))).is_err());
        assert!(Blob::cas(Some(&digest(&HASH.to_uppercase(), 3))).is_err());
        assert!(Blob::cas(Some(&digest("abc", 3))).is_err());
        assert!(Blob::cas(Some(&digest(HASH, 0))).is_err());
        assert!(Blob::cas(Some(&digest(EMPTY_SHA256, 1))).is_err());
        assert!(
            Blob::cas(Some(&digest(EMPTY_SHA256, 0)))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parses_read_resources() {
        let blob = read_resource(&format!("blobs/{HASH}/3")).unwrap();
        assert_eq!((blob.key.name(), blob.size), (HASH, 3));
        let blob = read_resource(&format!("my/instance/blobs/{HASH}/3")).unwrap();
        assert_eq!(blob.size, 3);
        for bad in [
            format!("blobs/{HASH}"),
            format!("blobs/{HASH}/3/extra"),
            format!("blobs/{HASH}/-3"),
            format!("blobs/{HASH}/+3"),
            format!("blobs/{HASH}/x"),
            "nothing".to_owned(),
            format!("compressed-blobs/zstd/{HASH}/3"),
        ] {
            assert!(read_resource(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_write_resources() {
        let uuid = "4b4ffb9b-91e6-4a07-9b2a-0b0a4e5bd6b3";
        let blob = write_resource(&format!("uploads/{uuid}/blobs/{HASH}/3")).unwrap();
        assert_eq!(blob.size, 3);
        let blob =
            write_resource(&format!("inst/uploads/{uuid}/blobs/{HASH}/3/meta/data")).unwrap();
        assert_eq!(blob.size, 3);
        for bad in [
            format!("blobs/{HASH}/3"),
            format!("uploads//blobs/{HASH}/3"),
            format!("uploads/{uuid}/blobs/{HASH}"),
            format!("uploads/{uuid}/compressed-blobs/zstd/{HASH}/3"),
        ] {
            assert!(write_resource(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_sha256() {
        assert!(check_function(0).is_ok());
        assert!(check_function(digest_function::Value::Sha256 as i32).is_ok());
        assert!(check_function(digest_function::Value::Blake3 as i32).is_err());
        assert!(check_function(999).is_err());
    }

    mod properties {
        use std::fmt::Write;

        use proptest::prelude::*;

        use super::super::*;

        /// Instance names: zero to three segments that are not reserved words.
        fn instance() -> impl Strategy<Value = String> {
            proptest::collection::vec("[a-z0-9_-]{1,8}", 0..3).prop_map(|segments| {
                segments.into_iter().fold(String::new(), |mut out, s| {
                    out.push_str(&s);
                    out.push('/');
                    out
                })
            })
        }

        proptest! {
            #[test]
            fn read_names_round_trip(
                instance in instance(),
                hash in "[0-9a-f]{64}",
                size in 1_u64..u64::MAX,
            ) {
                let blob = read_resource(&format!("{instance}blobs/{hash}/{size}")).unwrap();
                prop_assert_eq!(blob.key.name(), hash.as_str());
                prop_assert_eq!(blob.size, size);
            }

            #[test]
            fn write_names_round_trip(
                instance in instance(),
                uuid in "[0-9a-f-]{1,36}",
                hash in "[0-9a-f]{64}",
                size in 1_u64..u64::MAX,
                metadata in proptest::option::of("[a-z/]{1,20}"),
            ) {
                let tail = metadata.map(|m| format!("/{m}")).unwrap_or_default();
                let name = format!("{instance}uploads/{uuid}/blobs/{hash}/{size}{tail}");
                let blob = write_resource(&name).unwrap();
                prop_assert_eq!(blob.key.name(), hash.as_str());
                prop_assert_eq!(blob.size, size);
            }

            #[test]
            fn any_name_parses_or_is_refused(name in ".{0,200}") {
                // Never panics, and anything accepted has a valid key.
                if let Ok(blob) = read_resource(&name) {
                    prop_assert_eq!(blob.key.name().len(), 64);
                }
                if let Ok(blob) = write_resource(&name) {
                    prop_assert_eq!(blob.key.name().len(), 64);
                }
            }

            #[test]
            fn digests_keep_their_hash_bytes(bytes in proptest::array::uniform32(any::<u8>())) {
                let hash = bytes.iter().fold(String::new(), |mut s, b| {
                    let _ = write!(s, "{b:02x}");
                    s
                });
                let blob = Blob::cas(Some(&Digest { hash, size_bytes: 1 })).unwrap();
                prop_assert_eq!(blob.sha256(), bytes);
            }
        }
    }
}
