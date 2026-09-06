//! Operator commands from SPEC 15 and MULTI-NODE 20.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use bento_types::{Image, ImageKind};
use time::macros::format_description;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::adapters::ImageReport;
use crate::runners::ImageSync;
use crate::setup::{App, shutdown_signal};

const IMAGE_SYNC_LEASE_TTL: Duration = Duration::from_secs(31 * 60);

pub(crate) async fn sync_image_allowlist(app: &App) -> Result<()> {
    for image in &app.cfg.images {
        app.store
            .upsert_image(Image {
                name: image.name.clone(),
                url: if image.oci.is_empty() {
                    image.url.clone()
                } else {
                    image.oci.clone()
                },
                kind: if image.oci.is_empty() {
                    ImageKind::Qcow2
                } else {
                    ImageKind::Oci
                },
                pinned_checksum: image.pinned_checksum.clone(),
                current_checksum: None,
            })
            .await
            .map_err(|error| anyhow::anyhow!("image allowlist {}: {error}", image.name))?;
    }
    Ok(())
}

/// Downloads, verifies, and stores each allowlisted image, then collects
/// unreferenced versions (SPEC 5.1).
pub(crate) async fn run_fetch_images(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = async {
        sync_image_allowlist(&app).await?;
        let images = app.image_store();
        tokio::select! {
            result = images.fetch_images() => Ok(result?),
            () = shutdown_signal() => Ok(()),
        }
    }
    .await;
    app.close().await;
    result
}

/// Pulls what each allowlist source serves now on every enabled runner.
pub(crate) async fn run_sync_images(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = sync_images(&app).await;
    app.close().await;
    result
}

async fn sync_images(app: &App) -> Result<()> {
    sync_image_allowlist(app).await?;
    for runner in &app.cfg.runners {
        // The configuration validated this address at startup, so the
        // only reachable failure here would be a code change that let an
        // unvalidated entry through (MULTI-NODE 8.5).
        let underlay = runner
            .underlay_address()
            .map_err(|error| anyhow::anyhow!("runner {:?}: {error}", runner.name))?;
        app.store
            .register_runner(&runner.name, &runner.endpoint, underlay.to_string())
            .await
            .with_context(|| format!("register runner {:?}", runner.name))?;
    }

    let holder_id = bento_lifecycle::random_uuid();
    let lease = app
        .store
        .acquire_lease(holder_id.clone(), IMAGE_SYNC_LEASE_TTL)
        .await
        .context("acquire controller lease for image sync (MULTI-NODE 11.3)")?;
    let (_, lease_receiver) = watch::channel(lease);
    let result = tokio::select! {
        result = sync_images_with_lease(app, lease_receiver) => result,
        () = shutdown_signal() => Ok(()),
    };
    let release = app.store.release_lease(holder_id).await;
    result?;
    release.context("release controller lease after image sync (MULTI-NODE 11.3)")?;
    Ok(())
}

async fn sync_images_with_lease(
    app: &App,
    lease: watch::Receiver<bento_types::Lease>,
) -> Result<()> {
    let hosts = app
        .store
        .hosts()
        .await?
        .into_iter()
        .filter(|host| host.enabled)
        .collect::<Vec<_>>();
    let images = app.store.images().await?;
    let sync = ImageSync::new(app.store.clone(), lease);
    let mut tasks = JoinSet::new();
    for image in &images {
        for host in &hosts {
            let sync = sync.clone();
            let image = image.clone();
            let host = host.clone();
            tasks.spawn(async move {
                let image_name = image.name.clone();
                let host_name = host.name.clone();
                // `None` makes the runner fetch the trusted source again.
                // This is the operator's update action (MULTI-NODE 13.2).
                let result = sync.ensure(&host, &image, None).await;
                (image_name, host_name, result)
            });
        }
    }

    let mut reports = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut failures = Vec::new();
    while let Some(task) = tasks.join_next().await {
        match task {
            Ok((image_name, host_name, Ok(report))) => {
                reports
                    .entry(image_name)
                    .or_default()
                    .insert(host_name, report.checksum);
            }
            Ok((image_name, host_name, Err(error))) => {
                failures.push(format!("{image_name} on {host_name}: {error}"));
            }
            Err(error) => failures.push(format!("image sync task: {error}")),
        }
    }

    for image in &images {
        println!("{}", image.name);
        let versions = reports.get(&image.name);
        for host in &hosts {
            let checksum = versions
                .and_then(|rows| rows.get(&host.name))
                .map(String::as_str)
                .unwrap_or("(not ready)");
            println!("  {}  {checksum}", host.name);
        }
        let distinct = versions
            .into_iter()
            .flat_map(|rows| rows.values())
            .collect::<BTreeSet<_>>();
        let converged = !hosts.is_empty()
            && versions.is_some_and(|rows| rows.len() == hosts.len())
            && distinct.len() == 1;
        println!(
            "  fleet converged: {}",
            if converged { "yes" } else { "no" }
        );
    }

    if !failures.is_empty() {
        anyhow::bail!("image sync failed: {}", failures.join("; "));
    }
    Ok(())
}

