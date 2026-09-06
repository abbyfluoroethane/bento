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

/// Names the other nftables tables that also filter the forward hook.
///
/// nftables runs every base chain at a hook. An `accept` in one chain
/// only ends that chain; the packet still meets the next one, and a
/// `drop` or `reject` anywhere is final. So Bento's table cannot make a
/// packet pass that another table rejects.
///
/// This matters only once guest traffic crosses machines
/// (MULTI-NODE 8.4). A guest packet then arrives on the underlay
/// interface and has to be forwarded onto a user bridge, and a
/// host firewall that rejects it produces the same signature as a
/// missing route: the guest sees nothing, and every Bento rule looks
/// right. Naming the table turns that into a message an operator can
/// act on.
pub async fn foreign_forward_filters(path: &str) -> Result<Vec<String>> {
    let path = if path.is_empty() { "nft" } else { path };
    let output = Command::new(path)
        .args(["-j", "list", "chains"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            Error::Apply(Box::new(NftApplyError {
                cause: error.to_string(),
                output: String::new(),
            }))
        })?;
    if !output.status.success() {
        return Err(Error::Apply(Box::new(NftApplyError {
            cause: output.status.to_string(),
            output: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })));
    }
    Ok(parse_forward_filters(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Reads `nft -j list chains` and returns the `family table chain` of
/// every forward-hook base chain outside Bento's own table.
fn parse_forward_filters(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(items) = value.get("nftables").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for item in items {
        let Some(chain) = item.get("chain") else {
            continue;
        };
        if chain.get("hook").and_then(|v| v.as_str()) != Some("forward") {
            continue;
        }
        let table = chain.get("table").and_then(|v| v.as_str()).unwrap_or("");
        if table == BENTO_TABLE {
            continue;
        }
        let family = chain.get("family").and_then(|v| v.as_str()).unwrap_or("");
        let name = chain.get("name").and_then(|v| v.as_str()).unwrap_or("");
        found.push(format!("{family} {table} {name}"));
    }
    found.sort();
    found.dedup();
    found
}

/// The one table Bento owns (SPEC 6.3).
const BENTO_TABLE: &str = "bento";

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

    #[test]
    fn foreign_forward_chains_are_named_and_bentos_own_is_not() {
        // Taken from a Fedora machine running firewalld and libvirt.
        let json = r#"{"nftables":[
          {"metainfo":{"version":"1.1.1"}},
          {"chain":{"family":"ip","table":"libvirt_network","name":"forward",
                    "handle":1,"type":"filter","hook":"forward","prio":0,"policy":"accept"}},
          {"chain":{"family":"ip","table":"filter","name":"FORWARD",
                    "handle":2,"type":"filter","hook":"forward","prio":0,"policy":"accept"}},
          {"chain":{"family":"inet","table":"bento","name":"forward",
                    "handle":3,"type":"filter","hook":"forward","prio":0,"policy":"drop"}},
          {"chain":{"family":"inet","table":"bento","name":"output",
                    "handle":4,"type":"filter","hook":"output","prio":0,"policy":"accept"}},
          {"chain":{"family":"inet","table":"firewalld","name":"filter_FORWARD",
                    "handle":5,"type":"filter","hook":"forward","prio":10,"policy":"accept"}}
        ]}"#;
        assert_eq!(
            parse_forward_filters(json),
            [
                "inet firewalld filter_FORWARD",
                "ip filter FORWARD",
                "ip libvirt_network forward",
            ],
            "Bento's own forward chain must not be reported, and other tables must be"
        );
    }

    #[test]
    fn unreadable_chain_output_names_nothing_rather_than_failing() {
        // The check is advice. It must never stop a machine applying its
        // network just because the listing could not be read.
        for text in ["", "not json", "{}", r#"{"nftables":{}}"#] {
            assert!(parse_forward_filters(text).is_empty(), "{text:?}");
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
