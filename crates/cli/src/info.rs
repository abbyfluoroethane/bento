//! Read and settings commands from SPEC 15: `ls`, `port`, `visibility`,
//! `share`, `images`, `ssh-key`, and `whoami`.

use std::io::Read;

use bento_types::{Instance, Visibility};
use russh::keys::ssh_key::{HashAlg, PublicKey, authorized_keys::Entry};

use crate::parse::{ago, parse_flags};
use crate::{Cli, Env, is_not_found, render_table};

impl Cli {
    pub(crate) async fn ls(&self, env: &mut Env<'_>) -> i32 {
        if !env.args.is_empty() {
            return env.usage("ls");
        }
        // SPEC 15/6.1: ls shows the quota use.
        let quota = match self.quota_line(env.user.id).await {
            Ok(quota) => quota,
            Err(error) => return env.fail(error),
        };
        let _ = writeln!(env.out, "{quota}");

        let mut own = match self.store.instances_by_owner(env.user.id).await {
            Ok(instances) => instances,
            Err(error) => return env.fail(error),
        };
        sort_instances(&mut own);
        let now = (self.options.now)();
        if own.is_empty() {
            let _ = writeln!(env.out, "no instances; create one with: new <name>");
        } else {
            let mut rows = vec![cells(&[
                "NAME",
                "STATE",
                "ADDRESS",
                "IMAGE",
                "VISIBILITY",
                "LAST USE",
            ])];
            rows.extend(own.iter().map(|instance| {
                vec![
                    instance.name.clone(),
                    instance.state.to_string(),
                    instance.address.clone(),
                    instance.image_name.clone(),
                    instance.visibility.to_string(),
                    ago(now, instance.last_seen_at),
                ]
            }));
            render_table(env.out, &rows);
        }

        let mut shared = match self.store.instances_shared_with(env.user.id).await {
            Ok(instances) => instances,
            Err(error) => return env.fail(error),
        };
        if !shared.is_empty() {
            sort_instances(&mut shared);
            let _ = writeln!(env.out, "\nshared with you:");
            let mut rows = vec![cells(&["NAME", "STATE", "ADDRESS", "OWNER", "LAST USE"])];
            for instance in shared {
                let owner = self
                    .store
                    .user_by_id(instance.owner_id)
                    .await
                    .map_or_else(|_| "?".into(), |user| user.name);
                rows.push(vec![
                    instance.name,
                    instance.state.to_string(),
                    instance.address,
                    owner,
                    ago(now, instance.last_seen_at),
                ]);
            }
            render_table(env.out, &rows);
        }
        0
    }

    pub(crate) async fn port(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 2 {
            return env.usage("port <name> <port>");
        }
        let name = env.args[0].clone();
        let port_text = env.args[1].clone();
        let Some(instance) = self.resolve_owned(env, &name).await else {
            return 1;
        };
        let port = match port_text.parse::<i64>() {
            Ok(port @ 1..=65535) => port as u16,
            _ => {
                return env.fail_message(format!(
                    "port: {port_text:?} is not a port between 1 and 65535"
                ));
            }
        };
        // Through the lifecycle: a port change reloads the nftables table
        // (SPEC 6.3), so it must not wait for the convergence tick.
        let name = instance.name.clone();
        if let Err(error) = self.lifecycle.set_http_port(instance, port).await {
            return env.fail(error);
        }
        let _ = writeln!(
            env.out,
            "port: the default HTTP port of {name} is now {port}"
        );
        0
    }

    pub(crate) async fn visibility(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 2 {
            return env.usage("visibility <name> <off|private|public>");
        }
        let name = env.args[0].clone();
        let value = env.args[1].clone();
        let Some(instance) = self.resolve_owned(env, &name).await else {
            return 1;
        };
        let visibility = match value.as_str() {
            "off" => Visibility::Off,
            "private" => Visibility::Private,
            "public" => Visibility::Public,
            _ => return env.usage("visibility <name> <off|private|public>"),
        };
        // Through the lifecycle: published ports follow visibility, and SPEC
        // 6.3 reloads the nftables table on every change.
        let name = instance.name.clone();
        if let Err(error) = self.lifecycle.set_visibility(instance, visibility).await {
            return env.fail(error);
        }
        let url = self.instance_url(&name);
        match visibility {
            Visibility::Off => {
                let _ = writeln!(env.out, "visibility: {name} is now off; {url} returns 404");
            }
            Visibility::Private => {
                let _ = writeln!(
                    env.out,
                    "visibility: {name} is now private; {url} requires a login"
                );
            }
            Visibility::Public => {
                let _ = writeln!(
                    env.out,
                    "visibility: {name} is now public; anyone can reach {url}"
                );
            }
        }
        0
    }