/// Lists images, current checksums, and stale instance counts (SPEC 15).
pub(crate) async fn run_images(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = async {
        sync_image_allowlist(&app).await?;
        let statuses = bento_images::report(&ImageReport(app.store.clone())).await?;
        let rows = statuses
            .into_iter()
            .map(|status| {
                (
                    status.name,
                    status
                        .current_checksum
                        .unwrap_or_else(|| "(not fetched; run bentod fetch-images)".to_owned()),
                    status.older_instances.to_string(),
                )
            })
            .collect::<Vec<_>>();
        let image_width = rows
            .iter()
            .map(|row| row.0.len())
            .max()
            .unwrap_or(0)
            .max("IMAGE".len());
        let checksum_width = rows
            .iter()
            .map(|row| row.1.len())
            .max()
            .unwrap_or(0)
            .max("CURRENT CHECKSUM".len());
        println!(
            "{:<image_width$}  {:<checksum_width$}  OLDER INSTANCES",
            "IMAGE", "CURRENT CHECKSUM"
        );
        for (name, checksum, older) in rows {
            println!("{name:<image_width$}  {checksum:<checksum_width$}  {older}");
        }
        Ok(())
    }
    .await;
    app.close().await;
    result
}

/// Reports disagreement between libvirt and the database (SPEC 6.1). It
/// changes nothing; an operator corrects the discrepancy by hand.
pub(crate) async fn run_reconcile(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = async {
        let hypervisor = app.connect_libvirt().await?;
        // Reconcile must compare local libvirt with only this host's rows
        // (MULTI-NODE 21).
        let machine_id = bento_hostinfo::read_machine_id()
            .map_err(|error| anyhow::anyhow!("machine identity: {error}"))?;
        let host = app
            .store
            .ensure_host(
                machine_id,
                bento_hostinfo::read_hostname(),
                &app.cfg.libvirt_uri,
            )
            .await
            .map_err(|error| anyhow::anyhow!("hosts row: {error}"))?;
        let manager = app.manager(hypervisor.clone(), host.id)?;
        let report = tokio::select! {
            report = manager.reconcile() => report?,
            () = shutdown_signal() => return Ok(()),
        };
        if report.is_empty() {
            println!("libvirt and the database agree");
        }
        if !report.domains_without_rows.is_empty() {
            println!("domains without a database row:");
            for domain in report.domains_without_rows {
                println!("  {} ({}, {})", domain.name, domain.uuid, domain.state);
            }
        }
        if !report.rows_without_domains.is_empty() {
            println!("database rows without a libvirt domain:");
            for instance in report.rows_without_domains {
                println!(
                    "  {} ({}, desired {})",
                    instance.name, instance.uuid, instance.desired_state
                );
            }
        }
        hypervisor.close().await?;
        Ok(())
    }
    .await;
    app.close().await;
    result
}

/// Writes a consistent database copy with SQLite's backup API (SPEC 12.1).
/// A direct file copy of a WAL database is unsafe.
pub(crate) async fn run_dump_db(config: &Path, args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let destination = args.first().map_or_else(
        || {
            let stamp = time::OffsetDateTime::now_utc()
                .format(format_description!(
                    "[year][month][day]-[hour][minute][second]"
                ))
                .expect("fixed UTC timestamp format");
            PathBuf::from(format!("bento-{stamp}.db"))
        },
        PathBuf::from,
    );
    let result = app.store.dump_db(&destination).await.map_err(Into::into);
    if result.is_ok() {
        println!(
            "wrote a consistent copy of {} to {}",
            app.cfg.db_path,
            destination.display()
        );
    }
    app.close().await;
    result
}

pub(crate) async fn run_restore_db(config: &Path, args: &[OsString]) -> Result<()> {
    let Some(source) = args.first().map(PathBuf::from) else {
        anyhow::bail!("restore-db needs the database to restore from");
    };
    let app = App::new(config).await?;
    let result = restore(&app, &source).await;
    app.close().await;
    result
}

