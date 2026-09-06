use bento_types::Host;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Inserts the host row if absent and returns it (SPEC 12, 17).
    ///
    /// The machine ID is the key and the name is a label, so a machine
    /// that is renamed keeps its row and its instances (MULTI-NODE 16).
    /// An existing machine keeps its id; its name and URI follow what the
    /// caller reports.
    pub async fn ensure_host(
        &self,
        machine_id: impl Into<String>,
        name: impl Into<String>,
        libvirt_uri: impl Into<String>,
    ) -> Result<Host> {
        let machine_id = machine_id.into();
        let name = name.into();
        let libvirt_uri = libvirt_uri.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            let known = tx.execute(
                "UPDATE hosts SET name = ?, libvirt_uri = ? WHERE machine_id = ?",
                params![name, libvirt_uri, machine_id],
            )?;
            if known == 0 {
                // A database from version 1 carries one host row that no
                // machine has claimed, because version 1 stored no
                // machine ID. That row is this machine: version 1 ran one
                // host, and this is the process that ran it. Claiming the
                // row keeps its instances; a new row would strand them.
                //
                // The row is claimed only when it is the one row in the
                // table. A runner named in the configuration also has no
                // machine ID until its own runner service reports one
                // (MULTI-NODE 19), and this
                // machine must never claim the row of another machine.
                let unclaimed: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM hosts WHERE machine_id IS NULL AND \
                         (SELECT COUNT(*) FROM hosts) = 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                match unclaimed {
                    Some(id) => tx.execute(
                        "UPDATE hosts SET machine_id = ?, name = ?, libvirt_uri = ? WHERE id = ?",
                        params![machine_id, name, libvirt_uri, id],
                    )?,
                    None => tx.execute(
                        "INSERT INTO hosts (machine_id, name, libvirt_uri, created_at) \
                         VALUES (?, ?, ?, ?)",
                        params![machine_id, name, libvirt_uri, format_time(now())?],
                    )?,
                };
            }
            let host: Host = tx.query_row(
                &format!(
                    "SELECT {} FROM hosts WHERE machine_id = ?",
                    crate::runners::HOST_COLUMNS
                ),
                [&machine_id],
                scan_host,
            )?;

            // A deployment with no slot at all is a new one, or one that
            // predates slots. Every address belongs to slot 0 at the
            // default `/24`, and this machine is the only one that can be
            // running them, so it takes slot 0 (MULTI-NODE 7.2).
            //
            // Without this a fresh deployment could never place an
            // instance: allocation asks which slot the machine owns, and
            // the answer would be none. The guard is that any slot row at
            // all stops it, so a second machine never claims a slot this
            // way. Giving a slot to another machine is `claim_slot`.
            let slots: i64 =
                tx.query_row("SELECT COUNT(*) FROM runner_slots", [], |row| row.get(0))?;
            if slots == 0 {
                tx.execute(
                    "INSERT INTO runner_slots (slot, state, owner_host_id) \
                     VALUES (0, 'active', ?)",
                    params![host.id],
                )?;
            }
            Ok(host)
        })
        .await
    }

    /// Returns one host by id.
    pub async fn host(&self, id: i64) -> Result<Host> {
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {} FROM hosts WHERE id = ?",
                    crate::runners::HOST_COLUMNS
                ),
                [id],
                scan_host,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }
}

pub(crate) fn scan_host(row: &rusqlite::Row<'_>) -> rusqlite::Result<Host> {
    let placement: String = row.get(7)?;
    let created: String = row.get(8)?;
    Ok(Host {
        id: row.get(0)?,
        machine_id: row.get(1)?,
        name: row.get(2)?,
        libvirt_uri: row.get(3)?,
        endpoint: row.get(4)?,
        underlay: row.get(5)?,
        enabled: row.get(6)?,
        placement: crate::instances::parse_enum(7, &placement)?,
        created_at: parse_time(8, &created)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::tests::new_test_store;

    const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
    const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";

    #[tokio::test]
    async fn ensure_host_idempotent() {
        let store = new_test_store().await;
        let first = store
            .ensure_host(KONATA, "host1", "qemu:///system")
            .await
            .unwrap();
        let second = store
            .ensure_host(KONATA, "host1", "qemu+ssh://root@host1/system")
            .await
            .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.libvirt_uri, "qemu+ssh://root@host1/system");
    }

    #[tokio::test]
    async fn a_renamed_machine_keeps_its_row() {
        // The bug this replaced: the transient kernel hostname drifted,
        // and every drift minted a second row for the one machine.
        let store = new_test_store().await;
        let before = store
            .ensure_host(KONATA, "localhost.localdomain", "qemu:///system")
            .await
            .unwrap();
        let after = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.name, "konata");
    }

    #[tokio::test]
    async fn a_second_machine_gets_its_own_row() {
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let tsukasa = store
            .ensure_host(TSUKASA, "tsukasa", "qemu+ssh://root@tsukasa/system")
            .await
            .unwrap();
        assert_ne!(tsukasa.id, konata.id);
        assert_eq!(store.host(konata.id).await.unwrap().name, "konata");
    }

    #[tokio::test]
    async fn the_first_machine_claims_the_row_a_version_one_database_left() {
        let store = new_test_store().await;
        let id = store
            .with_tx(|tx| {
                tx.execute(
                    "INSERT INTO hosts (name, libvirt_uri, created_at) VALUES (?, ?, ?)",
                    rusqlite::params!["linux", "qemu:///system", "1970-01-01T00:00:00Z"],
                )?;
                Ok(tx.last_insert_rowid())
            })
            .await
            .unwrap();
        let claimed = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        assert_eq!(claimed.id, id, "a new row would strand the instances");
        assert_eq!(claimed.machine_id.as_deref(), Some(KONATA));

        // Only the first machine claims it. The next one is its own host.
        let tsukasa = store
            .ensure_host(TSUKASA, "tsukasa", "qemu:///system")
            .await
            .unwrap();
        assert_ne!(tsukasa.id, id);
    }

    #[tokio::test]
    async fn a_machine_never_claims_the_row_of_another_machine() {
        // A runner named in the configuration has a row and no machine ID
        // until its runner service reports one (MULTI-NODE 19). The
        // controller must make its
        // own row, not take the runner's.
        let store = new_test_store().await;
        store
            .ensure_host(TSUKASA, "tsukasa", "qemu+ssh://root@tsukasa/system")
            .await
            .unwrap();
        let pending = store
            .with_tx(|tx| {
                tx.execute(
                    "INSERT INTO hosts (name, libvirt_uri, created_at) VALUES (?, ?, ?)",
                    rusqlite::params!["runner-b", "qemu:///system", "1970-01-01T00:00:00Z"],
                )?;
                Ok(tx.last_insert_rowid())
            })
            .await
            .unwrap();

        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        assert_ne!(konata.id, pending, "konata took the row of runner-b");
        assert_eq!(
            store.host(pending).await.unwrap().machine_id,
            None,
            "the pending runner has not reported its machine yet"
        );
    }
}
