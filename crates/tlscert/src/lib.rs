//! Obtains and renews the one wildcard certificate for the base domain and
//! `*.<base domain>` over the ACME DNS-01 challenge (SPEC section 8).
//!
//! A wildcard requires DNS-01, and one wildcard is a deliberate choice: a
//! per-instance certificate would publish every instance name to the
//! Certificate Transparency logs, and would burn a Let's Encrypt issuance
//! on every create and rename. The HTTP and TLS-ALPN challenges are
//! impossible to select, so a misconfiguration can never silently fall back
//! to a challenge that cannot issue the wildcard.

use std::error::Error as StdError;
use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ServerConfig, version};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

mod cloudflare;
mod storage;
mod x509;

pub use cloudflare::CloudflareProvider;

const ACCOUNT_FILE: &str = "account.json";
const CERTIFICATE_FILE: &str = "certificate.pem";
const PRIVATE_KEY_FILE: &str = "certificate-key.pem";
const DEFAULT_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(5 * 60);

fn install_ring_provider() {
    // The HTTP clients use rustls without an implicit provider. Bento selects
    // ring for every TLS user and never enables aws-lc-rs.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// An error returned by a pluggable DNS provider.
pub type ProviderError = Box<dyn StdError + Send + Sync + 'static>;

/// One temporary DNS-01 TXT record created by a [`DnsProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRecord {
    provider_scope: String,
    provider_id: String,
    name: String,
    value: String,
}

impl DnsRecord {
    /// Creates a provider record handle. `provider_scope` and `provider_id`
    /// are returned unchanged to [`DnsProvider::cleanup`].
    pub fn new(
        provider_scope: impl Into<String>,
        provider_id: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            provider_scope: provider_scope.into(),
            provider_id: provider_id.into(),
            name: name.into(),
            value: value.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn provider_scope(&self) -> &str {
        &self.provider_scope
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

/// Sets, observes, and deletes temporary `_acme-challenge` TXT records.
///
/// The provider is pluggable so tests and deployments with another DNS host
/// do not depend on Cloudflare. A record returned by `present` is always
/// passed to `cleanup`, including after propagation or ACME failures.
#[async_trait]
pub trait DnsProvider: Send + Sync + fmt::Debug + 'static {
    async fn present(
        &self,
        base_domain: &str,
        name: &str,
        value: &str,
    ) -> Result<DnsRecord, ProviderError>;

    async fn wait_for_propagation(
        &self,
        record: &DnsRecord,
        timeout: Duration,
    ) -> Result<(), ProviderError>;

    async fn cleanup(&self, record: &DnsRecord) -> Result<(), ProviderError>;
}

/// Returns the Cloudflare DNS provider. Use a scoped API token
/// (`Zone.DNS:Write` on the one zone), never the global API key.
pub fn cloudflare(api_token: impl Into<String>) -> Arc<dyn DnsProvider> {
    Arc::new(CloudflareProvider::new(api_token))
}

/// Configures the wildcard certificate manager (SPEC section 8).
pub struct Config {
    /// The deployment domain, such as `bento.foid.space`. The certificate
    /// covers it and its direct wildcard.
    pub base_domain: String,
    /// The ACME account contact.
    pub email: String,
    /// The DNS-01 solver. Required.
    pub provider: Option<Arc<dyn DnsProvider>>,
    /// Holds the account and issued certificate so a restart does not
    /// re-issue. Required.
    pub storage_dir: PathBuf,
    /// The ACME directory URL. Empty means Let's Encrypt production; use the
    /// staging directory during development to stay clear of the 50-per-week
    /// limit (SPEC section 8).
    pub directory: String,
    /// Bounds the wait for the TXT record to appear in DNS. Zero selects two
    /// minutes.
    pub propagation_timeout: Duration,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("base_domain", &self.base_domain)
            .field("email", &self.email)
            .field("provider", &self.provider)
            .field("storage_dir", &self.storage_dir)
            .field("directory", &self.directory)
            .field("propagation_timeout", &self.propagation_timeout)
            .finish()
    }
}

/// Anything that prevents certificate loading, issuance, renewal, or DNS
/// challenge cleanup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("tlscert: {operation} {path:?}: {source}")]
    Storage {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("tlscert: ACME: {0}")]
    Acme(#[from] instant_acme::Error),
    #[error("tlscert: certificate: {0}")]
    Certificate(String),
    #[error("tlscert: DNS provider: {0}")]
    Provider(#[source] ProviderError),
    #[error("tlscert: DNS cleanup failed: {0}")]
    Cleanup(String),
    #[error("tlscert: serialize account: {0}")]
    AccountJson(#[from] serde_json::Error),
}

/// A cloneable cancellation signal for [`Manager::manage`].
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Returns the certificate's subject set: the base domain itself and its
/// direct wildcard. There are never per-instance certificates (SPEC 8).
pub fn domains(base_domain: &str) -> Vec<String> {
    vec![base_domain.to_string(), format!("*.{base_domain}")]
}

/// Owns the wildcard certificate, updates a live rustls resolver on renewal,
/// and persists all key material.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    base_domain: String,
    domains: Vec<String>,
    email: String,
    provider: Arc<dyn DnsProvider>,
    storage_dir: PathBuf,
    directory: String,
    propagation_timeout: Duration,
    resolver: Arc<LiveResolver>,
    tls_config: Arc<ServerConfig>,
    expires_at: RwLock<Option<OffsetDateTime>>,
    issue_lock: Mutex<()>,
    shutdown: CancellationToken,
}

impl fmt::Debug for Manager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Manager")
            .field("domains", &self.inner.domains)
            .field("storage_dir", &self.inner.storage_dir)
            .field("directory", &self.inner.directory)
            .finish_non_exhaustive()
    }
}