/// Copies the live database aside, then replaces it (MULTI-NODE 16).
///
/// The safety copy is the point. A restore is the one operator command
/// that throws data away, and an operator who restores the wrong file has
/// no way back unless the command made one first. It is written before
/// anything is replaced, and its name says what it is.
async fn restore(app: &App, source: &Path) -> Result<()> {
    let stamp = time::OffsetDateTime::now_utc()
        .format(format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .expect("fixed UTC timestamp format");
    let aside = PathBuf::from(format!("{}.before-restore-{stamp}", app.cfg.db_path));
    app.store.dump_db(&aside).await?;
    println!("copied {} to {}", app.cfg.db_path, aside.display());

    app.store.restore_db(source).await?;
    println!("restored {} from {}", app.cfg.db_path, source.display());
    println!("stop the bentod units before a restore and start them after it");
    Ok(())
}

/// Shows the runner slots, and changes the division or an owner
/// (MULTI-NODE 7 and 17).
///
/// Subdivision alone moves nothing: each old slot splits into children
/// that stay with the old owner, and every existing address keeps its
/// value and its `/24` guest configuration (MULTI-NODE 17.1). Giving a
/// child slot to another machine is the separate step, and this command
/// refuses it while the slot still holds an instance, because moving a
/// slot that is in use is the fenced workflow of section 17 and not this.
pub(crate) async fn run_slots(config: &Path, args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = slots_inner(&app, args).await;
    app.close().await;
    result
}

async fn slots_inner(app: &App, args: &[OsString]) -> Result<()> {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    match args.split_first() {
        None => show_slots(app).await,
        Some((verb, rest)) if verb == "show" && rest.is_empty() => show_slots(app).await,
        Some((verb, rest)) if verb == "set-prefix" => {
            let [prefix] = rest else {
                anyhow::bail!("usage: bentod slots set-prefix <24|25|26|27>");
            };
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| anyhow::anyhow!("{prefix:?} is not a slot prefix"))?;
            set_prefix(app, prefix).await
        }
        Some((verb, rest)) if verb == "plan" => {
            let machine = rest.first().map(String::as_str);
            show_plan(app, machine).await
        }
        Some((verb, rest)) if verb == "give" => {
            let [slot, machine] = rest else {
                anyhow::bail!("usage: bentod slots give <slot> <machine>");
            };
            let slot: i64 = slot
                .parse()
                .map_err(|_| anyhow::anyhow!("{slot:?} is not a slot number"))?;
            give_slot(app, slot, machine).await
        }
        Some((verb, _)) => {
            anyhow::bail!("unknown slots command {verb:?}; try show, set-prefix, or give")
        }
    }
}

async fn show_slots(app: &App) -> Result<()> {
    let deployment = app.store.deployment().await?;
    let hosts = app.store.hosts().await?;
    let instances = app.store.instances().await?;
    println!(
        "runner prefix /{} ({} slot(s) per user /24)",
        deployment.runner_prefix,
        deployment.slot_count()
    );
    println!("{:<6}  {:<10}  {:<20}  INSTANCES", "SLOT", "STATE", "OWNER");
    for slot in app.store.slots().await? {
        let owner = hosts
            .iter()
            .find(|host| host.id == slot.owner_host_id)
            .map_or("(unknown)".to_owned(), |host| host.name.clone());
        let count = instances
            .iter()
            .filter(|instance| instance.slot == Some(slot.slot))
            .count();
        println!(
            "{:<6}  {:<10}  {:<20}  {count}",
            slot.slot,
            slot.state.as_str(),
            owner
        );
    }
    let unplaced = instances.iter().filter(|i| i.slot.is_none()).count();
    if unplaced > 0 {
        println!("\n{unplaced} instance(s) hold no slot; run `bentod reconcile`.");
    }
    Ok(())
}

/// Prints the network one machine should have, without applying it.
///
/// The routes and the firewall are what the controller would send that
/// machine on its next poll. Reading them before a change is the way to
/// see what a prefix change or a slot move will do, because the same
/// description is what the machine renders from (MULTI-NODE 8).
async fn show_plan(app: &App, machine: Option<&str>) -> Result<()> {
    let hosts = app.store.hosts().await?;
    let controller = bento_hostinfo::read_machine_id()?;
    let here = hosts
        .iter()
        .find(|host| host.machine_id.as_deref() == Some(controller.as_str()))
        .ok_or_else(|| anyhow::anyhow!("this machine has no host row; run serve once"))?;
    let target = match machine {
        Some(name) => hosts
            .iter()
            .find(|host| host.name == name)
            .ok_or_else(|| anyhow::anyhow!("no machine named {name:?}"))?,
        None => here,
    };

    let high_ports = bento_network::PortRange {
        from: i32::from(app.cfg.listen.proxy_port_min),
        to: i32::from(app.cfg.listen.proxy_port_max),
    };
    let network =
        crate::netstate::machine_network(&app.store, app.plan, high_ports, target.id, here.id)
            .await?;
    match network.check() {
        Ok(()) => {}
        Err(error) => println!("this plan would be refused: {error}\n"),
    }

    println!("machine {} (host {})", target.name, target.id);
    println!(
        "underlay {}",
        target.underlay.as_deref().unwrap_or("(none)")
    );
    println!("\nroutes:");
    let routes = network.routes()?;
    if routes.is_empty() {
        println!("  (none; this machine owns every slot that has an owner)");
    }
    for route in &routes {
        println!(
            "  ip route replace {}/{} via {}",
            route.destination.addr, route.destination.bits, route.next_hop
        );
    }
    println!("\nproxy ARP and forwarding:");
    for bridge in network.proxy_arp_bridges()? {
        println!("  net.ipv4.conf.{bridge}.proxy_arp = 1");
    }
    println!("\nnftables:");
    for line in network.ruleset()?.render()?.lines() {
        println!("  {line}");
    }
    Ok(())
}

