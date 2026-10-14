//! L3: the object-store bucket.
//!
//! Entries are stored as plain objects at `<prefix>/<namespace>/<name>`, for example
//! `mammoth/gradle/a3f5…`, so a bucket can be inspected, shared between nodes and expired by its own
//! lifecycle rules. The object stores keep their own checksums, so no header or trailer is added.
//!
//! The [`object_store`] crate speaks S3 (and S3-compatible stores), GCS, Azure Blob and the local
//! file system. Credentials come from each cloud's default chain (design doc 0003); the config names
//! only the bucket URL, region and endpoint.
//!
//! The node needs `get`, `put` and `head` on its prefix, and nothing else. Uploads up to
//! [`BucketConfig::multipart_threshold`] are a single `put`; larger ones are multipart, so an entry
//! is never buffered whole.
//!
//! A bucket outage must leave the node serving from its local tiers (design doc 0013), so the
//! clients fail fast instead of using `object_store`'s defaults (10 retries over up to 3 minutes,
//! longer than Gradle's 30 s request timeout): connects time out after [`CONNECT_TIMEOUT`], a
//! connection that sends nothing for [`READ_TIMEOUT`] is dropped, and a request is retried at most
//! [`BucketConfig::retries`] times within [`BucketConfig::timeout`]. Environment variables such as
//! `AWS_CONNECT_TIMEOUT` or `GOOGLE_TIMEOUT` still override the client timeouts. The core adds the
//! hard deadline on reads and the cooldown after a failure (see [`crate::cache::BucketLimits`]).

use std::{io, str::FromStr, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    BackoffConfig, ClientConfigKey, ObjectStore, ObjectStoreExt, ObjectStoreScheme, PutPayload,
    RetryConfig, WriteMultipart,
    aws::{AmazonS3Builder, AmazonS3ConfigKey},
    azure::{AzureConfigKey, MicrosoftAzureBuilder},
    gcp::{GoogleCloudStorageBuilder, GoogleConfigKey},
    local::LocalFileSystem,
    memory::InMemory,
    path::Path,
};
use url::Url;

use super::{Blob, BlobStore, BoxFuture, ByteStream, PutOutcome, Tier};
use crate::key::CacheKey;

/// The smallest multipart part S3 accepts, and the default threshold for switching to multipart.
const MIN_PART_SIZE: usize = 5 * 1024 * 1024;
/// Parts of one multipart upload in flight at once.
const UPLOAD_CONCURRENCY: usize = 4;

/// The default for [`BucketConfig::timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
/// The default for [`BucketConfig::retries`].
pub const DEFAULT_RETRIES: usize = 1;
/// The time allowed to open a connection to the bucket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest a bucket connection may send nothing before the request fails. Per read, not
/// total, so a large entry on a slow link still streams.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest one bucket request may take, body included. Generous, because a single request
/// carries a whole entry on a read and up to [`BucketConfig::multipart_threshold`] on a write;
/// [`READ_TIMEOUT`] catches stalls long before this.
pub const REQUEST_TIMEOUT: Duration = Duration::from_mins(5);

/// Settings for a [`BucketStore`].
#[derive(Debug, Clone)]
pub struct BucketConfig {
    /// Where entries live: `s3://`, `gs://`, `az://`, `file://` or `memory://`, with an optional
    /// key prefix as the path.
    pub url: String,
    /// The bucket's region. S3 only.
    pub region: Option<String>,
    /// An S3-compatible endpoint such as `MinIO` or Cloudflare R2, or a custom Azure endpoint.
    pub endpoint: Option<String>,
    /// Allows (`Some(true)`) or refuses (`Some(false)`) `http://` endpoints, for a local
    /// S3-compatible store or Azurite. `None` leaves it to the environment. S3 and Azure only.
    pub allow_http: Option<bool>,
    /// Uploads larger than this are multipart; also the part size. At least 5 MiB.
    pub multipart_threshold: usize,
    /// How long the client keeps retrying a failed request. The core also cuts reads off here.
    pub timeout: Duration,
    /// Retries of a failed request, within [`timeout`](Self::timeout).
    pub retries: usize,
}

impl BucketConfig {
    /// A config for `url` with no region or endpoint and the default multipart threshold.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            region: None,
            endpoint: None,
            allow_http: None,
            multipart_threshold: MIN_PART_SIZE,
            timeout: DEFAULT_TIMEOUT,
            retries: DEFAULT_RETRIES,
        }
    }
}

/// The bucket tier. Cheap to clone.
#[derive(Debug, Clone)]
pub struct BucketStore {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
    multipart_threshold: usize,
}

