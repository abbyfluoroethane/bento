use std::collections::HashMap;

use bento_types::{Instance, State};

use crate::{Error, Manager, Result};

impl Manager {
    /// Restores each instance to its last desired state after host reboot
    /// (SPEC 11.2). Starts are batched and every batch reaches running or
    /// times out before the next, avoiding a simultaneous memory/disk spike.
    pub async fn restore(&self) -> Result<()> {
        let domains = self.hyp.list().await.map_err(|error| {
            Error::operation(format!("lifecycle: restore: list domains: {error}"))
        })?;
        if let Some(clearer) = &self.autostart_clearer {
            for domain in &domains {
                if let Err(error) = clearer.clear_autostart(&domain.name).await {
                    self.log.warn(&format!(
                        "restore: autostart not cleared for {}: {error}",
                        domain.name
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
        self.store
            .update_observed_states(states)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: restore: record observed states: {error}"
                ))
            })?;
        let instances = self
            .store
            .instances_to_restore()
            .await
            .map_err(|error| Error::operation(format!("lifecycle: restore: {error}")))?;
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
            for instance in batch {
                if let Err(error) = self
                    .store
                    .set_observed_state(&instance.uuid, State::Starting)
                    .await
                {
                    self.log
                        .warn(&format!("restore: starting state not recorded: {error}"));
                }
                if let Err(error) = self.hyp.start(&instance.name).await {
                    self.log.warn(&format!(
                        "restore: {} did not start: {error}",
                        instance.name
                    ));
                    self.sync_observed(instance).await;
                } else {
                    started.push(instance);
                }
            }
            for instance in started {
                if let Err(error) = self.wait_running(&instance.name).await {
                    self.log.warn(&format!(
                        "restore: {} did not reach running: {error}",
                        instance.name
                    ));
                    self.sync_observed(instance).await;
                } else if let Err(error) = self
                    .store
                    .set_observed_state(&instance.uuid, State::Running)
                    .await
                {
                    self.log
                        .warn(&format!("restore: running state not recorded: {error}"));
                }
            }
            self.log.info(&format!("restore: batch {} done", index + 1));
        }
        self.log.info("restore: complete");
        Ok(())
    }

    async fn wait_running(&self, name: &str) -> Result<()> {
        let attempts = std::cmp::max(
            1,
            (self.start_wait.as_nanos() / self.start_poll.as_nanos()) as usize,
        );
        let mut last = State::Stopped;
        for attempt in 0..attempts {
            last = self.hyp.state(name).await.map_err(Error::caused)?;
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

    async fn sync_observed(&self, instance: &Instance) {
        let state = self
            .hyp
            .state(&instance.name)
            .await
            .unwrap_or(State::Stopped);
        if let Err(error) = self.store.set_observed_state(&instance.uuid, state).await {
            self.log
                .warn(&format!("observed state not recorded: {error}"));
        }
    }
}
