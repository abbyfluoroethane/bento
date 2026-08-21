use async_trait::async_trait;
use bento_types::Image;

use crate::store::{DynError, Result, dependency};

/// The consumer-side view of the queries the images command needs
/// (SPEC section 15). The real store package implements it.
#[async_trait]
pub trait ReportSource: Send + Sync {
    /// Returns the operator allowlist with current checksums.
    async fn images(&self) -> std::result::Result<Vec<Image>, DynError>;
    /// Returns how many instances rows of the image carry a base_checksum
    /// different from the given checksum.
    async fn count_instances_on_other_versions(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> std::result::Result<i64, DynError>;
}

/// One row of the images command: the image name, its current checksum,
/// and the number of instances that hold an older version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub name: String,
    pub current_checksum: Option<String>,
    pub older_instances: i64,
}

/// Builds the data for the images command, sorted by image name.
pub async fn report<S: ReportSource + ?Sized>(source: &S) -> Result<Vec<Status>> {
    let images = source
        .images()
        .await
        .map_err(|error| dependency("report", error))?;
    let mut statuses = Vec::with_capacity(images.len());
    for image in images {
        let older_instances = if let Some(checksum) = &image.current_checksum {
            source
                .count_instances_on_other_versions(&image.name, checksum)
                .await
                .map_err(|error| dependency(format!("report {}", image.name), error))?
        } else {
            0
        };
        statuses.push(Status {
            name: image.name,
            current_checksum: image.current_checksum,
            older_instances,
        });
    }
    statuses.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;

    use super::*;

    struct FakeReportSource {
        images: std::result::Result<Vec<Image>, &'static str>,
        older: HashMap<String, i64>,
    }

    #[async_trait]
    impl ReportSource for FakeReportSource {
        async fn images(&self) -> std::result::Result<Vec<Image>, DynError> {
            self.images
                .clone()
                .map_err(|message| io::Error::other(message).into())
        }

        async fn count_instances_on_other_versions(
            &self,
            image_name: &str,
            _checksum: &str,
        ) -> std::result::Result<i64, DynError> {
            Ok(*self.older.get(image_name).unwrap_or(&0))
        }
    }

    fn image(name: &str, checksum: Option<&str>) -> Image {
        Image {
            name: name.into(),
            url: String::new(),
            kind: Default::default(),
            pinned_checksum: None,
            current_checksum: checksum.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn report_builds_sorted_statuses() {
        let source = FakeReportSource {
            images: Ok(vec![
                image("ubuntu-24.04", Some("bbb")),
                image("debian-13", Some("aaa")),
                image("never-fetched", None),
            ]),
            older: HashMap::from([("debian-13".into(), 2), ("never-fetched".into(), 9)]),
        };
        assert_eq!(
            report(&source).await.expect("report"),
            vec![
                Status {
                    name: "debian-13".into(),
                    current_checksum: Some("aaa".into()),
                    older_instances: 2,
                },
                Status {
                    name: "never-fetched".into(),
                    current_checksum: None,
                    older_instances: 0,
                },
                Status {
                    name: "ubuntu-24.04".into(),
                    current_checksum: Some("bbb".into()),
                    older_instances: 0,
                },
            ]
        );
    }

    #[tokio::test]
    async fn report_error() {
        let source = FakeReportSource {
            images: Err("db down"),
            older: HashMap::new(),
        };
        assert!(report(&source).await.is_err());
    }
}