impl BucketStore {
    /// Connects to the bucket named by `config.url`. Nothing is sent until the first request, so
    /// a wrong bucket or missing permission shows up as errors on use, not here.
    pub fn open(config: &BucketConfig) -> io::Result<Self> {
        let invalid = |e: &dyn std::fmt::Display| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bucket url {:?}: {e}", config.url),
            )
        };
        let url = Url::parse(&config.url).map_err(|e| invalid(&e))?;
        let (scheme, prefix) = ObjectStoreScheme::parse(&url).map_err(|e| invalid(&e))?;
        let prefix = Path::parse(prefix).map_err(|e| invalid(&e))?;
        let store: Arc<dyn ObjectStore> = match scheme {
            ObjectStoreScheme::AmazonS3 => {
                Arc::new(s3_builder(config, &url).build().map_err(|e| invalid(&e))?)
            }
            ObjectStoreScheme::GoogleCloudStorage => {
                let mut builder =
                    GoogleCloudStorageBuilder::from_env().with_retry(retry_config(config));
                for (key, value) in client_defaults() {
                    let key = GoogleConfigKey::Client(key);
                    if !env_sets("GOOGLE_", &key) {
                        builder = builder.with_config(key, value);
                    }
                }
                let builder = builder.with_url(url.as_str());
                Arc::new(builder.build().map_err(|e| invalid(&e))?)
            }
            ObjectStoreScheme::MicrosoftAzure => {
                let mut builder =
                    MicrosoftAzureBuilder::from_env().with_retry(retry_config(config));
                for (key, value) in client_defaults() {
                    let key = AzureConfigKey::Client(key);
                    if !env_sets("AZURE_", &key) {
                        builder = builder.with_config(key, value);
                    }
                }
                builder = builder.with_url(url.as_str());
                if let Some(endpoint) = &config.endpoint {
                    builder = builder.with_endpoint(endpoint.clone());
                }
                if let Some(allow) = config.allow_http {
                    builder = builder.with_allow_http(allow);
                }
                Arc::new(builder.build().map_err(|e| invalid(&e))?)
            }
            ObjectStoreScheme::Local => Arc::new(LocalFileSystem::new()),
            ObjectStoreScheme::Memory => Arc::new(InMemory::new()),
            other => return Err(invalid(&format_args!("unsupported scheme {other:?}"))),
        };
        Ok(Self::with_store(store, prefix, config.multipart_threshold))
    }

    /// A bucket tier over an existing object store, with keys under `prefix`.
    #[must_use]
    pub fn with_store(
        store: Arc<dyn ObjectStore>,
        prefix: Path,
        multipart_threshold: usize,
    ) -> Self {
        Self {
            store,
            prefix,
            multipart_threshold: multipart_threshold.max(MIN_PART_SIZE),
        }
    }

    fn location(&self, key: &CacheKey) -> Path {
        // Keys are validated: every segment is non-empty and free of `.`, `..` and `%`.
        key.as_str()
            .split('/')
            .fold(self.prefix.clone(), |path, segment| {
                path.clone().join(segment)
            })
    }

    async fn read(&self, key: &CacheKey) -> io::Result<Option<Blob>> {
        match self.store.get(&self.location(key)).await {
            Ok(result) => {
                let size = result.meta.size;
                let body = result.into_stream().map_err(io::Error::other);
                Ok(Some(Blob {
                    size,
                    body: Box::pin(body),
                }))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(io::Error::other(e)),
        }
    }

    async fn size(&self, key: &CacheKey) -> io::Result<Option<u64>> {
        match self.store.head(&self.location(key)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(io::Error::other(e)),
        }
    }

    async fn write(&self, key: &CacheKey, mut body: ByteStream) -> io::Result<PutOutcome> {
        // Gradle entries are content-addressed, so an existing object already has these bytes.
        if key.namespace().is_immutable() && self.size(key).await?.is_some() {
            return Ok(PutOutcome::Exists);
        }
        let location = self.location(key);

        // Buffer up to the threshold: most entries are small, and one put is one request.
        let mut buffered = Vec::new();
        let mut buffered_len = 0;
        while buffered_len <= self.multipart_threshold {
            let Some(chunk) = body.next().await else {
                let payload: PutPayload = buffered.into_iter().collect();
                self.store
                    .put(&location, payload)
                    .await
                    .map_err(io::Error::other)?;
                return Ok(PutOutcome::Stored);
            };
            let chunk = chunk?;
            buffered_len += chunk.len();
            buffered.push(chunk);
        }

        let upload = self
            .store
            .put_multipart(&location)
            .await
            .map_err(io::Error::other)?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, self.multipart_threshold);
        match upload_rest(&mut writer, buffered, body).await {
            Ok(()) => {
                writer.finish().await.map_err(io::Error::other)?;
                Ok(PutOutcome::Stored)
            }
            Err(e) => {
                if let Err(error) = writer.abort().await {
                    tracing::warn!(%key, %error, "could not abort multipart upload");
                }
                Err(e)
            }
        }
    }
}

