use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{DynError, Error, Result, Ruleset};

/// Applies a complete nftables ruleset atomically. The real
/// implementation executes `nft`; tests use a fake.
#[async_trait]
pub trait Applier: Send + Sync {
    async fn apply_ruleset(&self, ruleset: &str) -> std::result::Result<(), DynError>;
}

/// Applies a ruleset by feeding it to `nft -f -` on stdin. The ruleset
/// text itself carries the delete-and-redefine of the Bento table, and
/// nft applies a file as one transaction, so the reload is atomic.
#[derive(Debug, Clone, Default)]
pub struct NftApplier {
    /// Overrides the nft binary path. An empty string means `nft` from
    /// `PATH`.
    pub path: String,
}

#[derive(Debug, thiserror::Error)]
#[error("network: nft -f -: {cause}: {output}")]
struct NftApplyError {
    cause: String,
    output: String,
}

#[async_trait]
impl Applier for NftApplier {
    async fn apply_ruleset(&self, ruleset: &str) -> std::result::Result<(), DynError> {
        let path = if self.path.is_empty() {
            "nft"
        } else {
            &self.path
        };
        let mut child = Command::new(path)
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                Box::new(NftApplyError {
                    cause: error.to_string(),
                    output: String::new(),
                }) as DynError
            })?;

        let mut stdin = child.stdin.take().expect("piped stdin is present");
        let write_result = async {
            stdin.write_all(ruleset.as_bytes()).await?;
            stdin.shutdown().await
        }
        .await;
        drop(stdin);

        let output = child.wait_with_output().await.map_err(|error| {
            Box::new(NftApplyError {
                cause: error.to_string(),
                output: String::new(),
            }) as DynError
        })?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let combined = combined.trim().to_string();

        if let Err(error) = write_result {
            return Err(Box::new(NftApplyError {
                cause: error.to_string(),
                output: combined,
            }));
        }
        if !output.status.success() {
            return Err(Box::new(NftApplyError {
                cause: output.status.to_string(),
                output: combined,
            }));
        }
        Ok(())
    }
}

/// Renders the ruleset and applies it as one atomic full-table reload
/// (SPEC 6.3). Call this on every change to network policy. A partial
/// rule update leaves a window with the wrong policy.
pub async fn reload<A: Applier + ?Sized>(applier: &A, ruleset: &Ruleset) -> Result<()> {
    let text = ruleset.render()?;
    applier.apply_ruleset(&text).await.map_err(Error::Apply)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::nftables::tests::two_user_ruleset;

    #[derive(Default)]
    struct FakeApplier {
        applied: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl Applier for FakeApplier {
        async fn apply_ruleset(&self, ruleset: &str) -> std::result::Result<(), DynError> {
            self.applied.lock().unwrap().push(ruleset.to_string());
            if self.fail {
                Err(std::io::Error::other("nft exploded").into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn reload_ruleset() {
        let fake = FakeApplier::default();
        reload(&fake, &two_user_ruleset()).await.unwrap();
        {
            let applied = fake.applied.lock().unwrap();
            assert_eq!(applied.len(), 1);
            assert_eq!(applied[0], two_user_ruleset().render().unwrap());
        }

        // A render error must not reach the applier.
        let mut bad = two_user_ruleset();
        bad.users[0].network.bridge = "no good".to_string();
        let fake = FakeApplier::default();
        assert!(reload(&fake, &bad).await.is_err());
        assert!(fake.applied.lock().unwrap().is_empty());

        // An applier error is returned.
        let fake = FakeApplier {
            applied: Mutex::new(Vec::new()),
            fail: true,
        };
        assert!(reload(&fake, &two_user_ruleset()).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nft_applier_exec() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let ok = directory.path().join("nft-ok");
        std::fs::write(&ok, "#!/bin/sh\ncat > \"$0.stdin\"\n").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let applier = NftApplier {
            path: ok.to_string_lossy().into_owned(),
        };
        applier
            .apply_ruleset("table inet bento {\n}\n")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(format!("{}.stdin", ok.display())).unwrap(),
            "table inet bento {\n}\n"
        );

        let fail = directory.path().join("nft-fail");
        std::fs::write(&fail, "#!/bin/sh\necho 'syntax error' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&fail, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = NftApplier {
            path: fail.to_string_lossy().into_owned(),
        }
        .apply_ruleset("bogus")
        .await
        .unwrap_err();
        assert!(error.to_string().contains("syntax error"), "{error}");
    }
}