    pub(crate) async fn share(&self, env: &mut Env<'_>) -> i32 {
        let flags = match parse_flags(env.args, &[], &["revoke"]) {
            Ok(flags) => flags,
            Err(error) => {
                let _ = writeln!(env.err, "{error}");
                return 2;
            }
        };
        if !(1..=2).contains(&flags.positionals.len()) {
            return env.usage("share [--revoke] <name> [<user>]");
        }
        let name = flags.positionals[0].clone();
        let Some(instance) = self.resolve_owned(env, &name).await else {
            return 1;
        };
        let revoke = flags.booleans.get("revoke").copied().unwrap_or(false);
        if flags.positionals.len() == 1 {
            if revoke {
                return env.usage("share --revoke <name> <user>");
            }
            return self.list_shares(env, instance).await;
        }
        let target_name = &flags.positionals[1];
        let target = match self.store.user_by_name(target_name).await {
            Ok(user) => user,
            Err(error) if is_not_found(error.as_ref()) => {
                return env.fail_message(format!("share: no such user: {target_name}"));
            }
            Err(error) => return env.fail(error),
        };
        if revoke {
            if let Err(error) = self.store.remove_share(&instance.uuid, target.id).await {
                if is_not_found(error.as_ref()) {
                    return env.fail_message(format!(
                        "share: {} has no share on {}",
                        target.name, instance.name
                    ));
                }
                return env.fail(error);
            }
            let _ = writeln!(
                env.out,
                "share: {} no longer has access to {}",
                target.name, instance.name
            );
            return 0;
        }
        if target.id == env.user.id {
            return env.fail_message(format!("share: you own {} already", instance.name));
        }
        if let Err(error) = self.store.add_share(&instance.uuid, target.id).await {
            return env.fail(error);
        }
        let _ = writeln!(
            env.out,
            "share: {} can now use {}",
            target.name, instance.name
        );
        0
    }

    async fn list_shares(&self, env: &mut Env<'_>, instance: Instance) -> i32 {
        let shares = match self.store.shares_for(&instance.uuid).await {
            Ok(shares) => shares,
            Err(error) => return env.fail(error),
        };
        if shares.is_empty() {
            let _ = writeln!(env.out, "share: {} is shared with nobody", instance.name);
            return 0;
        }
        let mut rows = vec![cells(&["USER", "SINCE"])];
        for share in shares {
            let name = self
                .store
                .user_by_id(share.user_id)
                .await
                .map_or_else(|_| format!("user-{}", share.user_id), |user| user.name);
            rows.push(vec![name, share.created_at.date().to_string()]);
        }
        render_table(env.out, &rows);
        0
    }

    pub(crate) async fn images(&self, env: &mut Env<'_>) -> i32 {
        if !env.args.is_empty() {
            return env.usage("images");
        }
        let mut images = match self.store.images().await {
            Ok(images) => images,
            Err(error) => return env.fail(error),
        };
        let instances = match self.store.instances().await {
            Ok(instances) => instances,
            Err(error) => return env.fail(error),
        };
        images.sort_by(|left, right| left.name.cmp(&right.name));
        // SPEC 5.1: show each image, its current checksum, and how many
        // instances hold an older version.
        let mut rows = vec![cells(&["NAME", "CURRENT CHECKSUM", "ON OLDER VERSIONS"])];
        for image in images {
            let older = instances
                .iter()
                .filter(|instance| {
                    instance.image_name == image.name
                        && Some(instance.base_checksum.as_str())
                            != image.current_checksum.as_deref()
                })
                .count();
            rows.push(vec![
                image.name,
                image
                    .current_checksum
                    .unwrap_or_else(|| "(never fetched)".into()),
                older.to_string(),
            ]);
        }
        render_table(env.out, &rows);
        0
    }