/// The S3 client for `url`: the environment, then the fail-fast client settings, then the config.
fn s3_builder(config: &BucketConfig, url: &Url) -> AmazonS3Builder {
    let mut builder = AmazonS3Builder::from_env().with_retry(retry_config(config));
    for (key, value) in client_defaults() {
        let key = AmazonS3ConfigKey::Client(key);
        if !env_sets("AWS_", &key) {
            builder = builder.with_config(key, value);
        }
    }
    builder = builder.with_url(url.as_str());
    if let Some(region) = &config.region {
        builder = builder.with_region(region);
    }
    if let Some(endpoint) = &config.endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    // Unset leaves `AWS_ALLOW_HTTP` from the environment in charge; set, the file wins.
    if let Some(allow) = config.allow_http {
        builder = builder.with_allow_http(allow);
    }
    builder
}

/// Client settings applied over each cloud's defaults, as config values: see the module docs.
fn client_defaults() -> [(ClientConfigKey, String); 3] {
    let secs = |d: Duration| format!("{}s", d.as_secs());
    [
        (ClientConfigKey::ConnectTimeout, secs(CONNECT_TIMEOUT)),
        (ClientConfigKey::ReadTimeout, secs(READ_TIMEOUT)),
        (ClientConfigKey::Timeout, secs(REQUEST_TIMEOUT)),
    ]
}

/// Retries for the cloud clients: [`BucketConfig::retries`] quick ones, within
/// [`BucketConfig::timeout`].
fn retry_config(config: &BucketConfig) -> RetryConfig {
    RetryConfig {
        backoff: BackoffConfig {
            init_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            base: 2.0,
        },
        max_retries: config.retries,
        retry_timeout: config.timeout,
    }
}

/// Whether an environment variable starting with `prefix` sets `key`, the way each builder's
/// `from_env` reads them. Such a setting wins over [`client_defaults`].
fn env_sets<K: FromStr + PartialEq>(prefix: &str, key: &K) -> bool {
    std::env::vars_os().any(|(name, _)| {
        name.to_str().is_some_and(|name| {
            name.starts_with(prefix)
                && name
                    .to_ascii_lowercase()
                    .parse::<K>()
                    .is_ok_and(|parsed| parsed == *key)
        })
    })
}

/// Feeds the buffered chunks, then the rest of the body, to a multipart upload.
async fn upload_rest(
    writer: &mut WriteMultipart,
    buffered: Vec<Bytes>,
    mut body: ByteStream,
) -> io::Result<()> {
    for chunk in buffered {
        writer.put(chunk);
    }
    while let Some(chunk) = body.next().await {
        writer
            .wait_for_capacity(UPLOAD_CONCURRENCY)
            .await
            .map_err(io::Error::other)?;
        writer.put(chunk?);
    }
    Ok(())
}

