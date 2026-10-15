//! Who may do what (design doc 0002).
//!
//! Users are listed in the config file with the SHA-256 hashes of their tokens and a set of
//! scopes. A request carries a token, as the password of HTTP Basic auth or as a Bearer token, or
//! nothing and gets the anonymous scopes. Nothing here knows about HTTP.

use std::fmt;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The prefix of every token, so secret scanners can recognise a leaked one.
pub const TOKEN_PREFIX: &str = "mmc_";

/// A permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum Scope {
    /// `GET` and `HEAD` on cache entries.
    #[serde(rename = "cache:read")]
    CacheRead,
    /// `PUT` on cache entries.
    #[serde(rename = "cache:write")]
    CacheWrite,
    /// `/metrics`, when metrics require authentication.
    #[serde(rename = "metrics:read")]
    MetricsRead,
    /// The admin API.
    #[serde(rename = "admin:read")]
    AdminRead,
}

impl Scope {
    /// Every scope.
    pub const ALL: [Self; 4] = [
        Self::CacheRead,
        Self::CacheWrite,
        Self::MetricsRead,
        Self::AdminRead,
    ];

    /// The scope's name in the config file and in errors.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CacheRead => "cache:read",
            Self::CacheWrite => "cache:write",
            Self::MetricsRead => "metrics:read",
            Self::AdminRead => "admin:read",
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of scopes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scopes(u8);

impl Scopes {
    /// Every scope.
    pub const ALL: Self = Self(0b1111);

    /// Whether the set holds `scope`.
    #[must_use]
    pub fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }
}

impl FromIterator<Scope> for Scopes {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        Self(iter.into_iter().fold(0, |bits, scope| bits | scope.bit()))
    }
}

/// A token's SHA-256 hash.
pub type TokenHash = [u8; 32];

/// The SHA-256 hash of a token, as stored in the config file.
#[must_use]
pub fn hash_token(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

/// A hash as the config file writes it: `sha256:` and 64 lowercase hex digits.
#[must_use]
pub fn format_hash(hash: &TokenHash) -> String {
    use fmt::Write;
    hash.iter().fold(String::from("sha256:"), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Credentials as a request presents them.
#[derive(Clone, Copy)]
pub enum Credentials<'a> {
    /// HTTP Basic: the user name and the token as the password.
    Basic {
        /// The user name.
        user: &'a str,
        /// The token.
        token: &'a str,
    },
    /// A Bearer token, which names no user.
    Bearer {
        /// The token.
        token: &'a str,
    },
}

impl fmt::Debug for Credentials<'_> {
    // Never print a token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { user, .. } => write!(f, "Basic({user})"),
            Self::Bearer { .. } => f.write_str("Bearer"),
        }
    }
}

/// Who a request is, and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The user name; `None` for a request without credentials.
    pub user: Option<String>,
    /// What the request may do.
    pub scopes: Scopes,
}

impl Principal {
    /// The name for logs: the user, or `anonymous`.
    #[must_use]
    pub fn name(&self) -> &str {
        self.user.as_deref().unwrap_or("anonymous")
    }
}

/// Credentials that match no user: an unknown user, or a wrong token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidCredentials;

#[derive(Debug, Clone)]
struct User {
    name: String,
    hashes: Vec<TokenHash>,
    scopes: Scopes,
}

/// Checks credentials against the configured users.
#[derive(Debug, Clone)]
pub struct Authenticator {
    users: Vec<User>,
    anonymous: Scopes,
    protect_metrics: bool,
}

impl Authenticator {
    /// Allows everything to everyone: the node has no users.
    #[must_use]
    pub fn open() -> Self {
        Self {
            users: Vec::new(),
            anonymous: Scopes::ALL,
            protect_metrics: false,
        }
    }

    /// An authenticator for `users`, given as name, token hashes and scopes.
    #[must_use]
    pub fn new(
        anonymous: Scopes,
        users: impl IntoIterator<Item = (String, Vec<TokenHash>, Scopes)>,
        protect_metrics: bool,
    ) -> Self {
        Self {
            users: users
                .into_iter()
                .map(|(name, hashes, scopes)| User {
                    name,
                    hashes,
                    scopes,
                })
                .collect(),
            anonymous,
            protect_metrics,
        }
    }

    /// Whether `/metrics` needs [`Scope::MetricsRead`].
    #[must_use]
    pub fn protects_metrics(&self) -> bool {
        self.protect_metrics
    }

