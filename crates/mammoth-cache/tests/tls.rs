//! Built-in TLS over a real socket (design doc 0014): round trips, refused and stalled
//! handshakes, ALPN, certificate reload, and the drain. Certificates are generated per test, so
//! nothing expires and nothing reaches the network.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use mammoth_cache::{
    auth::{Authenticator, Scope, hash_token},
    cache::Cache,
    key::CacheKey,
    router_with_auth,
    server::{self, ServerSettings, TlsSettings},
    store::memory::MemoryStore,
    tls::{self, Certificates},
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{
        ClientConfig, RootCertStore,
        crypto::aws_lc_rs,
        pki_types::{CertificateDer, ServerName, pem::PemObject},
        version::{TLS12, TLS13},
    },
};

/// `ci:ci-token`, base64.
const CI: &str = "Basic Y2k6Y2ktdG9rZW4=";
const KEY: &str = "0123456789abcdef";

/// A test CA that issues leaf certificates for `localhost` into a directory.
struct Ca {
    issuer: Issuer<'static, KeyPair>,
    cert: CertificateDer<'static>,
    dir: tempfile::TempDir,
}

impl Ca {
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        Self {
            cert: cert.der().clone(),
            issuer: Issuer::new(params, key),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// Issues a new leaf and writes it to `tls.crt` and `tls.key` in `dir`. Returns its DER.
    fn issue_into(&self, dir: &Path) -> CertificateDer<'static> {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .signed_by(&key, &self.issuer)
            .unwrap();
        std::fs::write(dir.join("tls.crt"), cert.pem()).unwrap();
        std::fs::write(dir.join("tls.key"), key.serialize_pem()).unwrap();
        cert.der().clone()
    }

    fn paths(&self) -> (PathBuf, PathBuf) {
        (
            self.dir.path().join("tls.crt"),
            self.dir.path().join("tls.key"),
        )
    }

    fn connector(&self, alpn: &[&[u8]]) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        roots.add(self.cert.clone()).unwrap();
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
                .with_protocol_versions(&[&TLS13, &TLS12])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        TlsConnector::from(Arc::new(config))
    }
}

struct Node {
    addr: SocketAddr,
    cache: Arc<Cache>,
    certificates: Arc<Certificates>,
    stop: Option<oneshot::Sender<()>>,
    server: JoinHandle<std::io::Result<()>>,
}

fn fast() -> ServerSettings {
    ServerSettings {
        shutdown_delay: Duration::from_millis(300),
        shutdown_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        body_read_timeout: Duration::from_secs(5),
        send_timeout: Duration::from_secs(5),
    }
}

/// Starts a TLS server with the certificate in `cert`/`key`. Anonymous requests may read; `ci`
/// may write.
async fn start(cert: &Path, key: &Path, handshake_timeout: Duration) -> Node {
    let certificates = Arc::new(Certificates::load(cert, key).unwrap());
    let cache = Arc::new(
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .build(),
    );
    let auth = Authenticator::new(
        [Scope::CacheRead].into_iter().collect(),
        [(
            "ci".to_owned(),
            vec![hash_token("ci-token")],
            [Scope::CacheRead, Scope::CacheWrite].into_iter().collect(),
        )],
        false,
    );
    let app = router_with_auth(cache.clone(), None, Arc::new(auth));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = TlsSettings {
        config: certificates.server_config().unwrap(),
        handshake_timeout,
    };
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(server::serve_tls(
        listener,
        tls,
        app,
        cache.clone(),
        fast(),
        async {
            let _ = stopped.await;
        },
    ));
    Node {
        addr,
        cache,
        certificates,
        stop: Some(stop),
        server,
    }
}

async fn start_default(ca: &Ca) -> Node {
    ca.issue_into(ca.dir.path());
    let (cert, key) = ca.paths();
    start(&cert, &key, Duration::from_secs(5)).await
}

async fn connect(ca: &Ca, addr: SocketAddr) -> TlsStream<TcpStream> {
    connect_with(&ca.connector(&[b"http/1.1"]), addr)
        .await
        .unwrap()
}

async fn connect_with(
    connector: &TlsConnector,
    addr: SocketAddr,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    timeout(Duration::from_secs(5), connector.connect(name, tcp))
        .await
        .expect("the handshake hung")
}

fn served_cert(stream: &TlsStream<TcpStream>) -> CertificateDer<'static> {
    stream.get_ref().1.peer_certificates().unwrap()[0].clone()
}

/// Reads a whole response: status and body (`Content-Length` framed).
async fn read_response(stream: &mut (impl AsyncRead + Unpin)) -> (u16, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("timed out waiting for a response")
            .unwrap();
        assert_eq!(
            n,
            1,
            "closed mid-response: {:?}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status = head[9..12].parse().unwrap();
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map_or(0, |v| v.trim().parse().unwrap());
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).await.unwrap();
    (status, body)
}