/// Validates `config` and builds a manager. This performs no network traffic.
pub fn new(config: Config) -> Result<Manager, Error> {
    Manager::new(config)
}

impl Manager {
    /// Validates `config` and builds a manager. A persisted certificate is
    /// loaded immediately, but ACME issuance starts only with `manage` or
    /// `manage_sync`.
    pub fn new(config: Config) -> Result<Self, Error> {
        install_ring_provider();
        let base_domain = config
            .base_domain
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if base_domain.is_empty() {
            return Err(Error::Invalid("tlscert: base domain is empty".to_string()));
        }
        if base_domain.contains(['*', '/', ' ']) {
            return Err(Error::Invalid(format!(
                "tlscert: base domain {:?} must be a bare domain, not a wildcard or URL",
                config.base_domain
            )));
        }
        if base_domain.starts_with('.') {
            return Err(Error::Invalid(format!(
                "tlscert: base domain {:?} starts with a dot",
                config.base_domain
            )));
        }
        let provider = config
            .provider
            .ok_or_else(|| Error::Invalid("tlscert: DNS provider is nil".to_string()))?;
        if config.storage_dir.as_os_str().is_empty() {
            return Err(Error::Invalid("tlscert: storage dir is empty".to_string()));
        }

        let directory = if config.directory.is_empty() {
            LetsEncrypt::Production.url().to_string()
        } else {
            config.directory
        };
        let propagation_timeout = if config.propagation_timeout.is_zero() {
            DEFAULT_PROPAGATION_TIMEOUT
        } else {
            config.propagation_timeout
        };
        let loaded = load_certificate(&config.storage_dir)?;
        let resolver = Arc::new(LiveResolver::new(
            loaded.as_ref().map(|certificate| certificate.key.clone()),
        ));
        let tls_config = Arc::new(make_tls_config(resolver.clone())?);
        let expires_at = loaded.map(|certificate| certificate.not_after);

        Ok(Self {
            inner: Arc::new(ManagerInner {
                domains: domains(&base_domain),
                base_domain,
                email: config.email,
                provider,
                storage_dir: config.storage_dir,
                directory,
                propagation_timeout,
                resolver,
                tls_config,
                expires_at: RwLock::new(expires_at),
                issue_lock: Mutex::new(()),
                shutdown: CancellationToken::new(),
            }),
        })
    }

