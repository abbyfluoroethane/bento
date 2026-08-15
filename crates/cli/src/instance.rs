//! Lifecycle commands from SPEC 15: `new`, `rm`, `start`, `stop`, `restart`,
//! `rename`, `cp`, `resize`, and `console`.

use std::io::{Read, Write};

use bento_hypervisor::StopResult;
use bento_types::State;

use crate::parse::{parse_disk_gib, parse_flags, parse_memory_mib, validate_name};
use crate::{Cli, CreateRequest, Env, ResizeRequest, confirm};

impl Cli {
    pub(crate) async fn new_command(&self, env: &mut Env<'_>) -> i32 {
        let flags = match parse_flags(
            env.args,
            &["image", "memory", "cpu", "disk"],
            &["nested", "no-ksm"],
        ) {
            Ok(flags) => flags,
            Err(error) => return flag_error(env, error),
        };
        if flags.positionals.len() != 1 {
            return env.usage("new <name> [--image --memory --cpu --disk --nested --no-ksm]");
        }
        let name = &flags.positionals[0];
        if let Err(error) = validate_name(name) {
            return env.fail_message(error);
        }
        let image = flags
            .values
            .get("image")
            .cloned()
            .unwrap_or_else(|| self.options.default_image.clone());
        if image.is_empty() {
            return env.usage("new: --image is required (no default image is configured)");
        }
        let cpu = match flags.values.get("cpu") {
            Some(value) => match value.parse::<i64>() {
                Ok(value) => value,
                Err(_) => return flag_error(env, format!("invalid value {value:?} for flag -cpu")),
            },
            None => i64::from(self.options.default_vcpu),
        };
        if cpu < 1 {
            return env.fail_message("--cpu must be at least 1");
        }
        let Ok(vcpu) = u32::try_from(cpu) else {
            return flag_error(env, format!("invalid value {cpu:?} for flag -cpu"));
        };
        let memory_mib = match flags.values.get("memory") {
            Some(value) => match parse_memory_mib(value) {
                Ok(value) => value,
                Err(error) => return env.fail_message(error),
            },
            None => self.options.default_memory_mib,
        };
        let disk_gib = match flags.values.get("disk") {
            Some(value) => match parse_disk_gib(value) {
                Ok(value) => value,
                Err(error) => return env.fail_message(error),
            },
            None => self.options.default_disk_gib,
        };
        let request = CreateRequest {
            owner_id: env.user.id,
            name: name.clone(),
            image,
            vcpu,
            memory_mib,
            disk_gib,
            nested: flags.booleans.get("nested").copied().unwrap_or(false),
            ksm: !flags.booleans.get("no-ksm").copied().unwrap_or(false),
        };
        let instance = match self.lifecycle.create(request).await {
            Ok(instance) => instance,
            Err(error) => return env.fail(error),
        };
        let _ = writeln!(
            env.out,
            "created {}: image {}, {} vCPU, {} MiB memory, {} GiB disk",
            instance.name,
            instance.image_name,
            instance.vcpu,
            instance.memory_mib,
            instance.disk_gib
        );
        let _ = writeln!(env.out, "address {}", instance.address);
        if !self.options.domain.is_empty() {
            let _ = writeln!(
                env.out,
                "connect with: ssh {}@{}",
                instance.name, self.options.domain
            );
        }
        0
    }

    pub(crate) async fn rm(&self, env: &mut Env<'_>) -> i32 {
        let flags = match parse_flags(env.args, &[], &["force"]) {
            Ok(flags) => flags,
            Err(error) => return flag_error(env, error),
        };
        if flags.positionals.len() != 1 {
            return env.usage("rm <name> [--force]");
        }
        let name = flags.positionals[0].clone();
        let Some(instance) = self.resolve_owned(env, &name).await else {
            return 1;
        };
        // SPEC 11.1: rm asks for confirmation; --force is for scripts. The
        // prompt names the instance (SPEC 14.4).
        if !flags.booleans.get("force").copied().unwrap_or(false)
            && !confirm(
                env.input,
                env.out,
                &format!(
                    "delete instance {:?}? this destroys its disk. [y/N] ",
                    instance.name
                ),
            )
        {
            let _ = writeln!(env.err, "rm: aborted");
            return 1;
        }
        let name = instance.name.clone();
        if let Err(error) = self.lifecycle.remove(instance).await {
            return env.fail(error);
        }
        let _ = writeln!(env.out, "rm: deleted {name}");
        0
    }

