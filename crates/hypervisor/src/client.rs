use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bento_types::State;

use crate::DEFAULT_SOCKET_PATH;
use crate::error::{ApiError, ERR_NO_DOMAIN, ERR_NO_NETWORK, Error, operation_error};
use crate::rpc::RpcApi;

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
// Every domain boots UEFI through OVMF (SPEC 5), so it owns an NVRAM
// file. Removing that file with the definition is part of the four-step
// removal in SPEC 11.1.
const DOMAIN_UNDEFINE_NVRAM: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Domain {
    pub(crate) name: String,
    pub(crate) uuid: [u8; 16],
    pub(crate) id: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Network {
    pub(crate) name: String,
    pub(crate) uuid: [u8; 16],
}

#[async_trait]
pub(crate) trait LibvirtApi: Send + Sync {
    async fn domain_define_xml(&self, xml: &str) -> Result<Domain, ApiError>;
    async fn domain_create(&self, domain: &Domain) -> Result<(), ApiError>;
    async fn domain_shutdown(&self, domain: &Domain) -> Result<(), ApiError>;
    async fn domain_reboot(&self, domain: &Domain, flags: u32) -> Result<(), ApiError>;
    async fn domain_destroy(&self, domain: &Domain) -> Result<(), ApiError>;
    async fn domain_undefine_flags(&self, domain: &Domain, flags: u32) -> Result<(), ApiError>;
    async fn domain_set_autostart(&self, domain: &Domain, value: i32) -> Result<(), ApiError>;
    async fn domain_lookup_by_name(&self, name: &str) -> Result<Domain, ApiError>;
    async fn domain_get_state(&self, domain: &Domain, flags: u32) -> Result<(i32, i32), ApiError>;
    async fn connect_list_all_domains(
        &self,
        need_results: i32,
        flags: u32,
    ) -> Result<(Vec<Domain>, u32), ApiError>;
}

#[async_trait]
pub(crate) trait NetworkApi: Send + Sync {
    async fn network_lookup_by_name(&self, name: &str) -> Result<Network, ApiError>;
    async fn network_define_xml(&self, xml: &str) -> Result<Network, ApiError>;
    async fn network_create(&self, network: &Network) -> Result<(), ApiError>;
    async fn network_is_active(&self, network: &Network) -> Result<i32, ApiError>;
    async fn network_set_autostart(&self, network: &Network, value: i32) -> Result<(), ApiError>;
}

#[async_trait]
trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

#[derive(Debug)]
struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Reports which path a stop took (SPEC 11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopResult {
    /// The guest honored the ACPI shutdown request.
    Graceful,
    /// The 60-second wait expired and the domain was destroyed.
    Forced,
    /// The domain was already shut off.
    AlreadyStopped,
}

/// One libvirt domain with its observed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainInfo {
    pub name: String,
    pub uuid: String,
    pub state: State,
}

/// The consumer-facing libvirt surface. Every method maps to the action
/// table in SPEC 11.1.
#[async_trait]
pub trait Hypervisor: Send + Sync {
    /// Defines a domain, clears libvirt autostart, and starts it.
    async fn create(&self, xml: &str) -> Result<(), Error>;
    /// Starts an already-defined domain.
    async fn start(&self, name: &str) -> Result<(), Error>;
    /// Requests ACPI shutdown, waits, and destroys only after the timeout.
    async fn stop(&self, name: &str) -> Result<StopResult, Error>;
    /// Requests a guest reboot.
    async fn reboot(&self, name: &str) -> Result<(), Error>;
    /// Destroys a running domain and undefines it with its NVRAM file.
    async fn remove(&self, name: &str) -> Result<(), Error>;
    /// Lists defined and running domains with their observed state.
    async fn list(&self) -> Result<Vec<DomainInfo>, Error>;
    /// Reads one domain's observed state.
    async fn state(&self, name: &str) -> Result<State, Error>;
}

/// Optional capability for replacing persistent domain XML.
#[async_trait]
pub trait Definer: Send + Sync {
    async fn define(&self, xml: &str) -> Result<(), Error>;
}

/// Optional capability for making the control plane the only reboot
/// restorer (SPEC 11.2).
#[async_trait]
pub trait AutostartClearer: Send + Sync {
    async fn clear_autostart(&self, name: &str) -> Result<(), Error>;
}

/// Optional capability for per-user libvirt networks (SPEC 6.2).
#[async_trait]
pub trait NetworkManager: Send + Sync {
    async fn ensure_network(&self, name: &str, xml: &str) -> Result<(), Error>;
}

/// A client for the libvirt XDR RPC protocol on a local Unix socket
/// (SPEC 4.1).
pub struct Client {
    api: Arc<dyn LibvirtApi>,
    network_api: Option<Arc<dyn NetworkApi>>,
    rpc: Option<Arc<RpcApi>>,
    stop_timeout: Duration,
    poll_interval: Duration,
    sleeper: Arc<dyn Sleeper>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("has_network_api", &self.network_api.is_some())
            .field("stop_timeout", &self.stop_timeout)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connects to `qemu:///system`. An empty path uses
    /// [`DEFAULT_SOCKET_PATH`].
    pub async fn connect(socket_path: impl AsRef<Path>) -> Result<Self, Error> {
        let supplied = socket_path.as_ref();
        let path = if supplied.as_os_str().is_empty() {
            Path::new(DEFAULT_SOCKET_PATH)
        } else {
            supplied
        };
        let rpc = Arc::new(RpcApi::connect(path, 0).await.map_err(|error| {
            operation_error(format!("connect to libvirtd at {}", path.display()), error)
        })?);
        let api: Arc<dyn LibvirtApi> = rpc.clone();
        let network_api: Arc<dyn NetworkApi> = rpc.clone();
        Ok(Self {
            api,
            network_api: Some(network_api),
            rpc: Some(rpc),
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            sleeper: Arc::new(TokioSleeper),
        })
    }

    #[cfg(test)]
    fn new(api: Arc<dyn LibvirtApi>) -> Self {
        Self {
            api,
            network_api: None,
            rpc: None,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            sleeper: Arc::new(TokioSleeper),
        }
    }

    #[cfg(test)]
    fn new_with_network(api: Arc<dyn LibvirtApi>, network_api: Arc<dyn NetworkApi>) -> Self {
        Self {
            network_api: Some(network_api),
            ..Self::new(api)
        }
    }

    /// Sends `ConnectClose` to libvirt. Clients built over a test API have
    /// no socket and close as a no-op.
    pub async fn close(&self) -> Result<(), Error> {
        if let Some(rpc) = &self.rpc {
            rpc.close()
                .await
                .map_err(|error| operation_error("close libvirt connection", error))?;
        }
        Ok(())
    }

    /// Returns libvirt's host capabilities XML.
    pub async fn capabilities(&self) -> Result<String, Error> {
        let rpc = self.rpc.as_ref().ok_or_else(|| {
            Error::Operation("hypervisor: this client has no libvirt connection".to_string())
        })?;
        rpc.capabilities()
            .await
            .map_err(|error| operation_error("get libvirt capabilities", error))
    }

    async fn lookup(&self, name: &str) -> Result<Domain, Error> {
        match self.api.domain_lookup_by_name(name).await {
            Ok(domain) => Ok(domain),
            Err(ApiError::Libvirt(error)) if error.code == ERR_NO_DOMAIN => {
                Err(Error::DomainNotFound(name.to_string()))
            }
            Err(error) => Err(operation_error(format!("lookup domain {name}"), error)),
        }
    }

    async fn domain_state(&self, domain: &Domain) -> Result<State, Error> {
        let (state, _) = self
            .api
            .domain_get_state(domain, 0)
            .await
            .map_err(|error| {
                operation_error(format!("get state of domain {}", domain.name), error)
            })?;
        Ok(state_from_libvirt(state))
    }
}

#[async_trait]
impl Hypervisor for Client {
    /// A failed start undefines the new definition so a retry cannot hit a
    /// stale domain. Autostart is cleared first because Bento restores the
    /// desired state after a host reboot (SPEC 11.2).
    async fn create(&self, xml: &str) -> Result<(), Error> {
        let domain = self
            .api
            .domain_define_xml(xml)
            .await
            .map_err(|error| operation_error("define domain", error))?;
        if let Err(error) = self.api.domain_set_autostart(&domain, 0).await {
            let _ = self
                .api
                .domain_undefine_flags(&domain, DOMAIN_UNDEFINE_NVRAM)
                .await;
            return Err(operation_error(
                format!("clear autostart on {}", domain.name),
                error,
            ));
        }
        if let Err(error) = self.api.domain_create(&domain).await {
            let _ = self
                .api
                .domain_undefine_flags(&domain, DOMAIN_UNDEFINE_NVRAM)
                .await;
            return Err(operation_error(
                format!("start domain {}", domain.name),
                error,
            ));
        }
        Ok(())
    }

    async fn start(&self, name: &str) -> Result<(), Error> {
        let domain = self.lookup(name).await?;
        self.api
            .domain_create(&domain)
            .await
            .map_err(|error| operation_error(format!("start domain {name}"), error))
    }

    async fn stop(&self, name: &str) -> Result<StopResult, Error> {
        let domain = self.lookup(name).await?;
        if self.domain_state(&domain).await? == State::Stopped {
            return Ok(StopResult::AlreadyStopped);
        }
        self.api
            .domain_shutdown(&domain)
            .await
            .map_err(|error| operation_error(format!("shutdown domain {name}"), error))?;

        let mut elapsed = Duration::ZERO;
        loop {
            if self.domain_state(&domain).await? == State::Stopped {
                return Ok(StopResult::Graceful);
            }
            if elapsed >= self.stop_timeout {
                break;
            }
            self.sleeper.sleep(self.poll_interval).await;
            elapsed += self.poll_interval;
        }
        self.api.domain_destroy(&domain).await.map_err(|error| {
            operation_error(
                format!("destroy domain {name} after shutdown timeout"),
                error,
            )
        })?;
        Ok(StopResult::Forced)
    }

    async fn reboot(&self, name: &str) -> Result<(), Error> {
        let domain = self.lookup(name).await?;
        self.api
            .domain_reboot(&domain, 0)
            .await
            .map_err(|error| operation_error(format!("reboot domain {name}"), error))
    }

    async fn remove(&self, name: &str) -> Result<(), Error> {
        let domain = self.lookup(name).await?;
        if self.domain_state(&domain).await? != State::Stopped {
            self.api
                .domain_destroy(&domain)
                .await
                .map_err(|error| operation_error(format!("destroy domain {name}"), error))?;
        }
        self.api
            .domain_undefine_flags(&domain, DOMAIN_UNDEFINE_NVRAM)
            .await
            .map_err(|error| operation_error(format!("undefine domain {name}"), error))
    }

    async fn list(&self) -> Result<Vec<DomainInfo>, Error> {
        let (domains, _) = self
            .api
            .connect_list_all_domains(1, 0)
            .await
            .map_err(|error| operation_error("list domains", error))?;
        let mut infos = Vec::with_capacity(domains.len());
        for domain in domains {
            infos.push(DomainInfo {
                name: domain.name.clone(),
                uuid: format_uuid(domain.uuid),
                state: self.domain_state(&domain).await?,
            });
        }
        Ok(infos)
    }

    async fn state(&self, name: &str) -> Result<State, Error> {
        let domain = self.lookup(name).await?;
        self.domain_state(&domain).await
    }
}

#[async_trait]
impl Definer for Client {
    async fn define(&self, xml: &str) -> Result<(), Error> {
        self.api
            .domain_define_xml(xml)
            .await
            .map(|_| ())
            .map_err(|error| operation_error("define domain", error))
    }
}

#[async_trait]
impl AutostartClearer for Client {
    async fn clear_autostart(&self, name: &str) -> Result<(), Error> {
        let domain = self.lookup(name).await?;
        self.api
            .domain_set_autostart(&domain, 0)
            .await
            .map_err(|error| operation_error(format!("clear autostart on {name}"), error))
    }
}

#[async_trait]
impl NetworkManager for Client {
    async fn ensure_network(&self, name: &str, xml: &str) -> Result<(), Error> {
        let api = self.network_api.as_ref().ok_or_else(|| {
            Error::Operation("hypervisor: this connection cannot manage networks".to_string())
        })?;
        let network = match api.network_lookup_by_name(name).await {
            Ok(network) => network,
            Err(ApiError::Libvirt(error)) if error.code == ERR_NO_NETWORK => api
                .network_define_xml(xml)
                .await
                .map_err(|error| operation_error(format!("define network {name}"), error))?,
            Err(error) => {
                return Err(operation_error(format!("lookup network {name}"), error));
            }
        };
        api.network_set_autostart(&network, 1)
            .await
            .map_err(|error| operation_error(format!("set autostart on network {name}"), error))?;
        let active = api
            .network_is_active(&network)
            .await
            .map_err(|error| operation_error(format!("check network {name}"), error))?;
        if active == 0 {
            api.network_create(&network)
                .await
                .map_err(|error| operation_error(format!("start network {name}"), error))?;
        }
        Ok(())
    }
}

fn state_from_libvirt(state: i32) -> State {
    match state {
        // running, blocked, paused, shutdown, and pmsuspended all have a
        // live QEMU process. Only its absence counts as stopped (SPEC 11.1).
        1 | 2 | 3 | 4 | 7 => State::Running,
        // nostate, shutoff, and crashed.
        _ => State::Stopped,
    }
}

fn format_uuid(uuid: [u8; 16]) -> String {
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(&uuid[0..4]),
        hex(&uuid[4..6]),
        hex(&uuid[6..8]),
        hex(&uuid[8..10]),
        hex(&uuid[10..16])
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::pending;
    use std::sync::{Mutex, MutexGuard};

    use super::*;
    use crate::LibvirtError;

    const DOMAIN_RUNNING: i32 = 1;
    const DOMAIN_SHUTOFF: i32 = 5;

    #[derive(Debug, Clone)]
    struct ApiDomain {
        domain: Domain,
        state: i32,
        autostart: i32,
        xml: String,
    }

    #[derive(Debug, Clone)]
    struct ApiNetwork {
        network: Network,
        active: i32,
        autostart: i32,
    }

    #[derive(Debug, Clone)]
    enum InjectedError {
        Message(String),
        Libvirt(LibvirtError),
    }

    impl InjectedError {
        fn api(&self) -> ApiError {
            match self {
                Self::Message(message) => ApiError::Protocol(message.clone()),
                Self::Libvirt(error) => ApiError::Libvirt(error.clone()),
            }
        }
    }

    #[derive(Debug)]
    struct FakeApiInner {
        domains: HashMap<String, ApiDomain>,
        calls: Vec<String>,
        err_on: HashMap<String, InjectedError>,
        shutdown_after_polls: i32,
        polls_since_shutdown: i32,
        shutting_down: String,
        undefine_flags: Vec<u32>,
        networks: HashMap<String, ApiNetwork>,
        net_calls: Vec<String>,
        net_err_on: HashMap<String, InjectedError>,
        define_xml: String,
    }

    impl Default for FakeApiInner {
        fn default() -> Self {
            Self {
                domains: HashMap::new(),
                calls: Vec::new(),
                err_on: HashMap::new(),
                shutdown_after_polls: -1,
                polls_since_shutdown: 0,
                shutting_down: String::new(),
                undefine_flags: Vec::new(),
                networks: HashMap::new(),
                net_calls: Vec::new(),
                net_err_on: HashMap::new(),
                define_xml: String::new(),
            }
        }
    }

    #[derive(Debug, Default)]
    struct FakeApi {
        inner: Mutex<FakeApiInner>,
    }

    impl FakeApi {
        fn lock(&self) -> MutexGuard<'_, FakeApiInner> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn add(&self, name: &str, state: i32) {
            let mut inner = self.lock();
            let id = inner.domains.len() as i32 + 1;
            inner.domains.insert(
                name.to_string(),
                ApiDomain {
                    domain: Domain {
                        name: name.to_string(),
                        uuid: {
                            let mut uuid = [0; 16];
                            uuid[0] = 1;
                            uuid
                        },
                        id,
                    },
                    state,
                    autostart: 1,
                    xml: String::new(),
                },
            );
        }

        fn fail(&self, operation: &str, message: &str) {
            self.lock().err_on.insert(
                operation.to_string(),
                InjectedError::Message(message.to_string()),
            );
        }

        fn call(inner: &mut FakeApiInner, operation: &str) -> Result<(), ApiError> {
            inner.calls.push(operation.to_string());
            if let Some(error) = inner.err_on.get(operation) {
                return Err(error.api());
            }
            Ok(())
        }

        fn net_call(inner: &mut FakeApiInner, operation: &str) -> Result<(), ApiError> {
            inner.net_calls.push(operation.to_string());
            if let Some(error) = inner.net_err_on.get(operation) {
                return Err(error.api());
            }
            Ok(())
        }
    }

    #[async_trait]
    impl LibvirtApi for FakeApi {
        async fn domain_define_xml(&self, xml: &str) -> Result<Domain, ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "define")?;
            let domain = Domain {
                name: "defined".to_string(),
                uuid: [0; 16],
                id: -1,
            };
            inner.domains.insert(
                domain.name.clone(),
                ApiDomain {
                    domain: domain.clone(),
                    state: DOMAIN_SHUTOFF,
                    autostart: 1,
                    xml: xml.to_string(),
                },
            );
            Ok(domain)
        }

        async fn domain_create(&self, domain: &Domain) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "create")?;
            inner
                .domains
                .get_mut(&domain.name)
                .ok_or_else(|| ApiError::Protocol(format!("no such domain {:?}", domain.name)))?
                .state = DOMAIN_RUNNING;
            Ok(())
        }

        async fn domain_shutdown(&self, domain: &Domain) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "shutdown")?;
            if !inner.domains.contains_key(&domain.name) {
                return Err(ApiError::Protocol(format!(
                    "no such domain {:?}",
                    domain.name
                )));
            }
            inner.shutting_down.clone_from(&domain.name);
            inner.polls_since_shutdown = 0;
            Ok(())
        }

        async fn domain_reboot(&self, domain: &Domain, _flags: u32) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "reboot")?;
            if inner.domains.contains_key(&domain.name) {
                Ok(())
            } else {
                Err(ApiError::Protocol(format!(
                    "no such domain {:?}",
                    domain.name
                )))
            }
        }

        async fn domain_destroy(&self, domain: &Domain) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "destroy")?;
            inner
                .domains
                .get_mut(&domain.name)
                .ok_or_else(|| ApiError::Protocol(format!("no such domain {:?}", domain.name)))?
                .state = DOMAIN_SHUTOFF;
            Ok(())
        }

        async fn domain_undefine_flags(&self, domain: &Domain, flags: u32) -> Result<(), ApiError> {
            let mut inner = self.lock();
            inner.undefine_flags.push(flags);
            Self::call(&mut inner, "undefine")?;
            if flags & DOMAIN_UNDEFINE_NVRAM == 0 {
                return Err(ApiError::Protocol(format!(
                    "cannot undefine domain {}: it owns an NVRAM file",
                    domain.name
                )));
            }
            inner
                .domains
                .remove(&domain.name)
                .ok_or_else(|| ApiError::Protocol(format!("no such domain {:?}", domain.name)))?;
            Ok(())
        }

        async fn domain_set_autostart(&self, domain: &Domain, value: i32) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "autostart")?;
            inner
                .domains
                .get_mut(&domain.name)
                .ok_or_else(|| ApiError::Protocol(format!("no such domain {:?}", domain.name)))?
                .autostart = value;
            Ok(())
        }

        async fn domain_lookup_by_name(&self, name: &str) -> Result<Domain, ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "lookup")?;
            inner
                .domains
                .get(name)
                .map(|domain| domain.domain.clone())
                .ok_or_else(|| ApiError::Protocol(format!("no such domain {name:?}")))
        }

        async fn domain_get_state(
            &self,
            domain: &Domain,
            _flags: u32,
        ) -> Result<(i32, i32), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "state")?;
            if inner.shutting_down == domain.name && inner.shutdown_after_polls >= 0 {
                inner.polls_since_shutdown += 1;
                if inner.polls_since_shutdown > inner.shutdown_after_polls {
                    inner.domains.get_mut(&domain.name).unwrap().state = DOMAIN_SHUTOFF;
                }
            }
            let state = inner
                .domains
                .get(&domain.name)
                .ok_or_else(|| ApiError::Protocol(format!("no such domain {:?}", domain.name)))?
                .state;
            Ok((state, 0))
        }

        async fn connect_list_all_domains(
            &self,
            _need_results: i32,
            _flags: u32,
        ) -> Result<(Vec<Domain>, u32), ApiError> {
            let mut inner = self.lock();
            Self::call(&mut inner, "list")?;
            let domains: Vec<_> = inner
                .domains
                .values()
                .map(|entry| entry.domain.clone())
                .collect();
            let count = domains.len() as u32;
            Ok((domains, count))
        }
    }

    #[async_trait]
    impl NetworkApi for FakeApi {
        async fn network_lookup_by_name(&self, name: &str) -> Result<Network, ApiError> {
            let mut inner = self.lock();
            Self::net_call(&mut inner, "lookup")?;
            inner
                .networks
                .get(name)
                .map(|entry| entry.network.clone())
                .ok_or_else(|| {
                    ApiError::Libvirt(LibvirtError {
                        code: ERR_NO_NETWORK,
                        message: "network not found".to_string(),
                    })
                })
        }

        async fn network_define_xml(&self, xml: &str) -> Result<Network, ApiError> {
            let mut inner = self.lock();
            Self::net_call(&mut inner, "define")?;
            inner.define_xml = xml.to_string();
            let network = Network {
                name: "defined".to_string(),
                uuid: [0; 16],
            };
            inner.networks.insert(
                network.name.clone(),
                ApiNetwork {
                    network: network.clone(),
                    active: 0,
                    autostart: 0,
                },
            );
            Ok(network)
        }

        async fn network_create(&self, network: &Network) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::net_call(&mut inner, "start")?;
            if let Some(entry) = inner.networks.get_mut(&network.name) {
                entry.active = 1;
            }
            Ok(())
        }

        async fn network_is_active(&self, network: &Network) -> Result<i32, ApiError> {
            let mut inner = self.lock();
            Self::net_call(&mut inner, "isactive")?;
            Ok(inner
                .networks
                .get(&network.name)
                .map_or(0, |entry| entry.active))
        }

        async fn network_set_autostart(
            &self,
            network: &Network,
            value: i32,
        ) -> Result<(), ApiError> {
            let mut inner = self.lock();
            Self::net_call(&mut inner, "autostart")?;
            if let Some(entry) = inner.networks.get_mut(&network.name) {
                entry.autostart = value;
            }
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct ImmediateSleeper {
        durations: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl Sleeper for ImmediateSleeper {
        async fn sleep(&self, duration: Duration) {
            self.durations.lock().unwrap().push(duration);
        }
    }

    struct PendingSleeper;

    #[async_trait]
    impl Sleeper for PendingSleeper {
        async fn sleep(&self, _duration: Duration) {
            pending::<()>().await;
        }
    }

    fn test_client(api: Arc<FakeApi>) -> (Client, Arc<ImmediateSleeper>) {
        let sleeper = Arc::new(ImmediateSleeper::default());
        let mut client = Client::new(api);
        client.sleeper = sleeper.clone();
        (client, sleeper)
    }

    #[tokio::test]
    async fn client_create_clears_autostart() {
        let api = Arc::new(FakeApi::default());
        let (client, _) = test_client(api.clone());
        client.create("<domain/>").await.unwrap();
        let inner = api.lock();
        let domain = inner.domains.get("defined").unwrap();
        assert_eq!(domain.autostart, 0);
        assert_eq!(domain.state, DOMAIN_RUNNING);
        assert_eq!(inner.calls, ["define", "autostart", "create"]);
    }

    #[tokio::test]
    async fn client_create_undefines_on_start_failure() {
        let api = Arc::new(FakeApi::default());
        api.fail("create", "no memory");
        let (client, _) = test_client(api.clone());
        assert!(client.create("<domain/>").await.is_err());
        assert!(!api.lock().domains.contains_key("defined"));
    }

    #[tokio::test]
    async fn client_stop_gracefully() {
        let api = Arc::new(FakeApi::default());
        api.add("web", DOMAIN_RUNNING);
        api.lock().shutdown_after_polls = 3;
        let (client, sleeper) = test_client(api.clone());
        assert_eq!(client.stop("web").await.unwrap(), StopResult::Graceful);
        assert!(!api.lock().calls.iter().any(|call| call == "destroy"));
        assert_eq!(sleeper.durations.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn client_stop_forces_after_sixty_seconds() {
        let api = Arc::new(FakeApi::default());
        api.add("web", DOMAIN_RUNNING);
        let (client, sleeper) = test_client(api.clone());
        assert_eq!(client.stop("web").await.unwrap(), StopResult::Forced);
        assert_eq!(api.lock().domains["web"].state, DOMAIN_SHUTOFF);
        let durations = sleeper.durations.lock().unwrap();
        assert_eq!(
            durations.len(),
            (DEFAULT_STOP_TIMEOUT.as_millis() / DEFAULT_POLL_INTERVAL.as_millis()) as usize
        );
        assert_eq!(durations.iter().sum::<Duration>(), DEFAULT_STOP_TIMEOUT);
    }

    #[tokio::test]
    async fn client_stop_of_stopped_domain_is_a_noop() {
        let api = Arc::new(FakeApi::default());
        api.add("web", DOMAIN_SHUTOFF);
        let (client, _) = test_client(api.clone());
        assert_eq!(
            client.stop("web").await.unwrap(),
            StopResult::AlreadyStopped
        );
        assert!(
            !api.lock()
                .calls
                .iter()
                .any(|call| matches!(call.as_str(), "shutdown" | "destroy"))
        );
    }

    #[tokio::test]
    async fn client_stop_is_cancel_safe_while_sleeping() {
        let api = Arc::new(FakeApi::default());
        api.add("web", DOMAIN_RUNNING);
        let mut client = Client::new(api);
        client.sleeper = Arc::new(PendingSleeper);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), client.stop("web"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn client_remove_destroys_only_a_running_domain() {
        for (state, should_destroy) in [(DOMAIN_RUNNING, true), (DOMAIN_SHUTOFF, false)] {
            let api = Arc::new(FakeApi::default());
            api.add("web", state);
            let (client, _) = test_client(api.clone());
            client.remove("web").await.unwrap();
            let inner = api.lock();
            assert!(!inner.domains.contains_key("web"));
            assert_eq!(
                inner.calls.iter().any(|call| call == "destroy"),
                should_destroy
            );
        }
    }

    #[tokio::test]
    async fn every_undefine_carries_the_nvram_flag() {
        let api = Arc::new(FakeApi::default());
        api.add("web", DOMAIN_RUNNING);
        let (client, _) = test_client(api.clone());
        client.remove("web").await.unwrap();
        assert_eq!(api.lock().undefine_flags, [DOMAIN_UNDEFINE_NVRAM]);

        let api = Arc::new(FakeApi::default());
        api.fail("create", "no memory");
        let (client, _) = test_client(api.clone());
        assert!(client.create("<domain/>").await.is_err());
        assert_eq!(api.lock().undefine_flags, [DOMAIN_UNDEFINE_NVRAM]);
    }

    #[tokio::test]
    async fn client_list_and_state() {
        let api = Arc::new(FakeApi::default());
        api.add("running-vm", DOMAIN_RUNNING);
        api.add("stopped-vm", DOMAIN_SHUTOFF);
        let (client, _) = test_client(api);
        let domains = client.list().await.unwrap();
        assert_eq!(domains.len(), 2);
        let by_name: HashMap<_, _> = domains
            .into_iter()
            .map(|domain| {
                assert!(!domain.uuid.is_empty());
                (domain.name, domain.state)
            })
            .collect();
        assert_eq!(by_name["running-vm"], State::Running);
        assert_eq!(by_name["stopped-vm"], State::Stopped);
        assert_eq!(client.state("running-vm").await.unwrap(), State::Running);
        assert!(client.state("missing").await.is_err());
    }

    #[test]
    fn libvirt_states_collapse_to_observed_states() {
        for state in [1, 2, 3, 4, 7] {
            assert_eq!(state_from_libvirt(state), State::Running);
        }
        for state in [0, 5, 6] {
            assert_eq!(state_from_libvirt(state), State::Stopped);
        }
    }

    #[test]
    fn uuid_uses_the_canonical_form() {
        let uuid = [
            0x6d, 0x1e, 0x0f, 0x1c, 0x9a, 0x3b, 0x4f, 0x6e, 0x8a, 0x2d, 0x3c, 0x5b, 0x7e, 0x9f,
            0x1a, 0x2b,
        ];
        assert_eq!(format_uuid(uuid), "6d1e0f1c-9a3b-4f6e-8a2d-3c5b7e9f1a2b");
    }

    #[tokio::test]
    async fn client_define() {
        let api = Arc::new(FakeApi::default());
        let client = Client::new(api.clone());
        client.define("<domain/>").await.unwrap();
        assert_eq!(api.lock().domains["defined"].xml, "<domain/>");
        api.fail("define", "boom");
        assert!(client.define("<domain/>").await.is_err());
    }

    #[tokio::test]
    async fn client_clear_autostart() {
        let api = Arc::new(FakeApi::default());
        api.add("web", DOMAIN_SHUTOFF);
        let client = Client::new(api.clone());
        client.clear_autostart("web").await.unwrap();
        assert_eq!(api.lock().domains["web"].autostart, 0);
    }

    #[tokio::test]
    async fn client_ensures_network() {
        for case in ["missing", "inactive", "active", "lookup failure"] {
            let api = Arc::new(FakeApi::default());
            let expected: &[&str] = match case {
                "missing" => &["lookup", "define", "autostart", "isactive", "start"],
                "inactive" => {
                    api.lock().networks.insert(
                        "bento-user-1".to_string(),
                        ApiNetwork {
                            network: Network {
                                name: "bento-user-1".to_string(),
                                uuid: [0; 16],
                            },
                            active: 0,
                            autostart: 0,
                        },
                    );
                    &["lookup", "autostart", "isactive", "start"]
                }
                "active" => {
                    api.lock().networks.insert(
                        "bento-user-1".to_string(),
                        ApiNetwork {
                            network: Network {
                                name: "bento-user-1".to_string(),
                                uuid: [0; 16],
                            },
                            active: 1,
                            autostart: 0,
                        },
                    );
                    &["lookup", "autostart", "isactive"]
                }
                "lookup failure" => {
                    api.lock().net_err_on.insert(
                        "lookup".to_string(),
                        InjectedError::Message("connection lost".to_string()),
                    );
                    &["lookup"]
                }
                _ => unreachable!(),
            };
            let domain_api: Arc<dyn LibvirtApi> = api.clone();
            let network_api: Arc<dyn NetworkApi> = api.clone();
            let client = Client::new_with_network(domain_api, network_api);
            let result = client.ensure_network("bento-user-1", "<network/>").await;
            assert_eq!(
                result.is_err(),
                case == "lookup failure",
                "{case}: {result:?}"
            );
            let calls = api.lock().net_calls.clone();
            assert_eq!(calls, expected, "{case}");
        }
    }

    #[tokio::test]
    async fn client_without_network_api_cannot_manage_networks() {
        let client = Client::new(Arc::new(FakeApi::default()));
        assert!(client.ensure_network("n", "<network/>").await.is_err());
    }

    #[tokio::test]
    async fn lookup_maps_no_domain_error() {
        let api = Arc::new(FakeApi::default());
        api.lock().err_on.insert(
            "lookup".to_string(),
            InjectedError::Libvirt(LibvirtError {
                code: ERR_NO_DOMAIN,
                message: "no domain".to_string(),
            }),
        );
        let client = Client::new(api);
        assert!(matches!(
            client.state("ghost").await,
            Err(Error::DomainNotFound(_))
        ));
    }
}
