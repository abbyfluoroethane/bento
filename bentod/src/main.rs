//! The Bento binary. One binary holds every process as a subcommand
//! (SPEC section 4): the control plane (`serve`), the HTTP proxy (`proxy`),
//! and the SSH frontend (`sshd`), plus the operator commands from SPEC 15.

mod adapters;
mod firewall;
mod keys;
mod ops;
mod proxyd;
mod serve;
mod setup;
mod sshd;

#[cfg(test)]
mod adapters_tests;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "/etc/bento/bento.toml";

const COMMANDS: [(&str, &str); 7] = [
    (
        "serve",
        "run the control plane: database, policy, dashboard",
    ),
    (
        "proxy",
        "run the HTTP proxy on port 443 and ports 3000-9999",
    ),
    ("sshd", "run the SSH frontend on port 22"),
    (
        "fetch-images",
        "download, verify, and store allowlisted images",
    ),
    (
        "reconcile",
        "report disagreements between libvirt and the database",
    ),
    (
        "dump-db",
        "write a consistent database copy with the backup API",
    ),
    (
        "images",
        "list images, current checksums, and stale instance counts",
    ),
];

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_target(false)
        .init();
    std::process::exit(run(std::env::args_os().skip(1).collect()).await);
}

async fn run(args: Vec<OsString>) -> i32 {
    let (config_path, rest) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            if !error.is_empty() {
                eprintln!("{error}");
            }
            usage(&mut io::stderr());
            return 2;
        }
    };
    let Some((name, command_args)) = rest.split_first() else {
        usage(&mut io::stderr());
        return 2;
    };
    let Some(name) = name.to_str() else {
        eprintln!("bentod: command is not valid UTF-8");
        return 2;
    };
    let result = match name {
        "serve" => serve::run_serve(&config_path, command_args).await,
        "proxy" => proxyd::run_proxy(&config_path, command_args).await,
        "sshd" => sshd::run_sshd(&config_path, command_args).await,
        "fetch-images" => ops::run_fetch_images(&config_path, command_args).await,
        "reconcile" => ops::run_reconcile(&config_path, command_args).await,
        "dump-db" => ops::run_dump_db(&config_path, command_args).await,
        "images" => ops::run_images(&config_path, command_args).await,
        _ => {
            eprintln!("bentod: unknown command {name:?}\n");
            usage(&mut io::stderr());
            return 2;
        }
    };
    if let Err(error) = result {
        eprintln!("bentod {name}: {error:#}");
        1
    } else {
        0
    }
}

fn parse_args(args: Vec<OsString>) -> Result<(PathBuf, Vec<OsString>), String> {
    let mut config = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut rest = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        if text == "-h" || text == "--help" {
            return Err(String::new());
        }
        if text == "-config" || text == "--config" {
            config = iter
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("flag needs an argument: {text}"))?;
            continue;
        }
        if let Some(value) = text
            .strip_prefix("-config=")
            .or_else(|| text.strip_prefix("--config="))
        {
            config = PathBuf::from(value);
            continue;
        }
        if text.starts_with('-') {
            return Err(format!("flag provided but not defined: {text}"));
        }
        rest.push(arg);
        rest.extend(iter);
        break;
    }
    Ok((config, rest))
}

fn usage(writer: &mut dyn Write) {
    let _ = writeln!(writer, "Usage: bentod [flags] <command> [arguments]");
    let _ = writeln!(writer, "\nCommands:");
    for (name, summary) in COMMANDS {
        let _ = writeln!(writer, "  {name:<14} {summary}");
    }
    let _ = writeln!(writer, "\nFlags:");
    let _ = writeln!(writer, "  -config string");
    let _ = writeln!(
        writer,
        "    \tpath to the bento configuration file (default \"{DEFAULT_CONFIG_PATH}\")"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_exit_codes() {
        for (args, expected) in [
            (vec![], 2),
            (vec!["frobnicate"], 2),
            (vec!["serve"], 1),
            (vec!["proxy"], 1),
            (vec!["sshd"], 1),
            (vec!["fetch-images"], 1),
            (vec!["reconcile"], 1),
            (vec!["dump-db"], 1),
            (vec!["images"], 1),
            (vec!["-config", "/nonexistent/x.toml", "serve"], 1),
            (vec!["-nope"], 2),
        ] {
            assert_eq!(
                run(args.into_iter().map(OsString::from).collect()).await,
                expected
            );
        }
    }

    #[test]
    fn rustls_provider_supports_the_production_client_path() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _client = reqwest::Client::new();
        let _images_client = bento_images::ReqwestClient::default();
    }
}