async fn request(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {CI}\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    read_response(stream).await
}

#[tokio::test]
async fn round_trips_over_tls_on_one_connection() {
    let ca = Ca::new();
    let node = start_default(&ca).await;
    let mut stream = connect(&ca, node.addr).await;
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));

    let path = format!("/cache/{KEY}");
    assert_eq!(request(&mut stream, "GET", &path, b"").await.0, 404);
    assert_eq!(request(&mut stream, "PUT", &path, b"hello").await.0, 201);
    // Keep-alive: the same connection serves the read back.
    assert_eq!(
        request(&mut stream, "GET", &path, b"").await,
        (200, b"hello".to_vec())
    );
    assert_eq!(
        request(&mut stream, "PUT", "/maven/v1/a.jar", b"jar")
            .await
            .0,
        201
    );
    assert_eq!(
        request(&mut stream, "GET", "/maven/v1/a.jar", b"").await,
        (200, b"jar".to_vec())
    );
    assert_eq!(request(&mut stream, "GET", "/livez", b"").await.0, 200);
}

/// `[tls] generate`: the node writes a self-signed pair, and a client that trusts the written
/// certificate, as an operator would after copying it, connects by name and by address.
#[tokio::test]
async fn serves_a_generated_self_signed_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("tls.crt");
    let key = dir.path().join("tls.key");
    assert!(tls::generate_if_missing(&cert, &key, &[]).unwrap());
    let node = start(&cert, &key, Duration::from_secs(5)).await;

    let written = CertificateDer::from_pem_file(&cert).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(written.clone()).unwrap();
    let config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_protocol_versions(&[&TLS13, &TLS12])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    for (name, key) in [("localhost", KEY), ("127.0.0.1", "fedcba9876543210")] {
        let tcp = TcpStream::connect(node.addr).await.unwrap();
        let name = ServerName::try_from(name).unwrap();
        let mut stream = connector.connect(name, tcp).await.unwrap();
        assert_eq!(served_cert(&stream), written);
        let path = format!("/cache/{key}");
        assert_eq!(request(&mut stream, "PUT", &path, b"hello").await.0, 201);
        assert_eq!(
            request(&mut stream, "GET", &path, b"").await,
            (200, b"hello".to_vec())
        );
    }
    // A name the certificate wasn't made for is refused.
    let tcp = TcpStream::connect(node.addr).await.unwrap();
    let name = ServerName::try_from("cache.example.com").unwrap();
    assert!(connector.connect(name, tcp).await.is_err());

    // A restart reuses the pair, so a client that trusted it once still connects.
    assert!(!tls::generate_if_missing(&cert, &key, &[]).unwrap());
    assert_eq!(CertificateDer::from_pem_file(&cert).unwrap(), written);
}

#[tokio::test]
async fn both_tls_versions_are_served() {
    let ca = Ca::new();
    let node = start_default(&ca).await;
    for version in [&TLS12, &TLS13] {
        let mut roots = RootCertStore::empty();
        roots.add(ca.cert.clone()).unwrap();
        let config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_protocol_versions(&[version])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut stream = connect_with(&TlsConnector::from(Arc::new(config)), node.addr)
            .await
            .unwrap();
        assert_eq!(stream.get_ref().1.protocol_version(), Some(version.version));
        assert_eq!(request(&mut stream, "GET", "/livez", b"").await.0, 200);
    }
}

#[tokio::test]
async fn alpn_offers_only_http_1_1() {
    let ca = Ca::new();
    let node = start_default(&ca).await;
    let stream = connect_with(&ca.connector(&[b"h2", b"http/1.1"]), node.addr)
        .await
        .unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    // A client that can only speak HTTP/2 is refused rather than misunderstood.
    assert!(
        connect_with(&ca.connector(&[b"h2"]), node.addr)
            .await
            .is_err()
    );
    // A client that offers no ALPN at all is fine.
    let mut stream = connect_with(&ca.connector(&[]), node.addr).await.unwrap();
    assert_eq!(request(&mut stream, "GET", "/livez", b"").await.0, 200);
}

