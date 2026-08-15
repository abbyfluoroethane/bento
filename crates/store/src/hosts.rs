use bento_types::Host;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Inserts the host row if absent and returns it. Version 1 runs one
    /// host, but the column and row exist from the start (SPEC 12, 17).
    /// An existing host keeps its id; its URI is updated when changed.
    pub async fn ensure_host(
        &self,
        name: impl Into<String>,
        libvirt_uri: impl Into<String>,
    ) -> Result<Host> {
        let name = name.into();
        let libvirt_uri = libvirt_uri.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            tx.execute(
                "INSERT INTO hosts (name, libvirt_uri, created_at) VALUES (?, ?, ?) \
                 ON CONFLICT(name) DO UPDATE SET libvirt_uri = excluded.libvirt_uri",
                params![name, libvirt_uri, format_time(now())?],
            )?;
            Ok(tx.query_row(
                "SELECT id, name, libvirt_uri, created_at FROM hosts WHERE name = ?",
                [&name],
                scan_host,
            )?)
        })
        .await
    }

    /// Returns one host by id.
    pub async fn host(&self, id: i64) -> Result<Host> {
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, name, libvirt_uri, created_at FROM hosts WHERE id = ?",
                [id],
                scan_host,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }
}

fn scan_host(row: &rusqlite::Row<'_>) -> rusqlite::Result<Host> {
    let created: String = row.get(3)?;
    Ok(Host {
        id: row.get(0)?,
        name: row.get(1)?,
        libvirt_uri: row.get(2)?,
        created_at: parse_time(3, &created)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::tests::new_test_store;

    #[tokio::test]
    async fn ensure_host_idempotent() {
        let store = new_test_store().await;
        let first = store.ensure_host("host1", "qemu:///system").await.unwrap();
        let second = store
            .ensure_host("host1", "qemu+ssh://root@host1/system")
            .await
            .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.libvirt_uri, "qemu+ssh://root@host1/system");
    }
}
