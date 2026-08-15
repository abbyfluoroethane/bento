use bento_types::{Image, ImageVersion};
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Inserts or updates an allowlist entry (SPEC 5.1). The current
    /// checksum is managed separately so a configuration reload cannot
    /// roll an image back.
    pub async fn upsert_image(&self, image: Image) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO images (name, url, pinned_checksum) VALUES (?, ?, ?) \
                 ON CONFLICT(name) DO UPDATE SET \
                    url = excluded.url, pinned_checksum = excluded.pinned_checksum",
                params![image.name, image.url, image.pinned_checksum],
            )?;
            Ok(())
        })
        .await
    }

    /// Returns one allowlist entry by name.
    pub async fn image(&self, name: impl Into<String>) -> Result<Image> {
        let name = name.into();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT name, url, pinned_checksum, current_checksum FROM images WHERE name = ?",
                [name],
                scan_image,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }

    /// Lists the allowlist for the `images` command (SPEC 15).
    pub async fn images(&self) -> Result<Vec<Image>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT name, url, pinned_checksum, current_checksum FROM images ORDER BY name",
            )?;
            let rows = statement.query_map([], scan_image)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Records a downloaded file at its content-addressed path (SPEC 5.1).
    pub async fn add_image_version(&self, version: ImageVersion) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO image_versions (checksum, image_name, path, size, fetched_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    version.checksum,
                    version.image_name,
                    version.path,
                    version.size,
                    format_time(version.fetched_at)?
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Points an image at a fetched version.
    pub async fn set_current_checksum(
        &self,
        image_name: impl Into<String>,
        checksum: impl Into<String>,
    ) -> Result<()> {
        let image_name = image_name.into();
        let checksum = checksum.into();
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "UPDATE images SET current_checksum = ? WHERE name = ?",
                params![checksum, image_name],
            )?;
            if changed == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Lists fetched versions of one image, newest first.
    pub async fn image_versions(&self, image_name: impl Into<String>) -> Result<Vec<ImageVersion>> {
        let image_name = image_name.into();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT checksum, image_name, path, size, fetched_at FROM image_versions \
                 WHERE image_name = ? ORDER BY fetched_at DESC, checksum",
            )?;
            let rows = statement.query_map([image_name], scan_image_version)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Lists versions that no instance uses as its base and no image uses
    /// as current. These are the versions the image store may delete
    /// (SPEC 5.1).
    pub async fn unused_image_versions(&self) -> Result<Vec<ImageVersion>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT checksum, image_name, path, size, fetched_at FROM image_versions \
                 WHERE checksum NOT IN (SELECT base_checksum FROM instances) \
                 AND checksum NOT IN ( \
                    SELECT current_checksum FROM images WHERE current_checksum IS NOT NULL) \
                 ORDER BY fetched_at, checksum",
            )?;
            let rows = statement.query_map([], scan_image_version)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Removes the record for one fetched file.
    pub async fn delete_image_version(&self, checksum: impl Into<String>) -> Result<()> {
        let checksum = checksum.into();
        self.with_conn(move |conn| {
            if conn.execute("DELETE FROM image_versions WHERE checksum = ?", [checksum])? == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }
}

fn scan_image(row: &rusqlite::Row<'_>) -> rusqlite::Result<Image> {
    Ok(Image {
        name: row.get(0)?,
        url: row.get(1)?,
        pinned_checksum: row.get(2)?,
        current_checksum: row.get(3)?,
    })
}

fn scan_image_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImageVersion> {
    let fetched: String = row.get(4)?;
    Ok(ImageVersion {
        checksum: row.get(0)?,
        image_name: row.get(1)?,
        path: row.get(2)?,
        size: row.get(3)?,
        fetched_at: parse_time(4, &fetched)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bento_types::ImageVersion;
    use time::macros::datetime;

    use crate::tests::{new_test_store, seed_store, test_instance};

    #[tokio::test]
    async fn images_and_unused_versions() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        store
            .add_image_version(ImageVersion {
                checksum: "sha256-old".into(),
                image_name: "debian-13".into(),
                path: "/var/lib/bento/images/sha256-old.qcow2".into(),
                size: 2,
                fetched_at: datetime!(2025-12-01 0:00 UTC),
            })
            .await
            .unwrap();
        store
            .set_current_checksum("debian-13", "sha256-aa")
            .await
            .unwrap();
        assert_eq!(
            store
                .image("debian-13")
                .await
                .unwrap()
                .current_checksum
                .as_deref(),
            Some("sha256-aa")
        );
        let unused = store.unused_image_versions().await.unwrap();
        assert_eq!(unused.len(), 1);
        assert_eq!(unused[0].checksum, "sha256-old");

        let mut instance = test_instance(1, "web", &owner, &host);
        instance.base_checksum = "sha256-old".into();
        store
            .create_instance(instance, Duration::ZERO)
            .await
            .unwrap();
        assert!(store.unused_image_versions().await.unwrap().is_empty());
        let versions = store.image_versions("debian-13").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].checksum, "sha256-aa");
    }
}
