use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::Arc;

use bento_hypervisor::Hypervisor;
use bento_types::{DesiredState, Instance, State};

use crate::{Error, Manager, Result};

impl Manager {
    /// Restores each instance to its last desired state after host reboot
    /// (SPEC 11.2). Starts are batched and every batch reaches running or
    /// times out before the next, avoiding a simultaneous memory/disk spike.
    /// Restore selects only this host's instances (MULTI-NODE 21).
    ///
    /// First it settles the rows that a crash left `provisioning`
    /// (SPEC 11.2).
    pub async fn restore(&self) -> Result<()> {
        // The poller leaves a `starting` row alone while this runs
        // (SPEC 11.2).
        let _restoring = RestoringFlag::raise(&self.restoring);
        let domains = self.hyp.list().await.map_err(|error| {
            Error::operation(format!(
                "lifecycle: restore: list domains: {error}",
                error = bento_types::error_chain(&error)
            ))
        })?;
        if let Some(clearer) = &self.autostart_clearer {
            for domain in &domains {
                if let Err(error) = clearer.clear_autostart(&domain.name).await {
                    self.log.warn(&format!(
                        "restore: autostart not cleared for {}: {error}",
                        domain.name,
                        error = bento_types::error_chain(&error)
                    ));
                }
            }
        } else {
            self.log.warn("restore: hypervisor cannot clear autostart; persistent definitions catch up when redefined");
        }
        let states: HashMap<_, _> = domains
            .into_iter()
            .map(|domain| (domain.uuid, domain.state))
            .collect();
        // Before the normal work, so that a row that a crash left behind
        // is not read as an instance that is simply stopped.
        self.settle_provisioning(self.host_id, &states).await?;
        let known: HashSet<String> = states.keys().cloned().collect();
        // This also clears a `starting` that an earlier, interrupted
        // restore left on a row whose domain is shut off.
        self.store
            .update_observed_states(states)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: restore: record observed states: {error}",
                    error = bento_types::error_chain(&*error)
                ))
            })?;
        self.start_queued(self.host_id, &self.hyp, &known).await
    }

    /// Restores the instances of one machine, with the rules of
    /// [`Manager::restore`], through that machine's hypervisor. The
    /// runner agent calls it after the machine restarts, because a runner
    /// instance would otherwise stay stopped for an unlimited time
    /// (SPEC 11.2, MULTI-NODE 14.3).
    ///
    /// Two calls for one machine at the same time start a VM once: each
    /// start holds the instance lock and reads the row again first. The
    /// observed states of a runner come from its own observer, so this
    /// call does not write them from the domain list; it settles only the
    /// rows that a crash left `provisioning`.
    pub async fn restore_host(&self, host_id: i64) -> Result<()> {
        let hyp = self.hyp_of_host(host_id).await?;
        let _restoring = HostRestoring::raise(self, host_id);
        let domains = hyp.list().await.map_err(|error| {
            Error::operation(format!(
                "lifecycle: restore: list domains of machine {host_id}: {error}",
                error = bento_types::error_chain(&error)
            ))
        })?;
        let states: HashMap<_, _> = domains
            .into_iter()
            .map(|domain| (domain.uuid, domain.state))
            .collect();
        self.settle_provisioning(host_id, &states).await?;
        let known: HashSet<String> = states.keys().cloned().collect();
        self.start_queued(host_id, &hyp, &known).await
    }

    /// True while a [`Manager::restore_host`] for that machine runs. The
    /// observer of a runner must then leave a `starting` row alone, as the
    /// local poll does (SPEC 11.2, MULTI-NODE 14.3).
    pub fn is_restoring(&self, host_id: i64) -> bool {
        self.restoring_hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&host_id)
    }

    /// The hypervisor of one machine: the local one, or one reached
    /// through its runner (MULTI-NODE 13.1).
    async fn hyp_of_host(&self, host_id: i64) -> Result<Arc<dyn Hypervisor>> {
        if host_id == self.host_id {
            return Ok(Arc::clone(&self.hyp));
        }
        self.fleet.hypervisor(host_id).await.ok_or_else(|| {
            Error::operation(format!(
                "lifecycle: restore: machine {host_id} cannot be reached"
            ))
        })
    }

    /// Handles the rows of one machine that are still `provisioning`
    /// when a restore starts (SPEC 11.2). Each is a create that a crash
    /// cut short, unless that create runs in this process: its action
    /// lock is then held, and the row is left alone.
    ///
    /// A row whose domain exists takes the state libvirt shows. A row with
    /// no domain becomes a stopped instance with desired state stopped, so
    /// that nothing starts it. Bento does not delete it on its own
    /// (SPEC 11.1): the user removes it or creates it again.
    async fn settle_provisioning(
        &self,
        host_id: i64,
        domains: &HashMap<String, State>,
    ) -> Result<()> {
        let rows = self
            .store
            .provisioning_instances(host_id)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: restore: list provisioning rows: {error}",
                    error = bento_types::error_chain(&*error)
                ))
            })?;
        for row in rows {
            let Some(_guard) = self.try_lock_instance(&row.uuid) else {
                continue;
            };
            let outcome = match domains.get(&row.uuid) {
                Some(state) => self.store.finish_provisioning(&row.uuid, *state).await,
                None => match self
                    .store
                    .finish_provisioning(&row.uuid, State::Stopped)
                    .await
                {
                    Ok(true) => self
                        .store
                        .set_desired_state(&row.uuid, DesiredState::Stopped)
                        .await
                        .map(|()| true),
                    other => other,
                },
            };
            match outcome {
                Ok(true) if domains.contains_key(&row.uuid) => self.log.info(&format!(
                    "restore: {} was provisioning when bentod stopped; its domain exists, \
                     state taken from libvirt",
                    row.name
                )),
                Ok(true) => self.log.warn(&format!(
                    "restore: {} was provisioning when bentod stopped and has no domain. \
                     It is now a stopped instance: remove it or create it again",
                    row.name
                )),
                Ok(false) => {}
                Err(error) => self.log.warn(&format!(
                    "restore: provisioning row of {} not settled: {error}",
                    row.name,
                    error = bento_types::error_chain(&*error)
                )),
            }
        }
        Ok(())
    }

    /// Starts the desired-running, observed-stopped instances of one
    /// machine in batches (SPEC 11.2). Shared by [`Manager::restore`] and
    /// [`Manager::restore_host`].
    async fn start_queued(
        &self,
        host_id: i64,
        hyp: &Arc<dyn Hypervisor>,
        known: &HashSet<String>,
    ) -> Result<()> {
        let instances = self
            .store
            .instances_to_restore(host_id)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: restore: {error}",
                    error = bento_types::error_chain(&*error)
                ))
            })?;
        if instances.is_empty() {
            self.log.info("restore: nothing to start");
            return Ok(());
        }
        let batches = instances.len().div_ceil(self.batch_size);
        self.log.info(&format!(
            "restore: starting {} instances in {batches} batches",
            instances.len()
        ));
        for (index, batch) in instances.chunks(self.batch_size).enumerate() {
            self.log
                .info(&format!("restore: batch {} starting", index + 1));
            let mut started = Vec::new();
            for queued in batch {
                // A row with no domain cannot be started. Writing
                // `starting` for it would leave a state no start can fix,
                // so it is reported and left for the operator (SPEC 6.1).
                if !known.contains(&queued.uuid) {
                    self.log.warn(&format!(
                        "restore: {} has no domain in libvirt; not started. \
                         Run `bentod reconcile` to see the difference",
                        queued.name
                    ));
                    continue;
                }
                // The action lock, and a fresh row: a `stop` or an `rm`
                // can land while an earlier batch runs, and it must win
                // over the list read above (SPEC 11.1, 11.2). A second
                // restore of the same machine sees `starting` here and
                // skips the row.
                let _guard = self.lock_instance(&queued.uuid).await;
                let Ok(instance) = self.store.instance(&queued.uuid).await else {
                    self.log
                        .info(&format!("restore: {} is gone; skipped", queued.name));
                    continue;
                };
                if instance.desired_state != DesiredState::Running
                    || instance.state != State::Stopped
                {
                    self.log.info(&format!(
                        "restore: {} changed while queued; skipped",
                        instance.name
                    ));
                    continue;
                }
                if let Err(error) = self
                    .store
                    .set_observed_state(&instance.uuid, State::Starting)
                    .await
                {
                    self.log.warn(&format!(
                        "restore: starting state not recorded: {error}",
                        error = bento_types::error_chain(&*error)
                    ));
                }
                if let Err(error) = hyp.start(&instance.name).await {
                    self.log.warn(&format!(
                        "restore: {} did not start: {error}",
                        instance.name,
                        error = bento_types::error_chain(&error)
                    ));
                    self.sync_observed(hyp, &instance).await;
                } else {
                    started.push(instance);
                }
            }
            for instance in started {
                if let Err(error) = self.wait_running(hyp, &instance.name).await {
                    self.log.warn(&format!(
                        "restore: {} did not reach running: {error}",
                        instance.name,
                        error = bento_types::error_chain(&error)
                    ));
                    self.sync_observed(hyp, &instance).await;
                } else if let Err(error) = self
                    .store
                    .set_observed_state(&instance.uuid, State::Running)
                    .await
                {
                    self.log.warn(&format!(
                        "restore: running state not recorded: {error}",
                        error = bento_types::error_chain(&*error)
                    ));
                }
            }
            self.log.info(&format!("restore: batch {} done", index + 1));
        }
        self.log.info("restore: complete");
        Ok(())
    }

    async fn wait_running(&self, hyp: &Arc<dyn Hypervisor>, name: &str) -> Result<()> {
        let attempts = std::cmp::max(
            1,
            (self.start_wait.as_nanos() / self.start_poll.as_nanos()) as usize,
        );
        let mut last = State::Stopped;
        for attempt in 0..attempts {
            last = hyp.state(name).await.map_err(Error::caused)?;
            if last == State::Running {
                return Ok(());
            }
            if attempt + 1 < attempts {
                self.sleep.sleep(self.start_poll).await;
            }
        }
        Err(Error::operation(format!(
            "lifecycle: {name} not running after {:?} (last state {last})",
            self.start_wait
        )))
    }

    async fn sync_observed(&self, hyp: &Arc<dyn Hypervisor>, instance: &Instance) {
        let state = hyp.state(&instance.name).await.unwrap_or(State::Stopped);
        if let Err(error) = self.store.set_observed_state(&instance.uuid, state).await {
            self.log.warn(&format!(
                "observed state not recorded: {error}",
                error = bento_types::error_chain(&*error)
            ));
        }
    }
}

/// Raises the restore flag for the length of one restore, on every way out.
struct RestoringFlag<'a>(&'a AtomicBool);

impl<'a> RestoringFlag<'a> {
    fn raise(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(flag)
    }
}

impl Drop for RestoringFlag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Marks one machine as restoring for the length of one `restore_host`,
/// on every way out. For the controller's own machine it also raises the
/// flag that the local poll reads.
struct HostRestoring<'a> {
    manager: &'a Manager,
    host_id: i64,
}

impl<'a> HostRestoring<'a> {
    fn raise(manager: &'a Manager, host_id: i64) -> Self {
        *manager
            .restoring_hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(host_id)
            .or_default() += 1;
        if host_id == manager.host_id {
            manager.restoring.store(true, Ordering::SeqCst);
        }
        Self { manager, host_id }
    }
}

impl Drop for HostRestoring<'_> {
    fn drop(&mut self) {
        let mut hosts = self
            .manager
            .restoring_hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = hosts.get_mut(&self.host_id) {
            *count -= 1;
            if *count == 0 {
                hosts.remove(&self.host_id);
                if self.host_id == self.manager.host_id {
                    self.manager.restoring.store(false, Ordering::SeqCst);
                }
            }
        }
    }
}
