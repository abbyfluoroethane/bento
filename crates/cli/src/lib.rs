//! The Bento command line interface served over the SSH frontend (SPEC
//! section 15).
//!
//! The form is `ssh bento.example.org <command> [arguments]`. Every
//! command receives an already-authenticated user and explicit I/O handles;
//! this crate never reads the local process argument vector.

mod backend;
mod info;
mod instance;
mod parse;

pub use backend::{BoxError, CreateRequest, Lifecycle, ReadWrite, ResizeRequest, Store};

use std::error::Error as StdError;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use bento_store::{Error as StoreError, Usage};
use bento_types::{Instance, Quota, User};
use time::OffsetDateTime;

use parse::format_cooldown;

const FALLBACK_VCPU: u32 = 2;
const FALLBACK_MEMORY_MIB: i64 = 2048;
const FALLBACK_DISK_GIB: i64 = 20;

/// Thread-safe time source used for deterministic last-use rendering.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Configuration for the SSH command line interface.
#[derive(Clone)]
pub struct Options {
    /// The base domain, for example `bento.foid.space`. It appears in help
    /// text, rename confirmations, and visibility messages.
    pub domain: String,
    /// The image `new` uses when `--image` is absent. Empty makes the flag
    /// mandatory.
    pub default_image: String,
    /// Default vCPU count for `new` when its flag is absent.
    pub default_vcpu: u32,
    /// Default memory for `new` when its flag is absent.
    pub default_memory_mib: i64,
    /// Default disk size for `new` when its flag is absent.
    pub default_disk_gib: i64,
    /// The operator cooldown setting (SPEC 7.2), used in messages only; the
    /// store enforces it.
    pub name_cooldown: Duration,
    /// Time source for last-use formatting.
    pub now: Clock,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            domain: String::new(),
            default_image: String::new(),
            default_vcpu: FALLBACK_VCPU,
            default_memory_mib: FALLBACK_MEMORY_MIB,
            default_disk_gib: FALLBACK_DISK_GIB,
            name_cooldown: Duration::from_secs(24 * 60 * 60),
            now: Arc::new(OffsetDateTime::now_utc),
        }
    }
}

impl Options {
    fn with_defaults(mut self) -> Self {
        if self.default_vcpu == 0 {
            self.default_vcpu = FALLBACK_VCPU;
        }
        if self.default_memory_mib == 0 {
            self.default_memory_mib = FALLBACK_MEMORY_MIB;
        }
        if self.default_disk_gib == 0 {
            self.default_disk_gib = FALLBACK_DISK_GIB;
        }
        if self.name_cooldown.is_zero() {
            self.name_cooldown = Duration::from_secs(24 * 60 * 60);
        }
        self
    }
}

/// Executes commands for one authenticated user at a time.
pub struct Cli {
    store: Arc<dyn Store>,
    lifecycle: Arc<dyn Lifecycle>,
    options: Options,
}

impl Cli {
    /// Builds a CLI over the data layer and lifecycle actions.
    pub fn new(store: Arc<dyn Store>, lifecycle: Arc<dyn Lifecycle>, options: Options) -> Self {
        Self {
            store,
            lifecycle,
            options: options.with_defaults(),
        }
    }

    /// Executes one SSH command line and returns `0` on success, `1` on
    /// failure, or `2` on a usage error.
    pub async fn run(
        &self,
        user: User,
        args: &[String],
        stdin: &mut (dyn Read + Send),
        stdout: &mut (dyn Write + Send),
        stderr: &mut (dyn Write + Send),
    ) -> i32 {
        if args.is_empty() || args[0] == "help" {
            self.help(stdout);
            return 0;
        }
        let mut env = Env {
            user,
            args: &args[1..],
            input: stdin,
            out: stdout,
            err: stderr,
        };
        match args[0].as_str() {
            "ls" => self.ls(&mut env).await,
            "new" => self.new_command(&mut env).await,
            "rm" => self.rm(&mut env).await,
            "start" => self.start(&mut env).await,
            "stop" => self.stop(&mut env).await,
            "restart" => self.restart(&mut env).await,
            "rename" => self.rename(&mut env).await,
            "cp" => self.copy(&mut env).await,
            "resize" => self.resize(&mut env).await,
            "console" => self.console(&mut env).await,
            "port" => self.port(&mut env).await,
            "visibility" => self.visibility(&mut env).await,
            "share" => self.share(&mut env).await,
            "images" => self.images(&mut env).await,
            "ssh-key" => self.ssh_key(&mut env).await,
            "whoami" => self.whoami(&mut env).await,
            command => {
                let _ = writeln!(
                    env.err,
                    "bento: unknown command {command:?}; run \"help\" for the command list"
                );
                2
            }
        }
    }