    /// Returns the exact certificate subject set.
    pub fn domains(&self) -> Vec<String> {
        self.inner.domains.clone()
    }

    /// Returns the shared server configuration for every proxy listener.
    /// Its live resolver observes renewals without rebuilding listeners. Only
    /// TLS 1.2 and 1.3 are enabled, with ALPN preference `h2`, then
    /// `http/1.1`.
    pub fn tls_config(&self) -> Arc<ServerConfig> {
        self.inner.tls_config.clone()
    }

    /// Obtains a certificate when absent or near expiry, then keeps renewing
    /// until `cancellation` or [`Manager::close`] fires. The returned task
    /// logs transient errors and retries. Cancellation lets an in-flight
    /// issuance finish so its DNS record is always cleaned up.
    pub fn manage(&self, cancellation: CancellationToken) -> JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move { manager.renewal_loop(cancellation).await })
    }

    /// Obtains or renews the certificate before returning. If the persisted
    /// certificate has more than 30 days remaining, no network traffic occurs.
    pub async fn manage_sync(&self) -> Result<(), Error> {
        self.ensure_certificate().await
    }

    /// Stops background renewal tasks created by this manager.
    pub fn close(&self) {
        self.inner.shutdown.cancel();
    }

    async fn renewal_loop(&self, cancellation: CancellationToken) {
        loop {
            if cancellation.is_cancelled() || self.inner.shutdown.is_cancelled() {
                return;
            }
            let delay = match self.renewal_delay() {
                Some(delay) if !delay.is_zero() => delay,
                _ => match self.ensure_certificate().await {
                    Ok(()) => self.renewal_delay().unwrap_or(RETRY_AFTER_ERROR),
                    Err(error) => {
                        tracing::error!(%error, "certificate issuance or renewal failed");
                        RETRY_AFTER_ERROR
                    }
                },
            };
            tokio::select! {
                () = sleep(delay) => {}
                () = cancellation.cancelled() => return,
                () = self.inner.shutdown.cancelled() => return,
            }
        }
    }

    fn renewal_delay(&self) -> Option<Duration> {
        let expiry = (*self
            .inner
            .expires_at
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner))?;
        let renew_at = expiry - time::Duration::try_from(RENEW_BEFORE).ok()?;
        let remaining = renew_at - OffsetDateTime::now_utc();
        Some(Duration::try_from(remaining).unwrap_or(Duration::ZERO))
    }

    async fn ensure_certificate(&self) -> Result<(), Error> {
        let _guard = self.inner.issue_lock.lock().await;
        if self.renewal_delay().is_some_and(|delay| !delay.is_zero()) {
            return Ok(());
        }

        let (certificate_pem, private_key_pem) = self.issue().await?;
        let loaded = parse_certificate(&certificate_pem, &private_key_pem)?;
        storage::atomic_write(
            &self.inner.storage_dir.join(PRIVATE_KEY_FILE),
            private_key_pem.as_bytes(),
        )?;
        storage::atomic_write(
            &self.inner.storage_dir.join(CERTIFICATE_FILE),
            certificate_pem.as_bytes(),
        )?;
        self.inner.resolver.replace(loaded.key);
        *self
            .inner
            .expires_at
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(loaded.not_after);
        Ok(())
    }

    async fn issue(&self) -> Result<(String, String), Error> {
        let account = self.load_or_create_account().await?;
        let identifiers = self
            .inner
            .domains
            .iter()
            .cloned()
            .map(Identifier::Dns)
            .collect::<Vec<_>>();
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
        let mut records = Vec::new();

        let result = self.complete_order(&mut order, &mut records).await;
        let cleanup = self.cleanup_records(&records).await;
        match (result, cleanup) {
            (Ok(certificate), Ok(())) => Ok(certificate),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(Error::Cleanup(format!(
                "{cleanup}; issuance also failed: {error}"
            ))),
        }
    }

    async fn complete_order(
        &self,
        order: &mut instant_acme::Order,
        records: &mut Vec<DnsRecord>,
    ) -> Result<(String, String), Error> {
        let mut authorizations = order.authorizations();
        while let Some(authorization) = authorizations.next().await {
            let mut authorization = authorization?;
            match authorization.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                status => {
                    return Err(Error::Certificate(format!(
                        "unexpected ACME authorization status: {status:?}"
                    )));
                }
            }

            // The wildcard requires DNS-01 (SPEC 8). No code path selects
            // HTTP-01 or TLS-ALPN-01, so there is no fallback.
            let mut challenge = authorization
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| {
                    Error::Certificate("ACME server offered no DNS-01 challenge".into())
                })?;
            let record = self
                .inner
                .provider
                .present(
                    &self.inner.base_domain,
                    &format!("_acme-challenge.{}", self.inner.base_domain),
                    &challenge.key_authorization().dns_value(),
                )
                .await
                .map_err(Error::Provider)?;
            records.push(record);
            self.inner
                .provider
                .wait_for_propagation(
                    records.last().expect("record was just appended"),
                    self.inner.propagation_timeout,
                )
                .await
                .map_err(Error::Provider)?;
            challenge.set_ready().await?;
        }
        let retry = RetryPolicy::new()
            .initial_delay(Duration::from_secs(1))
            .backoff(1.5)
            .timeout(Duration::from_secs(5 * 60));
        let status = order.poll_ready(&retry).await?;
        if status != OrderStatus::Ready {
            return Err(Error::Certificate(format!(
                "unexpected ACME order status: {status:?}"
            )));
        }

        let key = KeyPair::generate()
            .map_err(|error| Error::Certificate(format!("generate private key: {error}")))?;
        let mut params = CertificateParams::new(self.inner.domains.clone())
            .map_err(|error| Error::Certificate(format!("build certificate request: {error}")))?;
        params.distinguished_name = DistinguishedName::new();
        let request = params.serialize_request(&key).map_err(|error| {
            Error::Certificate(format!("serialize certificate request: {error}"))
        })?;
        order.finalize_csr(request.der()).await?;
        let certificate_pem = order.poll_certificate(&retry).await?;
        Ok((certificate_pem, key.serialize_pem()))
    }

    async fn cleanup_records(&self, records: &[DnsRecord]) -> Result<(), Error> {
        let mut failures = Vec::new();
        for record in records.iter().rev() {
            if let Err(error) = self.inner.provider.cleanup(record).await {
                failures.push(format!("{}: {error}", record.name()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Cleanup(failures.join(", ")))
        }
    }

    async fn load_or_create_account(&self) -> Result<Account, Error> {
        let path = self.inner.storage_dir.join(ACCOUNT_FILE);
        let builder = Account::builder()?;
        if let Some(data) = storage::read_optional(&path)? {
            let value: serde_json::Value = serde_json::from_slice(&data)?;
            if value.get("directory").and_then(serde_json::Value::as_str)
                != Some(&self.inner.directory)
            {
                return Err(Error::Certificate(format!(
                    "stored ACME account uses a different directory than {}",
                    self.inner.directory
                )));
            }
            let credentials: AccountCredentials = serde_json::from_value(value)?;
            return Ok(builder.from_credentials(credentials).await?);
        }

        let contacts = if self.inner.email.is_empty() {
            Vec::new()
        } else {
            vec![format!("mailto:{}", self.inner.email)]
        };
        let contact_refs = contacts.iter().map(String::as_str).collect::<Vec<_>>();
        let (account, credentials) = builder
            .create(
                &NewAccount {
                    contact: &contact_refs,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.inner.directory.clone(),
                None,
            )
            .await?;
        storage::atomic_write(&path, &serde_json::to_vec_pretty(&credentials)?)?;
        Ok(account)
    }
}

#[derive(Debug)]
struct LiveResolver {
    key: RwLock<Option<Arc<CertifiedKey>>>,
}

impl LiveResolver {
    fn new(key: Option<Arc<CertifiedKey>>) -> Self {
        Self {
            key: RwLock::new(key),
        }
    }

    fn replace(&self, key: Arc<CertifiedKey>) {
        *self
            .key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key);
    }
}

impl ResolvesServerCert for LiveResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.key
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct LoadedCertificate {
    key: Arc<CertifiedKey>,
    not_after: OffsetDateTime,
}

fn load_certificate(storage_dir: &Path) -> Result<Option<LoadedCertificate>, Error> {
    let certificate = storage::read_optional(&storage_dir.join(CERTIFICATE_FILE))?;
    let private_key = storage::read_optional(&storage_dir.join(PRIVATE_KEY_FILE))?;
    match (certificate, private_key) {
        (None, None) => Ok(None),
        (Some(certificate), Some(private_key)) => {
            let certificate = String::from_utf8(certificate)
                .map_err(|_| Error::Certificate("stored certificate is not UTF-8 PEM".into()))?;
            let private_key = String::from_utf8(private_key)
                .map_err(|_| Error::Certificate("stored private key is not UTF-8 PEM".into()))?;
            parse_certificate(&certificate, &private_key).map(Some)
        }
        _ => Err(Error::Certificate(
            "stored certificate and private key are incomplete".to_string(),
        )),
    }
}

fn parse_certificate(
    certificate_pem: &str,
    private_key_pem: &str,
) -> Result<LoadedCertificate, Error> {
    let certificates = rustls_pemfile::certs(&mut Cursor::new(certificate_pem))
        .collect::<Result<Vec<CertificateDer<'static>>, _>>()
        .map_err(|error| Error::Certificate(format!("parse certificate PEM: {error}")))?;
    let end_entity = certificates
        .first()
        .ok_or_else(|| Error::Certificate("certificate PEM contains no certificates".into()))?;
    let not_after = x509::not_after(end_entity)?;
    let private_key = rustls_pemfile::private_key(&mut Cursor::new(private_key_pem))
        .map_err(|error| Error::Certificate(format!("parse private key PEM: {error}")))?
        .ok_or_else(|| Error::Certificate("private key PEM contains no private key".into()))?;
    let provider = rustls::crypto::ring::default_provider();
    let key = CertifiedKey::from_der(certificates, private_key, &provider).map_err(|error| {
        Error::Certificate(format!("load certificate and private key: {error}"))
    })?;
    Ok(LoadedCertificate {
        key: Arc::new(key),
        not_after,
    })
}

fn make_tls_config(resolver: Arc<LiveResolver>) -> Result<ServerConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&version::TLS13, &version::TLS12])
        .map_err(|error| Error::Certificate(format!("configure TLS versions: {error}")))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use rcgen::{CertificateParams, KeyPair, date_time_ymd};
    use tempfile::TempDir;

    use super::{
        CERTIFICATE_FILE, Config, DnsProvider, DnsRecord, Manager, PRIVATE_KEY_FILE, ProviderError,
        cloudflare, domains, storage,
    };

    #[derive(Debug)]
    struct FakeProvider;

    #[async_trait]
    impl DnsProvider for FakeProvider {
        async fn present(
            &self,
            _base_domain: &str,
            name: &str,
            value: &str,
        ) -> Result<DnsRecord, ProviderError> {
            Ok(DnsRecord::new("fake", "record", name, value))
        }

        async fn wait_for_propagation(
            &self,
            _record: &DnsRecord,
            _timeout: Duration,
        ) -> Result<(), ProviderError> {
            Ok(())
        }

        async fn cleanup(&self, _record: &DnsRecord) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn valid_config() -> (TempDir, Config) {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            base_domain: "bento.example.org".to_string(),
            email: "operator@example.org".to_string(),
            provider: Some(Arc::new(FakeProvider)),
            storage_dir: directory.path().to_path_buf(),
            directory: String::new(),
            propagation_timeout: Duration::ZERO,
        };
        (directory, config)
    }

    #[test]
    fn domain_set_is_apex_and_direct_wildcard() {
        assert_eq!(
            domains("bento.example.org"),
            ["bento.example.org", "*.bento.example.org"]
        );
    }

    #[test]
    fn validates_constructor_inputs_with_stable_messages() {
        let cases = [
            ("", "tlscert: base domain is empty"),
            (
                "*.bento.example.org",
                "tlscert: base domain \"*.bento.example.org\" must be a bare domain, not a wildcard or URL",
            ),
            (
                "https://bento.example.org",
                "tlscert: base domain \"https://bento.example.org\" must be a bare domain, not a wildcard or URL",
            ),
            (
                ".bento.example.org",
                "tlscert: base domain \".bento.example.org\" starts with a dot",
            ),
        ];
        for (domain, expected) in cases {
            let (_directory, mut config) = valid_config();
            config.base_domain = domain.to_string();
            assert_eq!(Manager::new(config).unwrap_err().to_string(), expected);
        }

        let (_directory, mut config) = valid_config();
        config.provider = None;
        assert_eq!(
            Manager::new(config).unwrap_err().to_string(),
            "tlscert: DNS provider is nil"
        );

        let (_directory, mut config) = valid_config();
        config.storage_dir = PathBuf::new();
        assert_eq!(
            Manager::new(config).unwrap_err().to_string(),
            "tlscert: storage dir is empty"
        );
    }

    #[test]
    fn normalizes_domain_without_network_traffic() {
        let (_directory, mut config) = valid_config();
        config.base_domain = "Bento.Example.Org.".to_string();
        let manager = Manager::new(config).unwrap();
        assert_eq!(
            manager.domains(),
            ["bento.example.org", "*.bento.example.org"]
        );
    }

    #[test]
    fn wires_dns_only_defaults_and_custom_directory() {
        let (_directory, mut config) = valid_config();
        config.propagation_timeout = Duration::from_secs(4 * 60);
        config.directory = "https://acme-staging.example/directory".to_string();
        let manager = Manager::new(config).unwrap();
        assert_eq!(
            manager.inner.directory,
            "https://acme-staging.example/directory"
        );
        assert_eq!(
            manager.inner.propagation_timeout,
            Duration::from_secs(4 * 60)
        );

        let (_directory, config) = valid_config();
        let manager = Manager::new(config).unwrap();
        assert_eq!(
            manager.inner.directory,
            instant_acme::LetsEncrypt::Production.url()
        );
    }

    #[test]
    fn tls_config_has_required_alpn_order() {
        let (_directory, config) = valid_config();
        let manager = Manager::new(config).unwrap();
        assert_eq!(
            manager.tls_config().alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn cloudflare_constructor_returns_pluggable_provider_without_exposing_token() {
        let provider = cloudflare("token-123");
        let debug = format!("{provider:?}");
        assert!(debug.contains("CloudflareProvider"));
        assert!(!debug.contains("token-123"));
    }

    #[tokio::test]
    async fn restart_loads_persisted_certificate_without_issuance() {
        let (directory, config) = valid_config();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(domains("bento.example.org")).unwrap();
        params.not_after = date_time_ymd(2031, 7, 8);
        let certificate = params.self_signed(&key).unwrap();
        storage::atomic_write(
            &directory.path().join(PRIVATE_KEY_FILE),
            key.serialize_pem().as_bytes(),
        )
        .unwrap();
        storage::atomic_write(
            &directory.path().join(CERTIFICATE_FILE),
            certificate.pem().as_bytes(),
        )
        .unwrap();

        let manager = Manager::new(config).unwrap();
        manager.manage_sync().await.unwrap();
        assert!(
            manager
                .renewal_delay()
                .is_some_and(|delay| !delay.is_zero())
        );
    }
}
