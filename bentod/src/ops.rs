//! Operator commands from SPEC 15: fetch-images, reconcile, dump-db, and
//! images.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bento_types::{Image, ImageKind};
use time::macros::format_description;

use crate::adapters::ImageReport;
use crate::setup::{App, shutdown_signal};

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
        let manager = app.manager(hypervisor.clone())?;
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
    async fn images_command() {
        let (_dir, config) = write_test_config();
        run_images(&config, &[]).await.unwrap();
    }
}