    /// Whether any request is refused at all.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.users.is_empty() && self.anonymous == Scopes::ALL && !self.protect_metrics
    }

    /// Finds who presented `credentials`. No credentials is the anonymous principal.
    pub fn authenticate(
        &self,
        credentials: Option<Credentials<'_>>,
    ) -> Result<Principal, InvalidCredentials> {
        // With no users and everything open to anonymous clients, no credentials could ever match,
        // and refusing them would break clients that keep credentials configured, as Build Cache
        // Node migrants and Bazel URLs with user:pass often do. They are ignored instead.
        let open = self.users.is_empty() && self.anonymous == Scopes::ALL;
        let user = match credentials {
            None => {
                return Ok(Principal {
                    user: None,
                    scopes: self.anonymous,
                });
            }
            Some(_) if open => {
                return Ok(Principal {
                    user: None,
                    scopes: self.anonymous,
                });
            }
            Some(Credentials::Basic { user, token }) => {
                let hash = hash_token(token);
                self.users
                    .iter()
                    .find(|u| u.name == user)
                    .filter(|u| matches(&u.hashes, &hash))
            }
            Some(Credentials::Bearer { token }) => {
                let hash = hash_token(token);
                self.users.iter().find(|u| matches(&u.hashes, &hash))
            }
        };
        let user = user.ok_or(InvalidCredentials)?;
        Ok(Principal {
            user: Some(user.name.clone()),
            scopes: user.scopes,
        })
    }
}

/// Whether `hash` is one of `hashes`, comparing every one in constant time.
fn matches(hashes: &[TokenHash], hash: &TokenHash) -> bool {
    hashes
        .iter()
        .fold(subtle::Choice::from(0), |found, h| found | h.ct_eq(hash))
        .into()
}

/// A new token: [`TOKEN_PREFIX`] and 32 random bytes in base62.
pub fn new_token() -> Result<String, getrandom::Error> {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    // 43 base62 digits carry 256 bits. Rejection sampling keeps every digit uniform.
    let mut token = String::from(TOKEN_PREFIX);
    let mut buf = [0_u8; 64];
    while token.len() < TOKEN_PREFIX.len() + 43 {
        getrandom::fill(&mut buf)?;
        for &b in &buf {
            if b < 248 && token.len() < TOKEN_PREFIX.len() + 43 {
                token.push(char::from(ALPHABET[usize::from(b % 62)]));
            }
        }
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Authenticator {
        Authenticator::new(
            Scopes::default(),
            [
                (
                    "ci".to_owned(),
                    vec![hash_token("old"), hash_token("ci-token")],
                    [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
                ),
                (
                    "dev".to_owned(),
                    vec![hash_token("dev-token")],
                    [Scope::CacheRead].into_iter().collect(),
                ),
            ],
            false,
        )
    }

    #[test]
    fn basic_needs_the_right_user_and_token() {
        let auth = auth();
        let ci = auth
            .authenticate(Some(Credentials::Basic {
                user: "ci",
                token: "ci-token",
            }))
            .unwrap();
        assert_eq!(ci.name(), "ci");
        assert!(ci.scopes.contains(Scope::CacheWrite));
        for (user, token) in [("ci", "dev-token"), ("nobody", "ci-token"), ("ci", "")] {
            let creds = Credentials::Basic { user, token };
            assert_eq!(auth.authenticate(Some(creds)), Err(InvalidCredentials));
        }
    }

    #[test]
    fn any_listed_token_works_for_rotation() {
        let creds = Credentials::Basic {
            user: "ci",
            token: "old",
        };
        assert!(auth().authenticate(Some(creds)).is_ok());
    }

    #[test]
    fn bearer_finds_the_user_by_token() {
        let dev = auth()
            .authenticate(Some(Credentials::Bearer { token: "dev-token" }))
            .unwrap();
        assert_eq!(dev.user.as_deref(), Some("dev"));
        assert!(!dev.scopes.contains(Scope::CacheWrite));
    }

    #[test]
    fn an_open_node_ignores_credentials() {
        let open = Authenticator::open();
        for creds in [
            Credentials::Basic {
                user: "ci",
                token: "secret",
            },
            Credentials::Bearer { token: "secret" },
        ] {
            let principal = open.authenticate(Some(creds)).unwrap();
            assert_eq!(principal.name(), "anonymous");
            assert_eq!(principal.scopes, Scopes::ALL);
        }
        // An empty `[auth]` table opens nothing, so credentials are still refused there.
        let closed = Authenticator::new(Scopes::default(), [], false);
        let creds = Credentials::Bearer { token: "secret" };
        assert_eq!(closed.authenticate(Some(creds)), Err(InvalidCredentials));
    }

    #[test]
    fn no_credentials_get_the_anonymous_scopes() {
        let anon = auth().authenticate(None).unwrap();
        assert_eq!(anon.name(), "anonymous");
        assert_eq!(anon.scopes, Scopes::default());
        assert!(
            Authenticator::open()
                .authenticate(None)
                .unwrap()
                .scopes
                .contains(Scope::AdminRead)
        );
    }

    #[test]
    fn tokens_are_prefixed_unique_and_base62() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_PREFIX.len() + 43);
        assert!(
            a[TOKEN_PREFIX.len()..]
                .bytes()
                .all(|c| c.is_ascii_alphanumeric())
        );
    }

    #[test]
    fn hashes_match_the_config_format() {
        // sha256("test"), the hash in the design doc's example.
        assert_eq!(
            format_hash(&hash_token("test")),
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }
}
