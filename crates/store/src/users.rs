use std::collections::HashSet;
use std::net::Ipv4Addr;

use bento_config::Ipv4Prefix;
use bento_types::User;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Store, format_time, parse_time};

/// What one user has provisioned (SPEC 6.1). There is no per-user
/// limit to compare it against; the dashboard shows it against the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub instances: i64,
    pub vcpu: i64,
    pub memory_mib: i64,
    pub disk_gib: i64,
}

impl Store {
    /// Creates a user and allocates the lowest free `/24` from
    /// `private_range` (SPEC 6.2, 13). The scan and insert share one
    /// transaction, and the unique index on `users.subnet` backs the
    /// allocation. An absent OIDC subject is stored as `NULL`.
    pub async fn register_user(
        &self,
        name: impl Into<String>,
        email: impl Into<String>,
        oidc_subject: Option<String>,
        private_range: Ipv4Prefix,
    ) -> Result<User> {
        self.register(
            name.into(),
            email.into(),
            oidc_subject,
            private_range,
            false,
        )
        .await
    }

    /// Registers a user under `preferred`, or under the first free
    /// `preferred-2`, `preferred-3`, ... when that name is taken.
    ///
    /// OIDC provisioning needs this (SPEC 13): the account name is derived
    /// from claims the provider chose, so two identities can perfectly well
    /// arrive wanting the same one, and there is no human in the loop to ask.
    /// The search shares the insert's transaction, which is `BEGIN
    /// IMMEDIATE`, so the name cannot be taken between the check and the
    /// write.
    pub async fn register_user_with_available_name(
        &self,
        preferred: impl Into<String>,
        email: impl Into<String>,
        oidc_subject: Option<String>,
        private_range: Ipv4Prefix,
    ) -> Result<User> {
        self.register(
            preferred.into(),
            email.into(),
            oidc_subject,
            private_range,
            true,
        )
        .await
    }

    async fn register(
        &self,
        name: String,
        email: String,
        oidc_subject: Option<String>,
        private_range: Ipv4Prefix,
        dedupe_name: bool,
    ) -> Result<User> {
        let now = self.clock();
        self.with_tx(move |tx| {
            let name = if dedupe_name {
                available_name(tx, &name)?
            } else {
                name
            };
            let mut statement = tx.prepare("SELECT subnet FROM users")?;
            let subnet_rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut used = HashSet::new();
            for subnet in subnet_rows {
                let subnet = subnet?;
                let (address, bits) = parse_prefix_text(&subnet).map_err(|message| {
                    Error::InvalidPrivateRange(format!("user subnet {subnet:?}: {message}"))
                })?;
                used.insert(masked_address(address, bits));
            }
            drop(statement);

            let subnet = next_free_subnet(private_range, &used)?;
            let created_at = now().to_offset(time::UtcOffset::UTC);
            tx.execute(
                "INSERT INTO users (name, email, oidc_subject, subnet, created_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params![name, email, oidc_subject, subnet, format_time(created_at)?],
            )?;
            Ok(User {
                id: tx.last_insert_rowid(),
                name,
                email,
                oidc_subject,
                subnet,
                created_at,
            })
        })
        .await
    }

    /// Returns one user by primary key.
    pub async fn user_by_id(&self, id: i64) -> Result<User> {
        self.user_by("id", id).await
    }

    /// Returns one user by account name.
    pub async fn user_by_name(&self, name: impl Into<String>) -> Result<User> {
        self.user_by("name", name.into()).await
    }

    /// Returns one user by OIDC subject (SPEC 13).
    pub async fn user_by_oidc_subject(&self, subject: impl Into<String>) -> Result<User> {
        self.user_by("oidc_subject", subject.into()).await
    }

