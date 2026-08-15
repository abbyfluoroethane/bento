use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use bento_types::State;

use crate::{DomainInfo, Error, Hypervisor, StopResult};

/// One domain held by [`Fake`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeDomain {
    pub name: String,
    pub uuid: String,
    pub xml: String,
    pub state: State,
    /// Create always clears this flag (SPEC 11.2), so it remains false
    /// unless a test seeds another value.
    pub autostart: bool,
}

type Hook = Arc<dyn Fn(&str, &str) -> Result<(), Error> + Send + Sync>;

#[derive(Default)]
struct Inner {
    domains: HashMap<String, FakeDomain>,
    calls: Vec<String>,
    hook: Option<Hook>,
    force_stop: bool,
}

/// An in-memory hypervisor for tests. Its default value is ready to use.
#[derive(Default)]
pub struct Fake {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        formatter
            .debug_struct("Fake")
            .field("domains", &inner.domains)
            .field("calls", &inner.calls)
            .field("has_hook", &inner.hook.is_some())
            .field("force_stop", &inner.force_stop)
            .finish()
    }
}

impl Fake {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn begin(inner: &mut Inner, operation: &str, name: &str) -> Result<(), Error> {
        inner.calls.push(format!("{operation} {name}"));
        if let Some(hook) = &inner.hook {
            hook(operation, name)?;
        }
        Ok(())
    }

    /// Returns a snapshot of a stored domain.
    pub fn domain(&self, name: &str) -> Option<FakeDomain> {
        self.lock().domains.get(name).cloned()
    }

    /// Seeds a domain directly for tests that need a preexisting state,
    /// such as host-reboot restoration (SPEC 11.2).
    pub fn set_domain(&self, domain: FakeDomain) {
        self.lock().domains.insert(domain.name.clone(), domain);
    }

    /// Installs a failure-injection hook run before each operation.
    pub fn set_hook<F>(&self, hook: F)
    where
        F: Fn(&str, &str) -> Result<(), Error> + Send + Sync + 'static,
    {
        self.lock().hook = Some(Arc::new(hook));
    }

    /// Removes the current operation hook.
    pub fn clear_hook(&self) {
        self.lock().hook = None;
    }

    /// Makes stops report the forced-destroy path.
    pub fn set_force_stop(&self, force: bool) {
        self.lock().force_stop = force;
    }

    /// Returns the recorded `"operation name"` calls.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }
}

#[async_trait]
impl Hypervisor for Fake {
    /// Parses name and UUID from the XML, which also proves the document is
    /// well formed.
    async fn create(&self, xml: &str) -> Result<(), Error> {
        let (name, uuid) = crate::xml::domain_identity(xml)
            .map_err(|error| Error::Operation(format!("fake create: bad domain xml: {error}")))?;
        if name.is_empty() || uuid.is_empty() {
            return Err(Error::Operation(
                "fake create: domain xml missing name or uuid".to_string(),
            ));
        }

        let mut inner = self.lock();
        Self::begin(&mut inner, "create", &name)?;
        if inner.domains.contains_key(&name) {
            return Err(Error::DomainExists(name));
        }
        inner.domains.insert(
            name.clone(),
            FakeDomain {
                name,
                uuid,
                xml: xml.to_string(),
                state: State::Running,
                autostart: false,
            },
        );
        Ok(())
    }