#[tokio::test]
async fn plain_http_to_the_tls_port_is_closed() {
    let ca = Ca::new();
    let node = start_default(&ca).await;
    let mut plain = TcpStream::connect(node.addr).await.unwrap();
    plain
        .write_all(b"GET /livez HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    // The server answers with a TLS alert at most, never HTTP, then closes.
    let mut received = Vec::new();
    let _ = timeout(Duration::from_secs(2), plain.read_to_end(&mut received))
        .await
        .expect("the plain-HTTP connection stayed open");
    assert!(!received.starts_with(b"HTTP/"), "{received:?}");
    // Others are still served.
    let mut stream = connect(&ca, node.addr).await;
    assert_eq!(request(&mut stream, "GET", "/livez", b"").await.0, 200);
}

#[tokio::test]
async fn a_silent_client_is_closed_after_the_handshake_timeout() {
    let ca = Ca::new();
    ca.issue_into(ca.dir.path());
    let (cert, key) = ca.paths();
    let node = start(&cert, &key, Duration::from_millis(200)).await;
    let mut silent = TcpStream::connect(node.addr).await.unwrap();
    // Meanwhile the accept loop is not held up.
    let mut stream = connect(&ca, node.addr).await;
    assert_eq!(request(&mut stream, "GET", "/livez", b"").await.0, 200);
    let mut buf = [0_u8; 64];
    let read = timeout(Duration::from_secs(2), silent.read(&mut buf))
        .await
        .expect("the silent connection stayed open");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

#[tokio::test]
async fn reload_serves_the_new_certificate_to_new_connections() {
    let ca = Ca::new();
    let node = start_default(&ca).await;
    let mut before = connect(&ca, node.addr).await;
    let old = served_cert(&before);

    let new = ca.issue_into(ca.dir.path());
    assert!(node.certificates.reload().unwrap());
    let mut after = connect(&ca, node.addr).await;
    assert_eq!(served_cert(&after), new);
    assert_ne!(old, new);
    assert_eq!(request(&mut after, "GET", "/livez", b"").await.0, 200);
    // The connection opened before the swap keeps working.
    assert_eq!(request(&mut before, "GET", "/livez", b"").await.0, 200);
}

#[tokio::test]
async fn a_bad_renewal_keeps_the_old_certificate() {
    let ca = Ca::new();
    let node = start_default(&ca).await;
    let old = served_cert(&connect(&ca, node.addr).await);
    // A key that belongs to another certificate.
    let other = tempfile::tempdir().unwrap();
    ca.issue_into(other.path());
    std::fs::copy(other.path().join("tls.key"), ca.dir.path().join("tls.key")).unwrap();
    assert!(node.certificates.reload().is_err());
    assert_eq!(served_cert(&connect(&ca, node.addr).await), old);
}

/// Kubernetes updates a mounted Secret by writing a new directory and swapping a `..data`
/// symlink to it; `tls.crt` and `tls.key` are symlinks through `..data`.
#[cfg(unix)]
#[tokio::test]
async fn the_watcher_follows_a_kubernetes_secret_swap() {
    use std::os::unix::fs::symlink;

    let ca = Ca::new();
    let mount = ca.dir.path();
    let first = mount.join("..2026_10_14_00_00_00.1");
    std::fs::create_dir(&first).unwrap();
    ca.issue_into(&first);
    symlink(&first, mount.join("..data")).unwrap();
    symlink("..data/tls.crt", mount.join("tls.crt")).unwrap();
    symlink("..data/tls.key", mount.join("tls.key")).unwrap();

    let (cert, key) = ca.paths();
    let node = start(&cert, &key, Duration::from_secs(5)).await;
    tokio::spawn(node.certificates.clone().watch(Duration::from_millis(50)));

    let second = mount.join("..2026_11_14_00_00_00.2");
    std::fs::create_dir(&second).unwrap();
    let new = ca.issue_into(&second);
    symlink(&second, mount.join("..data_tmp")).unwrap();
    std::fs::rename(mount.join("..data_tmp"), mount.join("..data")).unwrap();

    timeout(Duration::from_secs(5), async {
        while served_cert(&connect(&ca, node.addr).await) != new {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the renewed certificate was never served");
}

#[tokio::test]
async fn shutdown_finishes_an_upload_in_flight_over_tls() {
    let ca = Ca::new();
    let mut node = start_default(&ca).await;
    let mut upload = connect(&ca, node.addr).await;
    let head = format!(
        "PUT /cache/{KEY} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {CI}\r\n\
         Content-Length: 10\r\n\r\n"
    );
    upload.write_all(head.as_bytes()).await.unwrap();
    upload.write_all(b"01234").await.unwrap();
    upload.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    node.stop.take().unwrap().send(()).unwrap();

    // The upload finishes after the listener has closed.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(TcpStream::connect(node.addr).await.is_err());
    upload.write_all(b"56789").await.unwrap();
    assert_eq!(read_response(&mut upload).await.0, 201);
    drop(upload);

    timeout(Duration::from_secs(5), node.server)
        .await
        .expect("the server did not stop")
        .unwrap()
        .unwrap();
    let key = CacheKey::gradle(KEY).unwrap();
    assert!(node.cache.head(&key).await.unwrap().is_some());
}