    pub(crate) async fn ssh_key(&self, env: &mut Env<'_>) -> i32 {
        let subcommand = env.args.first().map_or("list", String::as_str);
        match subcommand {
            "list" => self.ssh_key_list(env).await,
            "add" => {
                let args = env.args[1..].to_vec();
                self.ssh_key_add(env, &args).await
            }
            "remove" => {
                if env.args.len() != 2 {
                    return env.usage("ssh-key remove <id>");
                }
                let text = env.args[1].clone();
                let id = match text.parse::<i64>() {
                    Ok(id) => id,
                    Err(_) => {
                        return env.fail_message(format!(
                            "ssh-key: {text:?} is not a key id; find the id with: ssh-key list"
                        ));
                    }
                };
                if let Err(error) = self.store.delete_ssh_key(env.user.id, id).await {
                    if is_not_found(error.as_ref()) {
                        return env.fail_message(format!("ssh-key: you have no key with id {id}"));
                    }
                    return env.fail(error);
                }
                let _ = writeln!(env.out, "ssh-key: removed key {id}");
                0
            }
            _ => env.usage("ssh-key [add <public key>|list|remove <id>]"),
        }
    }

    async fn ssh_key_list(&self, env: &mut Env<'_>) -> i32 {
        let mut keys = match self.store.ssh_keys_for_user(env.user.id).await {
            Ok(keys) => keys,
            Err(error) => return env.fail(error),
        };
        if keys.is_empty() {
            let _ = writeln!(env.out, "no keys; add one with: ssh-key add <public key>");
            return 0;
        }
        keys.sort_by_key(|key| key.id);
        let mut rows = vec![cells(&["ID", "FINGERPRINT", "COMMENT", "ADDED"])];
        rows.extend(keys.into_iter().map(|key| {
            vec![
                key.id.to_string(),
                key.fingerprint,
                key.comment,
                key.created_at.date().to_string(),
            ]
        }));
        render_table(env.out, &rows);
        0
    }

    async fn ssh_key_add(&self, env: &mut Env<'_>, args: &[String]) -> i32 {
        let mut raw = args.join(" ");
        if raw.trim().is_empty() {
            let mut limited = env.input.take(64 * 1024);
            if let Err(error) = limited.read_to_string(&mut raw) {
                return env.fail(Box::new(error));
            }
        }
        let key = match parse_authorized_key(&raw) {
            Some(key) => key,
            None => {
                return env.fail_message("ssh-key: not a public key in authorized_keys format");
            }
        };
        let comment = key.comment().as_str_lossy().to_owned();
        let line = match key.to_openssh() {
            Ok(line) => line,
            Err(_) => {
                return env.fail_message("ssh-key: not a public key in authorized_keys format");
            }
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        if let Err(error) = self
            .store
            .add_ssh_key(env.user.id, line.trim(), &fingerprint, &comment)
            .await
        {
            return env.fail(error);
        }
        let _ = writeln!(env.out, "ssh-key: added {fingerprint}");
        0
    }

    pub(crate) async fn whoami(&self, env: &mut Env<'_>) -> i32 {
        if !env.args.is_empty() {
            return env.usage("whoami");
        }
        let quota = match self.quota_line(env.user.id).await {
            Ok(quota) => quota,
            Err(error) => return env.fail(error),
        };
        render_table(
            env.out,
            &[
                vec!["name".into(), env.user.name.clone()],
                vec!["email".into(), env.user.email.clone()],
                vec!["subnet".into(), env.user.subnet.clone()],
                vec!["quota".into(), quota],
            ],
        );
        0
    }
}

fn sort_instances(instances: &mut [Instance]) {
    instances.sort_by(|left, right| left.name.cmp(&right.name));
}

fn cells(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn parse_authorized_key(input: &str) -> Option<PublicKey> {
    input.lines().find_map(|line| {
        let line = line.split('\r').next().unwrap_or(line).trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        line.parse::<Entry>().ok().map(Into::into)
    })
}