    async fn user_by<V>(&self, column: &'static str, value: V) -> Result<User>
    where
        V: rusqlite::ToSql + Send + 'static,
    {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT id, name, email, oidc_subject, subnet, created_at  \
                 FROM users WHERE {column} = ?"
            );
            conn.query_row(&sql, [value], scan_user)
                .optional()?
                .ok_or(Error::NotFound)
        })
        .await
    }

    /// Returns every user ordered by name. The control plane walks this
    /// list at startup to re-ensure per-user networks and build the
    /// firewall ruleset (SPEC 6.2, 6.3).
    pub async fn users(&self) -> Result<Vec<User>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, name, email, oidc_subject, subnet, created_at  \
                 FROM users ORDER BY name",
            )?;
            let rows = statement.query_map([], scan_user)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Sums what one user has provisioned (SPEC 6.1).
    pub async fn usage_for(&self, user_id: i64) -> Result<Usage> {
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(vcpu), 0), \
                 COALESCE(SUM(memory), 0), COALESCE(SUM(disk), 0) \
                 FROM instances WHERE owner_id = ?",
                [user_id],
                |row| {
                    Ok(Usage {
                        instances: row.get(0)?,
                        vcpu: row.get(1)?,
                        memory_mib: row.get(2)?,
                        disk_gib: row.get(3)?,
                    })
                },
            )?)
        })
        .await
    }
}

fn scan_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    let created: String = row.get(5)?;
    Ok(User {
        id: row.get(0)?,
        name: row.get(1)?,
        email: row.get(2)?,
        oidc_subject: row.get(3)?,
        subnet: row.get(4)?,
        created_at: parse_time(5, &created)?,
    })
}

/// The number of `name-N` variants tried before giving up. A deployment
/// with a hundred identities all claiming one name has a naming problem
/// the store cannot solve.
const NAME_VARIANTS: u32 = 100;

fn available_name(tx: &rusqlite::Transaction<'_>, preferred: &str) -> Result<String> {
    let mut taken = tx.prepare("SELECT 1 FROM users WHERE name = ?")?;
    for suffix in 1..=NAME_VARIANTS {
        let candidate = if suffix == 1 {
            preferred.to_owned()
        } else {
            format!("{preferred}-{suffix}")
        };
        if taken
            .query_row([&candidate], |_| Ok(()))
            .optional()?
            .is_none()
        {
            return Ok(candidate);
        }
    }
    Err(Error::NameTaken)
}

fn next_free_subnet(range: Ipv4Prefix, used: &HashSet<Ipv4Addr>) -> Result<String> {
    if range.bits > 24 {
        return Err(Error::InvalidPrivateRange(format!(
            "{}/{} is narrower than /24",
            range.addr, range.bits
        )));
    }
    let base = u32::from(masked_address(range.addr, range.bits));
    let count = 1_u32 << (24 - range.bits);
    for index in 0..count {
        let address = Ipv4Addr::from(base + (index << 8));
        if !used.contains(&address) {
            return Ok(format!("{address}/24"));
        }
    }
    Err(Error::SubnetsExhausted)
}

fn parse_prefix_text(value: &str) -> std::result::Result<(Ipv4Addr, u8), String> {
    let (address, bits) = value
        .split_once('/')
        .ok_or_else(|| "not a CIDR range".to_string())?;
    let address = address
        .parse::<Ipv4Addr>()
        .map_err(|_| "not an IPv4 address".to_string())?;
    let bits = bits
        .parse::<u8>()
        .map_err(|_| "not a prefix length".to_string())?;
    if bits > 32 {
        return Err("prefix length exceeds /32".to_string());
    }
    Ok((address, bits))
}

fn masked_address(address: Ipv4Addr, bits: u8) -> Ipv4Addr {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    Ipv4Addr::from(u32::from(address) & mask)
}

#[cfg(test)]
mod tests {
    use bento_config::Ipv4Prefix;

    use super::Usage;
    use crate::Error;
    use crate::tests::{new_test_store, seed_store, test_instance, test_range};

