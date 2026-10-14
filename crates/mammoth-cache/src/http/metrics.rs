//! Request metrics, labelled by client (design doc 0004).
//!
//! `client` comes from the `User-Agent` prefix and `gradle_version` from `X-Gradle-Version`.
//! Clients choose both, so they label metrics and logs and nothing else. Every label value is
//! drawn from a small fixed set or normalised, so a client cannot create unbounded series.

use axum::{
    extract::{MatchedPath, Request},
    http::{HeaderMap, Method},
    middleware::Next,
    response::Response,
};

/// The kind of client that sent a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    /// Gradle (`Gradle/…`).
    Gradle,
    /// Apache Maven (`Apache-Maven/…`).
    Maven,
    /// Anything else, or no `User-Agent`.
    Other,
}

impl Client {
    /// Classifies a request by its `User-Agent`.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let agent = headers
            .get(axum::http::header::USER_AGENT)
            .map_or(&b""[..], |v| v.as_bytes());
        if agent.starts_with(b"Gradle/") {
            Self::Gradle
        } else if agent.starts_with(b"Apache-Maven/") {
            Self::Maven
        } else {
            Self::Other
        }
    }

    /// The metric label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gradle => "gradle",
            Self::Maven => "maven",
            Self::Other => "other",
        }
    }
}

/// The `gradle_version` label: major and minor from `X-Gradle-Version` (`8.10.2` becomes `8.10`),
/// `other` if it does not parse, or `None` without the header.
#[must_use]
pub fn gradle_version(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get("x-gradle-version")?
        .to_str()
        .unwrap_or_default();
    let mut parts = value.split(['.', '-', '+']);
    let numeric = |s: &&str| !s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit());
    let label = match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) if numeric(&major) && numeric(&minor) => {
            format!("{major}.{minor}")
        }
        (Some(major), None) if numeric(&major) => format!("{major}.0"),
        _ => "other".to_owned(),
    };
    Some(label)
}

fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::PUT => "PUT",
        Method::POST => "POST",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ if method.as_str() == "MKCOL" => "MKCOL",
        _ => "other",
    }
}

/// Middleware that counts every request by route template, method, status and client.
pub(super) async fn record(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned());
    let method = method_label(request.method());
    let client = Client::from_headers(request.headers());
    let gradle_version = gradle_version(request.headers());

    let response = next.run(request).await;

    metrics::counter!(
        "mammoth_http_requests_total",
        "route" => route,
        "method" => method,
        "code" => response.status().as_str().to_owned(),
        "client" => client.as_str(),
    )
    .increment(1);
    if let Some(version) = gradle_version {
        metrics::counter!("mammoth_gradle_requests_total", "gradle_version" => version)
            .increment(1);
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(name: &'static str, value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn classifies_clients_by_user_agent() {
        let gradle = "Gradle/8.10 (Linux;6.1;amd64) (Eclipse Adoptium;17.0.9;17.0.9+9)";
        let maven = "Apache-Maven/3.9.9 (Java 21.0.4; Linux 6.8.0)";
        assert_eq!(
            Client::from_headers(&headers("user-agent", gradle)),
            Client::Gradle
        );
        assert_eq!(
            Client::from_headers(&headers("user-agent", maven)),
            Client::Maven
        );
        assert_eq!(
            Client::from_headers(&headers("user-agent", "curl/8")),
            Client::Other
        );
        assert_eq!(Client::from_headers(&HeaderMap::new()), Client::Other);
    }

    #[test]
    fn normalises_gradle_versions() {
        let version = |v| gradle_version(&headers("x-gradle-version", v));
        assert_eq!(version("8.10.2").as_deref(), Some("8.10"));
        assert_eq!(version("8.10").as_deref(), Some("8.10"));
        assert_eq!(version("9.0-rc-1").as_deref(), Some("9.0"));
        assert_eq!(version("9").as_deref(), Some("9.0"));
        assert_eq!(version("lol; DROP").as_deref(), Some("other"));
        assert_eq!(gradle_version(&HeaderMap::new()), None);
    }
}