    pub(crate) async fn start(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 1 {
            return env.usage("start <name>");
        }
        let name = env.args[0].clone();
        let Some(instance) = self.resolve(env, &name).await else {
            return 1;
        };
        if instance.state == State::Running {
            let _ = writeln!(env.out, "start: {} is already running", instance.name);
            return 0;
        }
        let name = instance.name.clone();
        if let Err(error) = self.lifecycle.start(instance).await {
            return env.fail(error);
        }
        let _ = writeln!(env.out, "start: {name} is starting");
        0
    }

    pub(crate) async fn stop(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 1 {
            return env.usage("stop <name>");
        }
        let name = env.args[0].clone();
        let Some(instance) = self.resolve(env, &name).await else {
            return 1;
        };
        let name = instance.name.clone();
        let result = match self.lifecycle.stop(instance).await {
            Ok(result) => result,
            Err(error) => return env.fail(error),
        };
        // SPEC 11.1: report which path the stop took.
        match result {
            StopResult::Graceful => {
                let _ = writeln!(env.out, "stop: {name} shut down after the ACPI request");
            }
            StopResult::Forced => {
                let _ = writeln!(
                    env.out,
                    "stop: {name} ignored the ACPI request for 60s and was forced off"
                );
            }
            StopResult::AlreadyStopped => {
                let _ = writeln!(env.out, "stop: {name} was already stopped");
            }
        }
        0
    }

    pub(crate) async fn restart(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 1 {
            return env.usage("restart <name>");
        }
        let name = env.args[0].clone();
        let Some(instance) = self.resolve(env, &name).await else {
            return 1;
        };
        let name = instance.name.clone();
        if let Err(error) = self.lifecycle.restart(instance).await {
            return env.fail(error);
        }
        let _ = writeln!(env.out, "restart: {name} is restarting");
        0
    }

    pub(crate) async fn rename(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 2 {
            return env.usage("rename <old> <new>");
        }
        let old_name = env.args[0].clone();
        let new_name = env.args[1].clone();
        let Some(instance) = self.resolve_owned(env, &old_name).await else {
            return 1;
        };
        if let Err(error) = validate_name(&new_name) {
            return env.fail_message(error);
        }
        // SPEC 7.3: confirm when visibility is public, stating two facts: the
        // old URL stops working (no redirect), and the SSH user name changes.
        if instance.visibility == bento_types::Visibility::Public {
            let prompt = format!(
                "rename: {old_name:?} is public. Two things change:\n  1. every existing link to {} stops working; there is no redirect\n  2. the SSH user name changes: ssh {old_name}@{} becomes ssh {new_name}@{}\nrename {old_name:?} to {new_name:?}? [y/N] ",
                self.instance_url(&old_name),
                self.ssh_host(),
                self.ssh_host()
            );
            if !confirm(env.input, env.out, &prompt) {
                let _ = writeln!(env.err, "rename: aborted");
                return 1;
            }
        }
        if let Err(error) = self.lifecycle.rename(instance, &new_name).await {
            return env.fail(error);
        }
        let _ = writeln!(
            env.out,
            "rename: {old_name} is now {new_name}; the name {old_name:?} enters a {} cooldown",
            crate::parse::format_cooldown(self.options.name_cooldown)
        );
        0
    }

    pub(crate) async fn copy(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 2 {
            return env.usage("cp <source> <target>");
        }
        let source_name = env.args[0].clone();
        let target = env.args[1].clone();
        let Some(source) = self.resolve(env, &source_name).await else {
            return 1;
        };
        // SPEC 15: cp copies a stopped instance.
        if source.state != State::Stopped {
            return env.fail_message(format!(
                "cp: the source {:?} must be stopped, its state is {}",
                source.name, source.state
            ));
        }
        if let Err(error) = validate_name(&target) {
            return env.fail_message(error);
        }
        let request = CreateRequest {
            owner_id: env.user.id,
            name: target,
            image: source.image_name.clone(),
            vcpu: source.vcpu,
            memory_mib: source.memory_mib,
            disk_gib: source.disk_gib,
            nested: source.nested,
            ksm: source.ksm,
        };
        let source_output_name = source.name.clone();
        let instance = match self.lifecycle.copy(source, request).await {
            Ok(instance) => instance,
            Err(error) => return env.fail(error),
        };
        let _ = writeln!(
            env.out,
            "cp: created {} from {source_output_name}, address {}",
            instance.name, instance.address
        );
        0
    }

