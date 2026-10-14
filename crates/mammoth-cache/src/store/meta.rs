//! Facts about an entry that every tier keeps (design doc 0010).

use std::time::{SystemTime, UNIX_EPOCH};

/// A checksum of an entry's bytes. A mismatch on read means the stored copy is damaged.
///
/// CRC32C: hardware-accelerated on `x86_64` and `aarch64`, and the checksum the major object
/// stores use. It detects corruption, not tampering; the stores are trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Checksum(pub u32);

impl Checksum {
    /// The checksum of no bytes.
    pub const EMPTY: Self = Self(0);

    /// Extends the checksum with the next chunk of bytes.
    #[must_use]
    pub fn update(self, bytes: &[u8]) -> Self {
        Self(crc32c::crc32c_append(self.0, bytes))
    }

    /// The checksum of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self::EMPTY.update(bytes)
    }
}

/// Facts about an entry that every tier keeps. Written once, never updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryMeta {
    /// Length of the entry's bytes.
    pub size: u64,
    /// Checksum of the entry's bytes.
    pub checksum: Checksum,
    /// When the entry was first stored on any node, in seconds since the Unix epoch.
    pub created: u64,
}

/// Seconds since the Unix epoch, now.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_crc32c_and_composes() {
        // The CRC32C check value from RFC 3720.
        assert_eq!(Checksum::of(b"123456789"), Checksum(0xE306_9283));
        assert_eq!(
            Checksum::EMPTY.update(b"1234").update(b"56789"),
            Checksum::of(b"123456789")
        );
    }
}