impl BlobStore for BucketStore {
    fn tier(&self) -> Tier {
        Tier::Bucket
    }

    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(self.read(key))
    }

    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(self.size(key))
    }

    fn put<'a>(
        &'a self,
        key: &'a CacheKey,
        _size_hint: Option<u64>,
        body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        Box::pin(self.write(key, body))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream;

    use super::*;

    fn memory() -> (BucketStore, Arc<InMemory>) {
        let backing = Arc::new(InMemory::new());
        let store = BucketStore::with_store(backing.clone(), Path::from("mammoth"), 0);
        (store, backing)
    }

    fn body(chunks: Vec<Bytes>) -> ByteStream {
        Box::pin(stream::iter(chunks.into_iter().map(Ok)))
    }

    async fn read_all(store: &BucketStore, key: &CacheKey) -> Option<Vec<u8>> {
        let blob = store.get(key).await.unwrap()?;
        let chunks: Vec<Bytes> = blob.body.try_collect().await.unwrap();
        Some(chunks.concat())
    }

    #[tokio::test]
    async fn round_trip_under_the_prefix() {
        let (store, backing) = memory();
        let key = CacheKey::gradle("a3f5").unwrap();
        assert_eq!(store.head(&key).await.unwrap(), None);
        let outcome = store.put(&key, None, body(vec![Bytes::from("hi")])).await;
        assert_eq!(outcome.unwrap(), PutOutcome::Stored);
        assert_eq!(store.head(&key).await.unwrap(), Some(2));
        assert_eq!(read_all(&store, &key).await.as_deref(), Some(&b"hi"[..]));
        let raw = backing
            .head(&Path::from("mammoth/gradle/a3f5"))
            .await
            .unwrap();
        assert_eq!(raw.size, 2);
    }

    #[tokio::test]
    async fn gradle_is_write_once_and_maven_is_replaced() {
        let (store, _) = memory();
        let gradle = CacheKey::gradle("abc").unwrap();
        store
            .put(&gradle, None, body(vec![Bytes::from("one")]))
            .await
            .unwrap();
        let again = store
            .put(&gradle, None, body(vec![Bytes::from("two")]))
            .await;
        assert_eq!(again.unwrap(), PutOutcome::Exists);
        assert_eq!(
            read_all(&store, &gradle).await.as_deref(),
            Some(&b"one"[..])
        );

        let maven = CacheKey::maven("v1.1/g/a/cs/buildinfo.xml").unwrap();
        store
            .put(&maven, None, body(vec![Bytes::from("old")]))
            .await
            .unwrap();
        let replaced = store
            .put(&maven, None, body(vec![Bytes::from("new")]))
            .await;
        assert_eq!(replaced.unwrap(), PutOutcome::Stored);
        assert_eq!(read_all(&store, &maven).await.as_deref(), Some(&b"new"[..]));
    }

    #[tokio::test]
    async fn large_entries_go_multipart() {
        let (store, _) = memory();
        let key = CacheKey::gradle("b16").unwrap();
        let chunk = Bytes::from(vec![9_u8; 1024 * 1024]);
        let chunks = vec![chunk; 12]; // 12 MiB: past the 5 MiB threshold
        store.put(&key, None, body(chunks)).await.unwrap();
        assert_eq!(store.head(&key).await.unwrap(), Some(12 * 1024 * 1024));
        let bytes = read_all(&store, &key).await.unwrap();
        assert!(bytes.len() == 12 * 1024 * 1024 && bytes.iter().all(|b| *b == 9));
    }

    #[tokio::test]
    async fn failed_uploads_leave_nothing() {
        let (store, _) = memory();
        for (name, before_failure) in [("51", 1), ("b19", 12)] {
            let key = CacheKey::gradle(name).unwrap();
            let mut chunks: Vec<io::Result<Bytes>> = (0..before_failure)
                .map(|_| Ok(Bytes::from(vec![1_u8; 1024 * 1024])))
                .collect();
            chunks.push(Err(io::Error::other("client went away")));
            let broken: ByteStream = Box::pin(stream::iter(chunks));
            assert!(store.put(&key, None, broken).await.is_err());
            assert_eq!(store.head(&key).await.unwrap(), None, "{name}");
        }
    }

    #[tokio::test]
    async fn opens_file_urls() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(dir.path()).unwrap();
        let store = BucketStore::open(&BucketConfig::new(format!("{url}cache"))).unwrap();
        let key = CacheKey::maven("v1/a.xml").unwrap();
        store
            .put(&key, None, body(vec![Bytes::from("x")]))
            .await
            .unwrap();
        assert!(dir.path().join("cache/maven/v1/a.xml").exists());
    }

    /// A bucket at `endpoint`, with test credentials.
    fn s3_at(endpoint: String) -> BucketStore {
        let mut config = BucketConfig::new("s3://outage/mammoth");
        config.region = Some("us-east-1".into());
        config.endpoint = Some(endpoint);
        config.allow_http = Some(true);
        let url = Url::parse(&config.url).unwrap();
        let s3 = s3_builder(&config, &url)
            .with_access_key_id("test")
            .with_secret_access_key("test")
            .build()
            .unwrap();
        BucketStore::with_store(Arc::new(s3), Path::from("mammoth"), 0)
    }

    #[tokio::test]
    async fn a_refused_bucket_fails_fast() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let store = s3_at(format!("http://{addr}"));
        let key = CacheKey::gradle("a3f5").unwrap();
        let started = std::time::Instant::now();
        assert!(store.head(&key).await.is_err());
        assert!(store.get(&key).await.is_err());
        // Two requests, each tried twice: well inside the default budget.
        let limit = 2 * DEFAULT_TIMEOUT + Duration::from_secs(1);
        assert!(started.elapsed() < limit, "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn an_unresponsive_bucket_is_a_fast_miss() {
        // Accepts connections and never answers, as when a load balancer is up but nothing
        // behind it is.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let cache = crate::cache::Cache::builder()
            .bucket_tier(
                Arc::new(s3_at(format!("http://{addr}"))),
                crate::cache::BucketWrite::Sync,
            )
            .build();
        let key = CacheKey::gradle("a3f5").unwrap();
        let started = std::time::Instant::now();
        assert!(cache.get(&key).await.unwrap().is_none());
        assert_eq!(cache.bucket_up(), Some(false));
        // Inside the cooldown: no request at all.
        assert_eq!(cache.head(&key).await.unwrap(), None);
        let limit = DEFAULT_TIMEOUT + Duration::from_secs(1);
        assert!(started.elapsed() < limit, "took {:?}", started.elapsed());
    }

    #[test]
    fn rejects_unknown_schemes() {
        let err = BucketStore::open(&BucketConfig::new("ftp://example.com/x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
