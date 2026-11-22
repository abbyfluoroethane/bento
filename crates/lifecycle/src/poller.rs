use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bento_types::{Instance, State};

use crate::{Error, Manager, Result};

/// How long an instance with a seed ISO must run without a break before
/// the seed is detached and deleted. QEMU starts in a moment, but
/// cloud-init reads the seed only when the guest has booted, and a guest
/// that stops or crashes before that would boot again with no keys and no
/// network. SPEC 5.2 asks for a successful first boot, so the cleanup
/// waits for continuous `Running`. The time lives in memory: a restart of
/// bentod starts it again, which only delays the cleanup.
pub(crate) const FIRST_BOOT_SETTLE: Duration = Duration::from_secs(2 * 60);

impl Manager {
    /// Runs one observed-state poll (SPEC 12), updates all rows together,
    /// then finishes first boot for running instances whose seed remains.
    /// The sweep uses only this host's rows (MULTI-NODE 21).
    pub async fn poll_once(&self) -> Result<()> {
        let domains = match self.hyp.list().await {
            Ok(domains) => domains,
            Err(error) => {
                // No answer means no proof of an unbroken run (SPEC 5.2).
                self.first_running_lock().clear();
                return Err(Error::operation(format!(
                    "lifecycle: poll: list domains: {error}",
                    error = bento_types::error_chain(&error)
                )));
            }
        };
        let mut states: HashMap<_, _> = domains
            .into_iter()
            .map(|domain| (domain.uuid, domain.state))
            .collect();
        let instances = self
            .store
            .instances_on_host(self.host_id)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: poll: list instances: {error}",
                    error = bento_types::error_chain(&*error)
                ))
            })?;
        let restoring = self.restoring.load(Ordering::SeqCst);
        // Restore writes `starting` before the domain leaves shut off, and
        // the user sees `starting` (SPEC 11.2). Writing the shut-off state
        // over it for the length of the restore would hide that.
        let observed = states.clone();
        if restoring {
            states.retain(|uuid, state| {
                !(*state == State::Stopped
                    && instances
                        .iter()
                        .any(|row| row.uuid == *uuid && row.state == State::Starting))
            });
        }
        self.store
            .update_observed_states(states)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: poll: record observed states: {error}",
                    error = bento_types::error_chain(&*error)
                ))
            })?;
        // libvirt is authoritative (SPEC 12). A row on this host with no
        // domain is not running, whatever the row says. The remote path
        // does the same for a runner. The domain list is a moment old, so
        // libvirt is asked once more before a row is changed: a domain that
        // a start or a create made since then is not a missing domain.
        for row in instances.iter().filter(|row| {
            // A `provisioning` row belongs to a create (SPEC 11.2).
            !matches!(row.state, State::Stopped | State::Provisioning)
                && !observed.contains_key(&row.uuid)
        }) {
            let state = match self.hyp.state(&row.name).await {
                Ok(state) => state,
                Err(bento_hypervisor::Error::DomainNotFound(_)) => State::Stopped,
                Err(error) => {
                    self.log.warn(&format!(
                        "poll: state of {} not read: {error}",
                        row.name,
                        error = bento_types::error_chain(&error)
                    ));
                    continue;
                }
            };
            if let Err(error) = self.store.set_observed_state(&row.uuid, state).await {
                self.log.warn(&format!(
                    "poll: state of {} not recorded: {error}",
                    row.name,
                    error = bento_types::error_chain(&*error)
                ));
            }
        }
        let states = observed;
        // An instance that is not `running` in this poll, or not listed,
        // loses its time. The rest keep the time of their first poll
        // (SPEC 5.2).
        self.first_running_lock().retain(|uuid, _| {
            states.get(uuid) == Some(&State::Running)
                && instances
                    .iter()
                    .any(|row| row.uuid == *uuid && row.state != State::Provisioning)
        });
        for instance in instances {
            if instance.state == State::Provisioning
                || states.get(&instance.uuid) != Some(&State::Running)
            {
                continue;
            }
            if !self.first_boot_settled(&instance.uuid) {
                continue;
            }
            if let Err(error) = self.finish_first_boot(&instance).await {
                self.log.warn(&format!(
                    "poll: first boot cleanup failed; retry next poll: {error}",
                    error = bento_types::error_chain(&error)
                ));
            }
        }
        Ok(())
    }

    /// Polls at the configured interval until `shutdown` resolves. Lifecycle
    /// events cover short transitions that fall between polls (SPEC 12).
    pub async fn run_poller<F>(&self, shutdown: F) -> Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        tokio::pin!(shutdown);
        let mut interval = tokio::time::interval(self.poll_every);
        interval.tick().await;
        loop {
            tokio::select! {
                () = &mut shutdown => return Ok(()),
                _ = interval.tick() => {
                    if let Err(error) = self.poll_once().await {
                        self.log.warn(&format!("poll failed: {error}", error = bento_types::error_chain(&error)));
                    }
                }
            }
        }
    }

    /// Applies one mapped libvirt lifecycle event. Unknown domains are
    /// ignored; reconciliation reports them (SPEC 12).
    pub async fn handle_event(&self, uuid: &str, state: State) -> Result<()> {
        let instance = match self.store.instance(uuid).await {
            Ok(instance) => instance,
            Err(error) => {
                self.log.warn(&format!(
                    "event: no row for {uuid}, ignoring {state}: {error}",
                    error = bento_types::error_chain(&*error)
                ));
                return Ok(());
            }
        };
        // A create owns a `provisioning` row until it ends (SPEC 11.2).
        if instance.state == State::Provisioning {
            return Ok(());
        }
        self.store
            .set_observed_state(uuid, state)
            .await
            .map_err(crate::actions::external)?;
        if state != State::Running {
            self.first_running_lock().remove(uuid);
            return Ok(());
        }
        if self.first_boot_settled(uuid)
            && let Err(error) = self.finish_first_boot(&instance).await
        {
            self.log.warn(&format!(
                "event: first boot cleanup failed; retry next poll: {error}",
                error = bento_types::error_chain(&error)
            ));
        }
        Ok(())
    }

    fn first_running_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, time::OffsetDateTime>> {
        self.first_running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Notes that the instance is `running` now, and says whether it has
    /// been so without a break for [`FIRST_BOOT_SETTLE`]. An instance
    /// with no seed ISO has no cleanup to wait for and is not tracked.
    fn first_boot_settled(&self, uuid: &str) -> bool {
        if !(self.iso_exists)(&self.seed_iso_path(uuid)) {
            self.first_running_lock().remove(uuid);
            return false;
        }
        let now = (self.now)();
        let mut map = self.first_running_lock();
        let since = *map.entry(uuid.to_owned()).or_insert(now);
        now - since >= FIRST_BOOT_SETTLE
    }

    /// Detaches and deletes the owner's public-key seed after first successful
    /// boot (SPEC 5.2). Redefinition affects the persistent domain; the live
    /// device remains until stop. Without the optional capability, the file is
    /// still deleted and a warning identifies the stale definition.
    pub async fn finish_first_boot(&self, snapshot: &Instance) -> Result<()> {
        let path = self.seed_iso_path(&snapshot.uuid);
        if !(self.iso_exists)(&path) {
            self.first_running_lock().remove(&snapshot.uuid);
            return Ok(());
        }
        // An action on this instance is running, such as a resize or a
        // create. The seed stays, and the next poll tries again, so the
        // sweep does not wait behind one slow action.
        let Some(_guard) = self.try_lock_instance(&snapshot.uuid) else {
            return Ok(());
        };
        // The caller's row can be older than a resize that just ended.
        // Building the XML from it would put the old size back.
        let instance = &self
            .store
            .instance(&snapshot.uuid)
            .await
            .map_err(crate::actions::external)?;
        // A row that a crash left `provisioning` has no settled boot to
        // finish (SPEC 11.2).
        if instance.state == State::Provisioning {
            return Ok(());
        }
        if let Some(definer) = &self.definer {
            let xml = self.domain_xml_by_uuid(instance, false).await?;
            definer.define(&xml).await.map_err(|error| {
                Error::operation(format!(
                    "lifecycle: detach seed iso of {}: {error}",
                    instance.name,
                    error = bento_types::error_chain(&error)
                ))
            })?;
        } else {
            self.log.warn("first boot: hypervisor cannot redefine; CD-ROM stays in definition until next redefine");
        }
        (self.delete_iso)(path).await.map_err(|error| {
            Error::operation(format!(
                "lifecycle: delete seed iso of {}: {error}",
                instance.name,
                error = bento_types::error_chain(&*error)
            ))
        })?;
        self.first_running_lock().remove(&snapshot.uuid);
        self.log.info(&format!(
            "first boot complete: seed ISO removed for {}",
            instance.name
        ));
        Ok(())
    }
}
