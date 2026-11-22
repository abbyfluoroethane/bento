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
use std::future::Future;
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
/// Each retry is a new ACME order, so a permanent failure must not reach the
/// CA rate limits (SPEC 8). The retry delay doubles up to this cap.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(6 * 60 * 60);
/// A hung HTTP call must not hold `issue_lock` or block startup for ever
/// (SPEC 8). The DNS records are still cleaned up after the deadline.
const ISSUE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

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

/// Runs one ACME order and returns the certificate chain and the private key,
/// both as PEM. `Manager` uses the real ACME flow unless a test supplies a
/// fake, so the renewal logic runs without a CA.
#[async_trait]
trait Issuer: Send + Sync + 'static {
    async fn issue(&self) -> Result<(String, String), Error>;
}

/// Returns the Cloudflare DNS provider. Use a scoped API token
/// (`Zone.DNS:Write` on the one zone), never the global API key.
pub fn cloudflare(api_token: impl Into<String>) -> Arc<dyn DnsProvider> {
    Arc::new(CloudflareProvider::new(api_token))
}

/// Configures the wildcard certificate manager (SPEC section 8).
pub struct Config {
    /// The domain instances publish under, such as `foid.space`. Empty
    /// means the same as `base_domain` (SPEC 7.1).
    pub instance_domain: String,
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
    #[error("tlscert: the certificate does not match the private key")]
    KeyMismatch,
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

/// Returns the certificate's subject set: the base domain and the wildcard
/// under the instance domain. There are never per-instance certificates
/// (SPEC 8).
///
/// A wildcard covers one label, so `*.foid.space` does not cover
/// `web.bento.foid.space`. When the base domain does not sit directly under
/// the instance domain, its own wildcard is added as well. When the two
/// domains are equal this returns the single-domain pair unchanged.
pub fn domains(base_domain: &str, instance_domain: &str) -> Vec<String> {
    let mut subjects = vec![base_domain.to_string(), format!("*.{instance_domain}")];
    let nested = base_domain
        .strip_suffix(&format!(".{instance_domain}"))
        .is_some_and(|label| !label.is_empty() && !label.contains('.'));
    if !nested {
        let wildcard = format!("*.{base_domain}");
        if !subjects.contains(&wildcard) {
            subjects.push(wildcard);
        }
    }
    subjects
}

/// Owns the wildcard certificate, updates a live rustls resolver on renewal,
/// and persists all key material.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    domains: Vec<String>,
    email: String,
    provider: Arc<dyn DnsProvider>,
    storage_dir: PathBuf,
    directory: String,
    propagation_timeout: Duration,
    resolver: Arc<LiveResolver>,
    tls_config: Arc<ServerConfig>,
    expires_at: RwLock<Option<OffsetDateTime>>,
    /// Replaces the ACME flow in tests.
    issuer: Option<Arc<dyn Issuer>>,
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
        Self::build(config, None)
    }

    fn build(config: Config, issuer: Option<Arc<dyn Issuer>>) -> Result<Self, Error> {
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
        let instance_domain = {
            let domain = config
                .instance_domain
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if domain.is_empty() {
                base_domain.clone()
            } else if domain.contains(['*', '/', ' ']) || domain.starts_with('.') {
                return Err(Error::Invalid(format!(
                    "tlscert: instance domain {:?} must be a bare domain",
                    config.instance_domain
                )));
            } else {
                domain
            }
        };
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
                domains: domains(&base_domain, &instance_domain),
                email: config.email,
                provider,
                storage_dir: config.storage_dir,
                directory,
                propagation_timeout,
                resolver,
                tls_config,
                expires_at: RwLock::new(expires_at),
                issuer,
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
    /// TLS 1.2 and 1.3 are enabled, and ALPN offers `http/1.1` only because
    /// the proxy speaks HTTP/1.
    pub fn tls_config(&self) -> Arc<ServerConfig> {
        self.inner.tls_config.clone()
    }

    /// Obtains a certificate when absent or near expiry, then keeps renewing
    /// until `cancellation` or [`Manager::close`] fires. The returned task
    /// logs transient errors and retries with a growing delay. Cancellation
    /// lets an in-flight issuance finish so its DNS record is always cleaned
    /// up. Without this task a long-lived process lets its certificate
    /// expire (SPEC 8).
    pub fn manage(&self, cancellation: CancellationToken) -> JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move { manager.renewal_loop(cancellation).await })
    }

    /// Obtains or renews the certificate before returning. If the persisted
    /// certificate has more than 30 days remaining, no network traffic occurs.
    pub async fn manage_sync(&self) -> Result<(), Error> {
        self.ensure_certificate().await
    }

    /// Like [`Manager::manage_sync`], but a failed renewal is only logged
    /// while the loaded certificate has not expired: a restart during a DNS
    /// or CA outage must not take the proxy down (SPEC 8). The renewal task
    /// from [`Manager::manage`] retries. With no usable certificate the
    /// error is returned.
    pub async fn manage_sync_tolerant(&self) -> Result<(), Error> {
        match self.ensure_certificate().await {
            Err(error) if self.has_valid_certificate() => {
                tracing::error!(
                    %error,
                    "certificate renewal failed; serving the certificate that is still valid"
                );
                Ok(())
            }
            result => result,
        }
    }

    /// Returns true when a loaded certificate has not expired.
    pub fn has_valid_certificate(&self) -> bool {
        self.inner
            .expires_at
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|expiry| expiry > OffsetDateTime::now_utc())
    }

    /// Stops background renewal tasks created by this manager.
    pub fn close(&self) {
        self.inner.shutdown.cancel();
    }

    async fn renewal_loop(&self, cancellation: CancellationToken) {
        let mut failures: u32 = 0;
        loop {
            if cancellation.is_cancelled() || self.inner.shutdown.is_cancelled() {
                return;
            }
            let delay = match self.renewal_delay() {
                Some(delay) if !delay.is_zero() => delay,
                _ => match self.ensure_certificate().await {
                    Ok(()) => {
                        failures = 0;
                        // A certificate that is already inside the renewal
                        // window would order again at once; keep a floor.
                        self.renewal_delay()
                            .filter(|delay| !delay.is_zero())
                            .unwrap_or(RETRY_AFTER_ERROR)
                    }
                    Err(error) => {
                        tracing::error!(%error, "certificate issuance or renewal failed");
                        failures = failures.saturating_add(1);
                        retry_delay(failures)
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
        if let Some(issuer) = &self.inner.issuer {
            return issuer.issue().await;
        }
        let mut records = Vec::new();
        // The deadline covers the order only. The records are cleaned up
        // after it, so a timeout never leaves a TXT record behind.
        let result = within(ISSUE_TIMEOUT, "ACME order", self.order(&mut records)).await;
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

    async fn order(&self, records: &mut Vec<DnsRecord>) -> Result<(String, String), Error> {
        let account = self.load_or_create_account().await?;
        let identifiers = self
            .inner
            .domains
            .iter()
            .cloned()
            .map(Identifier::Dns)
            .collect::<Vec<_>>();
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
        self.complete_order(&mut order, records).await
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
            // Each authorization proves one domain, and the TXT record
            // belongs under that domain. The instance domain can differ
            // from the base domain (SPEC 8), so the name comes from the
            // authorization and not from `base_domain`.
            let Identifier::Dns(domain) = challenge.identifier().identifier else {
                return Err(Error::Certificate(
                    "ACME authorization is not for a DNS name".into(),
                ));
            };
            let name = challenge_record_name(domain);
            let record = self
                .inner
                .provider
                .present(domain, &name, &challenge.key_authorization().dns_value())
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

/// The name of the DNS-01 TXT record for an authorized domain. ACME reports a
/// wildcard authorization under the bare name, so `*.example.org` and
/// `example.org` share one record name (RFC 8555 8.4).
fn challenge_record_name(domain: &str) -> String {
    format!("_acme-challenge.{}", domain.trim_start_matches("*."))
}

/// Returns the delay before retry number `failures` (1 for the first
/// failure): 5 minutes, doubling, capped at [`MAX_RETRY_DELAY`].
fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (RETRY_AFTER_ERROR * 2u32.pow(doublings)).min(MAX_RETRY_DELAY)
}

async fn within<T>(
    limit: Duration,
    what: &str,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::time::timeout(limit, future)
        .await
        .unwrap_or_else(|_| {
            Err(Error::Certificate(format!(
                "{what} timed out after {limit:?}"
            )))
        })
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
            match parse_certificate(&certificate, &private_key) {
                Err(Error::KeyMismatch) => {
                    // A crash between the two writes in `ensure_certificate`
                    // leaves a key and a certificate from different orders.
                    // Issuing a new pair repairs it; failing here would
                    // fail every start (SPEC 8).
                    tracing::warn!(
                        "stored certificate and private key do not match; ignoring both and issuing a new certificate"
                    );
                    Ok(None)
                }
                result => result.map(Some),
            }
        }
        // The same crash window, on the first issuance: only the key exists.
        (None, Some(_)) | (Some(_), None) => {
            tracing::warn!(
                "stored certificate and private key are incomplete; issuing a new certificate"
            );
            Ok(None)
        }
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
        if matches!(
            error,
            rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch)
        ) {
            Error::KeyMismatch
        } else {
            Error::Certificate(format!("load certificate and private key: {error}"))
        }
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
    // The proxy serves HTTP/1 only (`bento-proxy` runs hyper's HTTP/1
    // connection). Offering `h2` would let a browser negotiate a protocol
    // that the server cannot speak.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {

    /// The subject set follows the instance domain (SPEC 8).
    #[test]
    fn subject_set_follows_the_instance_domain() {
        // A single-domain deployment is unchanged.
        assert_eq!(
            domains("bento.example.org", "bento.example.org"),
            vec!["bento.example.org", "*.bento.example.org"]
        );
        // One label under the instance domain: its wildcard already covers
        // the base domain, so no third subject is needed.
        assert_eq!(
            domains("bento.example.org", "example.org"),
            vec!["bento.example.org", "*.example.org"]
        );
        // Unrelated domains need both wildcards, because a wildcard covers
        // exactly one label.
        assert_eq!(
            domains("bento.example.org", "example.net"),
            vec!["bento.example.org", "*.example.net", "*.bento.example.org"]
        );
        // Two labels down is not "directly under", so the base domain keeps
        // its own wildcard.
        assert_eq!(
            domains("a.b.example.org", "example.org"),
            vec!["a.b.example.org", "*.example.org", "*.a.b.example.org"]
        );
    }
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use rcgen::{CertificateParams, KeyPair, date_time_ymd};
    use tempfile::TempDir;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        CERTIFICATE_FILE, CancellationToken, Config, DnsProvider, DnsRecord, Error, Issuer,
        MAX_RETRY_DELAY, Manager, PRIVATE_KEY_FILE, ProviderError, RETRY_AFTER_ERROR, cloudflare,
        domains, retry_delay, storage, within,
    };

    /// Returns a self-signed certificate and its key as PEM, with `days`
    /// left until expiry.
    fn pem_pair(days: i64) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(domains("bento.example.org", "bento.example.org")).unwrap();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(days);
        let certificate = params.self_signed(&key).unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    fn persist(directory: &TempDir, certificate: &str, key: &str) {
        storage::atomic_write(&directory.path().join(PRIVATE_KEY_FILE), key.as_bytes()).unwrap();
        storage::atomic_write(
            &directory.path().join(CERTIFICATE_FILE),
            certificate.as_bytes(),
        )
        .unwrap();
    }

    /// Stands in for the ACME order: it issues a certificate with `days`
    /// left, or fails.
    struct FakeIssuer {
        days: i64,
        fail: bool,
        calls: AtomicUsize,
    }

    impl FakeIssuer {
        fn new(days: i64, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                days,
                fail,
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Issuer for FakeIssuer {
        async fn issue(&self) -> Result<(String, String), Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(Error::Certificate("the CA is unreachable".into()));
            }
            Ok(pem_pair(self.days))
        }
    }

    fn manager_with(config: Config, issuer: &Arc<FakeIssuer>) -> Manager {
        Manager::build(config, Some(issuer.clone())).unwrap()
    }

    /// A renewal task must run next to the proxy, or a long-lived process
    /// lets its certificate expire (SPEC 8).
    #[tokio::test]
    async fn renewal_loop_replaces_a_certificate_inside_the_window() {
        let (directory, config) = valid_config();
        let (certificate, key) = pem_pair(10);
        persist(&directory, &certificate, &key);
        let issuer = FakeIssuer::new(90, false);
        let manager = manager_with(config, &issuer);
        let before = manager.inner.resolver.key.read().unwrap().clone().unwrap();
        // Ten days left is inside the 30-day window.
        assert!(manager.renewal_delay().unwrap().is_zero());

        let cancellation = CancellationToken::new();
        let task = manager.manage(cancellation.clone());
        for _ in 0..500 {
            if issuer.calls.load(Ordering::SeqCst) > 0
                && manager
                    .renewal_delay()
                    .is_some_and(|delay| delay > Duration::from_secs(30 * 24 * 60 * 60))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(issuer.calls.load(Ordering::SeqCst), 1);
        assert!(manager.renewal_delay().unwrap() > Duration::from_secs(30 * 24 * 60 * 60));
        let after = manager.inner.resolver.key.read().unwrap().clone().unwrap();
        assert_ne!(before.cert, after.cert);

        cancellation.cancel();
        task.await.unwrap();
    }

    /// A restart during a DNS or CA outage keeps serving a certificate that
    /// is still valid (SPEC 8).
    #[tokio::test]
    async fn startup_serves_a_valid_certificate_when_renewal_fails() {
        let (directory, config) = valid_config();
        let (certificate, key) = pem_pair(5);
        persist(&directory, &certificate, &key);
        let issuer = FakeIssuer::new(90, true);
        let manager = manager_with(config, &issuer);

        assert!(manager.has_valid_certificate());
        assert!(manager.manage_sync().await.is_err());
        manager.manage_sync_tolerant().await.unwrap();
        assert!(manager.has_valid_certificate());
        assert!(manager.inner.resolver.key.read().unwrap().is_some());
    }

    #[tokio::test]
    async fn startup_fails_when_issuance_fails_and_no_certificate_exists() {
        let (_directory, config) = valid_config();
        let issuer = FakeIssuer::new(90, true);
        let manager = manager_with(config, &issuer);

        assert!(!manager.has_valid_certificate());
        assert!(manager.manage_sync_tolerant().await.is_err());
    }

    /// A crash between the key write and the certificate write leaves a
    /// mismatched pair. The manager must start and issue a new one (SPEC 8).
    #[tokio::test]
    async fn mismatched_key_and_certificate_count_as_no_certificate() {
        let (directory, config) = valid_config();
        let (certificate, _) = pem_pair(60);
        let (_, other_key) = pem_pair(60);
        persist(&directory, &certificate, &other_key);
        let issuer = FakeIssuer::new(90, false);
        let manager = manager_with(config, &issuer);

        assert!(!manager.has_valid_certificate());
        manager.manage_sync_tolerant().await.unwrap();
        assert_eq!(issuer.calls.load(Ordering::SeqCst), 1);
        assert!(manager.has_valid_certificate());
    }

    #[test]
    fn a_key_without_a_certificate_counts_as_no_certificate() {
        let (directory, config) = valid_config();
        let (_, key) = pem_pair(60);
        storage::atomic_write(&directory.path().join(PRIVATE_KEY_FILE), key.as_bytes()).unwrap();
        let manager = Manager::new(config).unwrap();
        assert!(!manager.has_valid_certificate());
    }

    /// Each authorization needs its TXT record under its own domain, so a
    /// wildcard for an instance domain that differs from the base domain is
    /// not validated against the base domain's name.
    #[test]
    fn challenge_record_follows_the_authorized_domain() {
        // ACME reports `*.example.org` as the bare name `example.org`.
        assert_eq!(
            super::challenge_record_name("example.org"),
            "_acme-challenge.example.org"
        );
        assert_eq!(
            super::challenge_record_name("bento.example.org"),
            "_acme-challenge.bento.example.org"
        );
        // A subject written with its wildcard maps to the same record.
        for (base, instance) in [
            ("bento.example.org", "bento.example.org"),
            ("bento.example.org", "example.org"),
            ("bento.example.org", "example.net"),
        ] {
            let names = domains(base, instance)
                .iter()
                .map(|subject| super::challenge_record_name(subject))
                .collect::<Vec<_>>();
            assert!(names.contains(&format!("_acme-challenge.{instance}")));
            assert!(names.contains(&format!("_acme-challenge.{base}")));
        }
    }

    #[test]
    fn retry_delay_doubles_up_to_a_cap() {
        let minutes = |count: u64| Duration::from_secs(count * 60);
        assert_eq!(retry_delay(1), RETRY_AFTER_ERROR);
        assert_eq!(retry_delay(2), minutes(10));
        assert_eq!(retry_delay(3), minutes(20));
        assert_eq!(retry_delay(4), minutes(40));
        assert_eq!(retry_delay(7), minutes(320));
        assert_eq!(retry_delay(8), MAX_RETRY_DELAY);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY_DELAY);
    }

    #[tokio::test]
    async fn an_order_that_hangs_hits_the_deadline() {
        let result = within(
            Duration::from_millis(20),
            "ACME order",
            std::future::pending::<Result<(), Error>>(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

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
            instance_domain: String::new(),
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
            domains("bento.example.org", "bento.example.org"),
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
    fn tls_config_offers_only_the_protocol_the_proxy_speaks() {
        let (_directory, config) = valid_config();
        let manager = Manager::new(config).unwrap();
        assert_eq!(manager.tls_config().alpn_protocols, [b"http/1.1".to_vec()]);
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
        let mut params =
            CertificateParams::new(domains("bento.example.org", "bento.example.org")).unwrap();
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