async fn set_prefix(app: &App, prefix: u8) -> Result<()> {
    let before = app.store.deployment().await?;
    if before.runner_prefix == prefix {
        println!("the runner prefix is already /{prefix}");
        return Ok(());
    }
    if prefix < before.runner_prefix {
        // Merging needs one child evacuated first so both children share
        // an owner (MULTI-NODE 17.2). That workflow does not exist yet,
        // and doing it wrong leaves two machines owning one prefix.
        anyhow::bail!(
            "narrowing /{} to /{prefix} merges slots, which is not implemented \
             (MULTI-NODE 17.2)",
            before.runner_prefix
        );
    }
    let after = app.store.set_runner_prefix(prefix).await?;
    let slots = app.store.slots().await?;
    println!(
        "runner prefix /{} -> /{}; {} slot(s) per user /24",
        before.runner_prefix,
        after.runner_prefix,
        after.slot_count()
    );
    println!(
        "{} slot(s) claimed; the rest are unowned until `bentod slots give`",
        slots.len()
    );
    // Subdivision keeps every guest address and every guest's /24
    // configuration (MULTI-NODE 17.1), so nothing needs restarting.
    println!("no instance moved and no address changed.");
    Ok(())
}

async fn give_slot(app: &App, slot: i64, machine: &str) -> Result<()> {
    let host = app
        .store
        .hosts()
        .await?
        .into_iter()
        .find(|host| host.name == machine)
        .ok_or_else(|| anyhow::anyhow!("no machine named {machine:?}"))?;
    if host.underlay.is_none() {
        // Without an address no other machine can route to this slot, and
        // the guests placed in it would be unreachable (MULTI-NODE 8.5).
        anyhow::bail!(
            "machine {machine:?} has no underlay address; add one to its \
             [[runners]] entry before giving it a slot"
        );
    }
    let claimed = app.store.claim_slot(slot, host.id).await?;
    println!(
        "slot {} -> {} (ownership epoch {})",
        claimed.slot, host.name, claimed.ownership_epoch
    );
    println!("the controller installs the routes on its next poll.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn write_test_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for child in ["images", "storage"] {
            std::fs::create_dir(dir.path().join(child)).unwrap();
        }
        let path = dir.path().join("bento.toml");
        std::fs::write(
            &path,
            format!(
                "base_domain = \"bento.example.org\"\n\
                 db_path = {:?}\nimage_dir = {:?}\nstorage_dir = {:?}\nkey_dir = {:?}\n\
                 [[images]]\nname = \"debian-13\"\nurl = \"https://example.test/debian-13.qcow2\"\n",
                dir.path().join("bento.db"),
                dir.path().join("images"),
                dir.path().join("storage"),
                dir.path().join("keys")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn dump_db_command() {
        let (dir, config) = write_test_config();
        let destination = dir.path().join("backup.db");
        run_dump_db(&config, &[destination.clone().into_os_string()])
            .await
            .unwrap();
        assert!(std::fs::metadata(&destination).unwrap().len() > 0);
        assert!(
            run_dump_db(&config, &[destination.into_os_string()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn restore_db_command() {
        let (_dir, config) = write_test_config();
        let output_directory = tempfile::tempdir().unwrap();
        let backup = output_directory.path().join("backup.db");
        run_dump_db(&config, &[backup.clone().into_os_string()])
            .await
            .unwrap();

        run_restore_db(&config, &[backup.into_os_string()])
            .await
            .unwrap();
        // The copy the restore took of what it replaced.
        let db = std::fs::read_to_string(&config)
            .unwrap()
            .lines()
            .find_map(|line| {
                line.split_once('=')
                    .filter(|(key, _)| key.trim() == "db_path")
                    .map(|(_, value)| value.trim().trim_matches('\"').to_owned())
            })
            .expect("db_path");
        let parent = Path::new(&db).parent().unwrap();
        let aside = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".before-restore-")
            });
        assert!(aside, "a restore keeps a copy of what it replaced");

        assert!(run_restore_db(&config, &[]).await.is_err());
    }

    #[tokio::test]
    async fn images_command() {
        let (_dir, config) = write_test_config();
        run_images(&config, &[]).await.unwrap();
    }
}
