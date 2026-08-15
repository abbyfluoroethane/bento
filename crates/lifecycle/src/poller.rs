use std::collections::HashMap;
use std::future::Future;

use bento_types::{Instance, State};

use crate::{Error, Manager, Result};

impl Manager {
    /// Runs one observed-state poll (SPEC 12), updates all rows together,
    /// then finishes first boot for running instances whose seed remains.
    pub async fn poll_once(&self) -> Result<()> {
        let domains =
            self.hyp.list().await.map_err(|error| {
                Error::operation(format!("lifecycle: poll: list domains: {error}"))
            })?;
        let states: HashMap<_, _> = domains
            .into_iter()
            .map(|domain| (domain.uuid, domain.state))
            .collect();
        self.store
            .update_observed_states(states.clone())
            .await
            .map_err(|error| {
                Error::operation(format!("lifecycle: poll: record observed states: {error}"))
            })?;
        let instances = self.store.instances().await.map_err(|error| {
            Error::operation(format!("lifecycle: poll: list instances: {error}"))
        })?;
        for instance in instances {
            if states.get(&instance.uuid) != Some(&State::Running) {
                continue;
            }
            if let Err(error) = self.finish_first_boot(&instance).await {
                self.log.warn(&format!(
                    "poll: first boot cleanup failed; retry next poll: {error}"
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
                        self.log.warn(&format!("poll failed: {error}"));
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
                    "event: no row for {uuid}, ignoring {state}: {error}"
                ));
                return Ok(());
            }
        };
        self.store
            .set_observed_state(uuid, state)
            .await
            .map_err(crate::actions::external)?;
        if state == State::Running
            && let Err(error) = self.finish_first_boot(&instance).await
        {
            self.log.warn(&format!(
                "event: first boot cleanup failed; retry next poll: {error}"
            ));
        }
        Ok(())
    }

    /// Detaches and deletes the owner's public-key seed after first successful
    /// boot (SPEC 5.2). Redefinition affects the persistent domain; the live
    /// device remains until stop. Without the optional capability, the file is
    /// still deleted and a warning identifies the stale definition.
    pub async fn finish_first_boot(&self, instance: &Instance) -> Result<()> {
        let path = self.seed_iso_path(&instance.uuid);
        if !(self.iso_exists)(&path) {
            return Ok(());
        }
        if let Some(definer) = &self.definer {
            let xml = self.domain_xml_by_uuid(instance, false).await?;
            definer.define(&xml).await.map_err(|error| {
                Error::operation(format!(
                    "lifecycle: detach seed iso of {}: {error}",
                    instance.name
                ))
            })?;
        } else {
            self.log.warn("first boot: hypervisor cannot redefine; CD-ROM stays in definition until next redefine");
        }
        (self.delete_iso)(path).await.map_err(|error| {
            Error::operation(format!(
                "lifecycle: delete seed iso of {}: {error}",
                instance.name
            ))
        })?;
        self.log.info(&format!(
            "first boot complete: seed ISO removed for {}",
            instance.name
        ));
        Ok(())
    }
}