    fn help(&self, writer: &mut dyn Write) {
        let host = if self.options.domain.is_empty() {
            "bento"
        } else {
            &self.options.domain
        };
        let _ = write!(
            writer,
            "bento — usage: ssh {host} <command> [arguments]\n\n\
  ls                                 list your instances\n\
  new <name> [--image --memory --cpu --disk --nested --no-ksm]\n\
                                     create an instance\n\
  rm <name> [--force]                delete an instance\n\
  start <name>                       start a stopped instance\n\
  stop <name>                        stop a running instance\n\
  restart <name>                     restart an instance\n\
  rename <old> <new>                 rename an instance\n\
  cp <source> <target>               copy a stopped instance\n\
  resize <name> [--memory --cpu --disk --nested|--no-nested]\n\
                                     change instance resources\n\
  console <name>                     attach to the serial console\n\
  port <name> <port>                 set the default HTTP port\n\
  visibility <name> <off|private|public>\n\
                                     set who can reach the instance URL\n\
  share [--revoke] <name> [<user>]   grant, revoke, or list access\n\
  images                             list images and versions in use\n\
  ssh-key [add|list|remove]          manage your SSH keys\n\
  whoami                             show your account and quota\n"
        );
    }

    async fn resolve(&self, env: &mut Env<'_>, name: &str) -> Option<Instance> {
        let instance = match self.store.instance_by_name(name).await {
            Ok(instance) => instance,
            Err(_) => {
                env.access_denied(name);
                return None;
            }
        };
        if instance.owner_id == env.user.id {
            return Some(instance);
        }
        match self.store.has_access(&instance.uuid, env.user.id).await {
            Ok(true) => Some(instance),
            Ok(false) => {
                env.access_denied(name);
                None
            }
            Err(error) => {
                env.fail(error);
                None
            }
        }
    }

    async fn resolve_owned(&self, env: &mut Env<'_>, name: &str) -> Option<Instance> {
        let instance = self.resolve(env, name).await?;
        if instance.owner_id != env.user.id {
            let _ = writeln!(
                env.err,
                "bento: only the owner of {name} may run this command"
            );
            return None;
        }
        Some(instance)
    }

    async fn quota_line(&self, user_id: i64) -> Result<String, BoxError> {
        let usage = self.store.usage_for(user_id).await?;
        let quota = match self.store.quota_for(user_id).await {
            Ok(quota) => Some(quota),
            Err(error) if is_not_found(error.as_ref()) => None,
            Err(error) => return Err(error),
        };
        Ok(render_quota(usage, quota))
    }

    fn ssh_host(&self) -> &str {
        if self.options.domain.is_empty() {
            "bento"
        } else {
            &self.options.domain
        }
    }

    fn instance_url(&self, name: &str) -> String {
        if self.options.domain.is_empty() {
            format!("the URL of {name}")
        } else {
            format!("https://{name}.{}/", self.options.domain)
        }
    }
}

struct Env<'a> {
    user: User,
    args: &'a [String],
    input: &'a mut (dyn Read + Send),
    out: &'a mut (dyn Write + Send),
    err: &'a mut (dyn Write + Send),
}

impl Env<'_> {
    fn usage(&mut self, message: &str) -> i32 {
        let _ = writeln!(self.err, "usage: {message}");
        2
    }

    fn fail(&mut self, error: BoxError) -> i32 {
        if let Some(error) = error.downcast_ref::<StoreError>() {
            match error {
                StoreError::NameCooldown { name, remaining } => {
                    let _ = writeln!(
                        self.err,
                        "bento: the name {name:?} was released by another user and is in cooldown; try again in {}",
                        format_cooldown(*remaining)
                    );
                    return 1;
                }
                StoreError::Quota {
                    limit,
                    used,
                    requested,
                    max,
                } => {
                    let _ = writeln!(
                        self.err,
                        "bento: quota exceeded: the {limit} limit is {max}, {used} in use, {requested} requested"
                    );
                    return 1;
                }
                StoreError::NameTaken => {
                    let _ = writeln!(
                        self.err,
                        "bento: that name is taken by an existing instance"
                    );
                    return 1;
                }
                _ => {}
            }
        }
        let _ = writeln!(self.err, "bento: {error}");
        1
    }

    fn fail_message(&mut self, message: impl Into<String>) -> i32 {
        self.fail(Box::new(MessageError(message.into())))
    }

    fn access_denied(&mut self, name: &str) -> i32 {
        let _ = writeln!(self.err, "bento: no such instance or no access: {name}");
        1
    }
}

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for MessageError {}

fn is_not_found(error: &(dyn StdError + 'static)) -> bool {
    matches!(
        error.downcast_ref::<StoreError>(),
        Some(StoreError::NotFound)
    )
}

fn render_quota(usage: Usage, quota: Option<Quota>) -> String {
    let limit = |value: Option<i64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
    format!(
        "instances {}/{} · vcpu {}/{} · memory {}/{} MiB · disk {}/{} GiB",
        usage.instances,
        limit(quota.map(|q| q.max_instances)),
        usage.vcpu,
        limit(quota.map(|q| q.max_vcpu)),
        usage.memory_mib,
        limit(quota.map(|q| q.max_memory_mib)),
        usage.disk_gib,
        limit(quota.map(|q| q.max_disk_gib))
    )
}

fn confirm(input: &mut dyn Read, out: &mut dyn Write, prompt: &str) -> bool {
    let _ = write!(out, "{prompt}");
    let mut line = String::new();
    let _ = BufReader::new(input).read_line(&mut line);
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn render_table(writer: &mut dyn Write, rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let _ = write!(writer, "{cell}");
            if index + 1 < row.len() {
                let padding = widths[index] - cell.chars().count() + 2;
                let _ = write!(writer, "{}", " ".repeat(padding));
            }
        }
        let _ = writeln!(writer);
    }
}

#[cfg(test)]
mod tests;