    async fn start(&self, name: &str) -> Result<(), Error> {
        let mut inner = self.lock();
        Self::begin(&mut inner, "start", name)?;
        let domain = inner
            .domains
            .get_mut(name)
            .ok_or_else(|| Error::DomainNotFound(name.to_string()))?;
        domain.state = State::Running;
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<StopResult, Error> {
        let mut inner = self.lock();
        Self::begin(&mut inner, "stop", name)?;
        let force_stop = inner.force_stop;
        let domain = inner
            .domains
            .get_mut(name)
            .ok_or_else(|| Error::DomainNotFound(name.to_string()))?;
        if domain.state == State::Stopped {
            return Ok(StopResult::AlreadyStopped);
        }
        domain.state = State::Stopped;
        Ok(if force_stop {
            StopResult::Forced
        } else {
            StopResult::Graceful
        })
    }

    async fn reboot(&self, name: &str) -> Result<(), Error> {
        let mut inner = self.lock();
        Self::begin(&mut inner, "reboot", name)?;
        let domain = inner
            .domains
            .get(name)
            .ok_or_else(|| Error::DomainNotFound(name.to_string()))?;
        if domain.state != State::Running {
            return Err(Error::Operation(format!(
                "fake reboot: domain {name} is not running"
            )));
        }
        Ok(())
    }

    async fn remove(&self, name: &str) -> Result<(), Error> {
        let mut inner = self.lock();
        Self::begin(&mut inner, "remove", name)?;
        if inner.domains.remove(name).is_none() {
            return Err(Error::DomainNotFound(name.to_string()));
        }
        Ok(())
    }

    async fn list(&self) -> Result<Vec<DomainInfo>, Error> {
        let mut inner = self.lock();
        Self::begin(&mut inner, "list", "")?;
        let mut domains: Vec<_> = inner
            .domains
            .values()
            .map(|domain| DomainInfo {
                name: domain.name.clone(),
                uuid: domain.uuid.clone(),
                state: domain.state,
            })
            .collect();
        domains.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(domains)
    }

    async fn state(&self, name: &str) -> Result<State, Error> {
        let mut inner = self.lock();
        Self::begin(&mut inner, "state", name)?;
        inner
            .domains
            .get(name)
            .map(|domain| domain.state)
            .ok_or_else(|| Error::DomainNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ARCH_AMD64, DomainSpec, domain_xml};

    fn base_spec() -> DomainSpec {
        DomainSpec {
            name: "bento-web".to_string(),
            uuid: "6d1e0f1c-9a3b-4f6e-8a2d-3c5b7e9f1a2b".to_string(),
            vcpu: 2,
            memory_mib: 2048,
            disk_path: "/var/lib/bento/instances/6d1e0f1c.qcow2".to_string(),
            iso_path: String::new(),
            network: "bento-user-1".to_string(),
            mac: "52:54:00:ab:cd:ef".to_string(),
            nested: false,
            ksm: true,
            arch: ARCH_AMD64.to_string(),
        }
    }

    async fn fake_with_domain() -> Fake {
        let fake = Fake::default();
        fake.create(&domain_xml(&base_spec()).unwrap())
            .await
            .unwrap();
        fake
    }

    #[tokio::test]
    async fn fake_lifecycle() {
        let fake = fake_with_domain().await;
        let domain = fake.domain("bento-web").unwrap();
        assert_eq!(domain.uuid, base_spec().uuid);
        assert!(!domain.autostart);
        assert_eq!(fake.state("bento-web").await.unwrap(), State::Running);

        assert_eq!(fake.stop("bento-web").await.unwrap(), StopResult::Graceful);
        assert_eq!(fake.state("bento-web").await.unwrap(), State::Stopped);
        assert_eq!(
            fake.stop("bento-web").await.unwrap(),
            StopResult::AlreadyStopped
        );
        fake.start("bento-web").await.unwrap();
        fake.reboot("bento-web").await.unwrap();
        fake.remove("bento-web").await.unwrap();
        assert!(matches!(
            fake.state("bento-web").await,
            Err(Error::DomainNotFound(name)) if name == "bento-web"
        ));
    }

    #[tokio::test]
    async fn fake_create_rejects_duplicate_and_bad_xml() {
        let fake = fake_with_domain().await;
        assert!(matches!(
            fake.create(&domain_xml(&base_spec()).unwrap()).await,
            Err(Error::DomainExists(name)) if name == "bento-web"
        ));
        assert!(Fake::default().create("not xml <").await.is_err());
        assert!(
            Fake::default()
                .create("<domain><name>x</name></domain>")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fake_unknown_domain_errors() {
        let fake = Fake::default();
        assert!(matches!(
            fake.start("ghost").await,
            Err(Error::DomainNotFound(_))
        ));
        assert!(matches!(
            fake.stop("ghost").await,
            Err(Error::DomainNotFound(_))
        ));
        assert!(matches!(
            fake.reboot("ghost").await,
            Err(Error::DomainNotFound(_))
        ));
        assert!(matches!(
            fake.remove("ghost").await,
            Err(Error::DomainNotFound(_))
        ));
    }

    #[tokio::test]
    async fn fake_list_is_sorted_and_can_be_seeded() {
        let fake = Fake::default();
        fake.set_domain(FakeDomain {
            name: "b-vm".to_string(),
            uuid: "u2".to_string(),
            xml: String::new(),
            state: State::Stopped,
            autostart: false,
        });
        fake.set_domain(FakeDomain {
            name: "a-vm".to_string(),
            uuid: "u1".to_string(),
            xml: String::new(),
            state: State::Running,
            autostart: false,
        });
        let domains = fake.list().await.unwrap();
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].name, "a-vm");
        assert_eq!(domains[1].name, "b-vm");
        assert_eq!(domains[0].state, State::Running);
        assert_eq!(domains[1].state, State::Stopped);
    }

    #[tokio::test]
    async fn fake_hook_and_forced_stop() {
        let fake = fake_with_domain().await;
        fake.set_hook(|operation, name| {
            if operation == "start" && name == "bento-web" {
                Err(Error::Operation("boom".to_string()))
            } else {
                Ok(())
            }
        });
        assert!(fake.start("bento-web").await.is_err());

        fake.clear_hook();
        fake.set_force_stop(true);
        assert_eq!(fake.stop("bento-web").await.unwrap(), StopResult::Forced);
        assert_eq!(
            fake.calls().first().map(String::as_str),
            Some("create bento-web")
        );
    }
}