    #[tokio::test]
    async fn register_user_allocates_sequential_subnets() {
        let store = new_test_store().await;
        let expected = ["10.100.0.0/24", "10.100.1.0/24", "10.100.2.0/24"];
        for (index, name) in ["alice", "bob", "carol"].into_iter().enumerate() {
            let user = store
                .register_user(name, format!("{name}@example.org"), None, test_range())
                .await
                .unwrap();
            assert_eq!(user.subnet, expected[index]);
        }
    }

    #[tokio::test]
    async fn register_user_reuses_freed_subnet() {
        let store = new_test_store().await;
        for name in ["alice", "bob", "carol"] {
            store
                .register_user(name, format!("{name}@example.org"), None, test_range())
                .await
                .unwrap();
        }
        store
            .with_conn(|conn| {
                conn.execute("DELETE FROM users WHERE name = 'bob'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let user = store
            .register_user("dave", "dave@example.org", None, test_range())
            .await
            .unwrap();
        assert_eq!(user.subnet, "10.100.1.0/24");
    }

    #[tokio::test]
    async fn register_user_exhausts_range() {
        let store = new_test_store().await;
        let tiny = Ipv4Prefix {
            addr: "10.200.0.0".parse().unwrap(),
            bits: 23,
        };
        for name in ["alice", "bob"] {
            store
                .register_user(name, format!("{name}@example.org"), None, tiny)
                .await
                .unwrap();
        }
        let error = store
            .register_user("carol", "carol@example.org", None, tiny)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::SubnetsExhausted));
    }

    #[tokio::test]
    async fn register_user_with_available_name_suffixes_a_taken_name() {
        let store = new_test_store().await;
        let mut names = Vec::new();
        for _ in 0..3 {
            let user = store
                .register_user_with_available_name("riley", "riley@example.org", None, test_range())
                .await
                .unwrap();
            names.push(user.name);
        }
        assert_eq!(names, ["riley", "riley-2", "riley-3"]);
        // The plain form still refuses to collide rather than renaming.
        assert!(
            store
                .register_user("riley", "riley@example.org", None, test_range())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn user_lookups() {
        let store = new_test_store().await;
        let created = store
            .register_user(
                "alice",
                "alice@example.org",
                Some("oidc-alice".into()),
                test_range(),
            )
            .await
            .unwrap();
        let users = [
            store.user_by_id(created.id).await.unwrap(),
            store.user_by_name("alice").await.unwrap(),
            store.user_by_oidc_subject("oidc-alice").await.unwrap(),
        ];
        for user in users {
            assert_eq!(user, created);
        }
        assert!(matches!(
            store.user_by_name("nobody").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn usage_sums_what_one_user_has_provisioned() {
        let store = new_test_store().await;
        let (user, host) = seed_store(&store).await;

        for index in 0..2 {
            let mut instance = test_instance(
                index,
                &format!("web{}", char::from(b'a' + index as u8)),
                &user,
                &host,
            );
            instance.vcpu = 2;
            instance.memory_mib = 1024;
            instance.disk_gib = 20;
            store
                .create_instance(
                    instance,
                    std::time::Duration::ZERO,
                    bento_types::Capacity::unbounded(),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store.usage_for(user.id).await.unwrap(),
            Usage {
                instances: 2,
                vcpu: 4,
                memory_mib: 2048,
                disk_gib: 40
            }
        );
    }

    #[tokio::test]
    async fn users_lists_all_ordered_by_name() {
        let store = new_test_store().await;
        for name in ["carol", "alice", "bob"] {
            store
                .register_user(
                    name,
                    format!("{name}@example.org"),
                    Some(format!("sub-{name}")),
                    test_range(),
                )
                .await
                .unwrap();
        }
        let users = store.users().await.unwrap();
        assert_eq!(
            users
                .iter()
                .map(|user| user.name.as_str())
                .collect::<Vec<_>>(),
            ["alice", "bob", "carol"]
        );
        for user in users {
            assert!(!user.subnet.is_empty());
            assert_eq!(
                user.oidc_subject.as_deref(),
                Some(format!("sub-{}", user.name).as_str())
            );
        }
    }
}