    pub(crate) async fn resize(&self, env: &mut Env<'_>) -> i32 {
        let flags = match parse_flags(
            env.args,
            &["memory", "cpu", "disk"],
            &["nested", "no-nested"],
        ) {
            Ok(flags) => flags,
            Err(error) => return flag_error(env, error),
        };
        if flags.positionals.len() != 1 {
            return env.usage("resize <name> [--memory --cpu --disk --nested|--no-nested]");
        }
        let nested = flags.booleans.get("nested").copied().unwrap_or(false);
        let no_nested = flags.booleans.get("no-nested").copied().unwrap_or(false);
        if nested && no_nested {
            return env.usage("resize: --nested and --no-nested exclude each other");
        }
        let name = flags.positionals[0].clone();
        let Some(instance) = self.resolve_owned(env, &name).await else {
            return 1;
        };
        let mut request = ResizeRequest::default();
        if let Some(value) = flags.values.get("memory") {
            request.memory_mib = match parse_memory_mib(value) {
                Ok(value) => Some(value),
                Err(error) => return env.fail_message(error),
            };
        }
        if let Some(value) = flags.values.get("cpu") {
            let cpu = match value.parse::<i64>() {
                Ok(value) => value,
                Err(_) => return flag_error(env, format!("invalid value {value:?} for flag -cpu")),
            };
            if cpu < 1 {
                return env.fail_message("--cpu must be at least 1");
            }
            request.vcpu = match u32::try_from(cpu) {
                Ok(value) => Some(value),
                Err(_) => return flag_error(env, format!("invalid value {value:?} for flag -cpu")),
            };
        }
        if let Some(value) = flags.values.get("disk") {
            let disk = match parse_disk_gib(value) {
                Ok(value) => value,
                Err(error) => return env.fail_message(error),
            };
            if disk < instance.disk_gib {
                return env.fail_message(format!(
                    "resize: the disk of {:?} is {} GiB and can only grow",
                    instance.name, instance.disk_gib
                ));
            }
            request.disk_gib = Some(disk);
        }
        if nested || no_nested {
            request.nested = Some(nested);
        }
        if request == ResizeRequest::default() {
            return env.usage(
                "resize: name at least one of --memory, --cpu, --disk, --nested, --no-nested",
            );
        }
        // SPEC 11.1: tell the user before the change that a restart is needed.
        let name = instance.name.clone();
        let _ = writeln!(
            env.out,
            "resize: the change takes effect after a restart of {name}"
        );
        if let Err(error) = self.lifecycle.resize(instance, request).await {
            return env.fail(error);
        }
        let _ = writeln!(env.out, "resize: {name} updated; run: restart {name}");
        0
    }

    pub(crate) async fn console(&self, env: &mut Env<'_>) -> i32 {
        if env.args.len() != 1 {
            return env.usage("console <name>");
        }
        let name = env.args[0].clone();
        let Some(instance) = self.resolve(env, &name).await else {
            return 1;
        };
        let _ = writeln!(env.out, "console: attached to {}", instance.name);
        let mut stream = Stdio {
            input: env.input,
            output: env.out,
        };
        match self.lifecycle.console(instance, &mut stream).await {
            Ok(()) => 0,
            Err(error) => env.fail(error),
        }
    }
}

fn flag_error(env: &mut Env<'_>, error: String) -> i32 {
    let _ = writeln!(env.err, "{error}");
    2
}

struct Stdio<'a> {
    input: &'a mut (dyn Read + Send),
    output: &'a mut (dyn Write + Send),
}

impl Read for Stdio<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buffer)
    }
}

impl Write for Stdio<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.output.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
