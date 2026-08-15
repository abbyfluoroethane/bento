use bento_hypervisor::Error as HypervisorError;
use bento_types::State;

use crate::{Error, Manager, Result};

impl Manager {
    /// Changes an instance label without moving its UUID-derived disk or seed.
    /// The stopped domain is undefined under the old name and redefined under
    /// the new one; the old name enters cooldown with no alias (SPEC 7.2, 7.3).
    pub async fn rename(&self, uuid: &str, new_name: &str) -> Result<()> {
        let mut instance = self
            .store
            .instance(uuid)
            .await
            .map_err(crate::actions::external)?;
        if new_name == instance.name {
            return Ok(());
        }
        let old_name = instance.name.clone();
        let domain_gone = match self.hyp.state(&old_name).await {
            Ok(State::Stopped) => false,
            Ok(state) => return Err(Error::RenameNeedsStop(format!("{old_name} is {state}"))),
            Err(HypervisorError::DomainNotFound(_)) => true,
            Err(error) => {
                return Err(Error::operation(format!(
                    "lifecycle: rename {old_name}: {error}"
                )));
            }
        };
        if !domain_gone && self.definer.is_none() {
            return Err(Error::operation(format!(
                "lifecycle: rename {old_name}: hypervisor cannot redefine domains"
            )));
        }
        self.store
            .rename_instance(uuid, new_name, self.cooldown)
            .await
            .map_err(crate::actions::external)?;
        if domain_gone {
            return Ok(());
        }
        instance.name = new_name.to_string();
        let with_iso = (self.iso_exists)(&self.seed_iso_path(uuid));
        let xml = match self.domain_xml_by_uuid(&instance, with_iso).await {
            Ok(xml) => xml,
            Err(error) => return Err(self.unwind_rename(&instance, &old_name, error).await),
        };
        match self.hyp.remove(&old_name).await {
            Ok(()) | Err(HypervisorError::DomainNotFound(_)) => {}
            Err(error) => {
                return Err(self
                    .unwind_rename(
                        &instance,
                        &old_name,
                        Error::operation(format!("undefine {old_name}: {error}")),
                    )
                    .await);
            }
        }
        let definer = self.definer.as_ref().expect("capability checked");
        if let Err(error) = definer.define(&xml).await {
            let mut old = instance.clone();
            old.name.clone_from(&old_name);
            if let Ok(old_xml) = self.domain_xml_by_uuid(&old, with_iso).await
                && let Err(restore_error) = definer.define(&old_xml).await
            {
                self.log.error(&format!(
                    "rename: domain lost; redefine it by hand: {restore_error}"
                ));
            }
            return Err(self
                .unwind_rename(
                    &instance,
                    &old_name,
                    Error::operation(format!("define {new_name}: {error}")),
                )
                .await);
        }
        Ok(())
    }

    async fn unwind_rename(
        &self,
        instance: &bento_types::Instance,
        old_name: &str,
        cause: Error,
    ) -> Error {
        if let Err(error) = self
            .store
            .rename_instance(&instance.uuid, old_name, self.cooldown)
            .await
        {
            self.log
                .error(&format!("rename: revert to old name failed: {error}"));
        }
        Error::operation(format!(
            "lifecycle: rename {old_name} to {}: {cause}",
            instance.name
        ))
    }
}
