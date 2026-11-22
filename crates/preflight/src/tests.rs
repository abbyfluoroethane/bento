use std::io::ErrorKind;
use std::path::PathBuf;

use bento_config::Config;

use super::*;

const HEALTHY: &str = r#"
base_domain = "example.org"

[oidc]
issuer = "https://id.example.org"
client_id = "bento"
client_secret = "secret"

[[images]]
name = "debian-13"
url = "https://cloud.example.org/debian-13-genericcloud-amd64.qcow2"

# Private, behind the TLS terminator (SPEC 8). The port stays 443 so the
# privileged-port checks still apply.
[listen]
https = "127.0.0.1:443"
"#;

/// [`HEALTHY`] plus `extra`. HEALTHY ends in `[listen]`, so an `extra`
/// that opens with that table continues it.
fn config(extra: &str) -> Config {
    let extra = extra.strip_prefix("[listen]\n").unwrap_or(extra);
    Config::parse(&format!("{HEALTHY}\n{extra}")).unwrap()
}

/// A config whose TOML replaces a whole table of [`HEALTHY`].
fn config_from(toml: &str) -> Config {
    Config::parse(toml).unwrap()
}

fn probe() -> FakeProbe {
    let mut probe = FakeProbe::healthy();
    probe.http.insert(
        "https://id.example.org/.well-known/openid-configuration".into(),
        Ok((200, r#"{"issuer":"https://id.example.org"}"#.into())),
    );
    probe.resolvable.insert("example.org".into());
    probe
        .resolvable
        .insert("preflight-check.example.org".into());
    probe
}

fn input(config: &Config) -> Input<'_> {
    Input {
        config: Some(config),
        config_error: None,
        config_path: Some(PathBuf::from("/etc/bento/bento.toml")),
        role: Role::Controller,
        online: true,
        nested_wanted: false,
        privilege: Privilege::Actual,
        binds: true,
    }
}

fn check(config: &Config, probe: &FakeProbe) -> Report {
    run(&input(config), probe)
}

fn find<'a>(report: &'a Report, id: &str) -> &'a Requirement {
    report
        .get(id)
        .unwrap_or_else(|| panic!("no requirement {id}: {:#?}", report.requirements))
}

fn fix_argv(requirement: &Requirement) -> Vec<Vec<String>> {
    requirement.fix.as_ref().unwrap().commands.clone()
}

fn argv(words: &[&str]) -> Vec<Vec<String>> {
    vec![words.iter().map(|word| word.to_string()).collect()]
}

#[test]
fn a_healthy_controller_meets_every_requirement() {
    let config = config("");
    let report = check(&config, &probe());
    for requirement in &report.requirements {
        assert_eq!(requirement.status, Status::Met, "{requirement:#?}");
        assert!(requirement.fix.is_none());
    }
    assert!(report.ok());
    assert_eq!(report.role, "controller");
    let ids: Vec<&str> = report.requirements.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "config-loads",
            "root-privileges",
            "container",
            "kvm-device",
            "ksm",
            "qemu-img",
            "xorriso",
            "nft-binary",
            "podman",
            "libvirt-socket",
            "dedicated-host",
            "nft-permitted",
            "ip-forward",
            "port-control-plane",
            "port-https",
            "port-ssh",
            "proxy-high-ports",
            "control-plane-outside-range",
            "open-files-limit",
            "image-dir",
            "image-dir-writable",
            "storage-dir",
            "storage-dir-writable",
            "container-storage",
            "container-storage-writable",
            "base-domain",
            "front-proxy",
            "oidc-settings",
            "oidc-discovery",
            "dns",
            "image-allowlist",
            "image-arch",
            "images-fetched",
        ]
    );
}

#[test]
fn groups_are_contiguous_and_in_enum_order() {
    let config = config("");
    let report = check(&config, &FakeProbe::default());
    let groups: Vec<u8> = report.requirements.iter().map(|r| r.group as u8).collect();
    let mut sorted = groups.clone();
    sorted.sort();
    assert_eq!(groups, sorted);
    assert_eq!(report.requirements[0].group, Group::Host);
    assert_eq!(report.requirements.last().unwrap().group, Group::Images);
}

#[test]
fn a_missing_configuration_runs_only_the_checks_that_need_none() {
    let input = Input {
        config: None,
        config_error: Some("config: base_domain is required".into()),
        config_path: None,
        role: Role::Controller,
        online: true,
        nested_wanted: false,
        privilege: Privilege::Actual,
        binds: true,
    };
    let report = run(&input, &probe());
    let loads = find(&report, "config-loads");
    assert_eq!(loads.status, Status::Unmet);
    assert!(loads.fatal);
    assert_eq!(loads.detail, "config: base_domain is required");
    assert!(!report.ok());
    for id in [
        "kvm-device",
        "nft-permitted",
        "dedicated-host",
        "ip-forward",
    ] {
        assert_eq!(find(&report, id).status, Status::Met, "{id}");
    }
    for id in [
        "libvirt-socket",
        "port-https",
        "image-dir",
        "base-domain",
        "image-allowlist",
    ] {
        assert!(report.get(id).is_none(), "{id} needs a configuration");
    }
}

#[test]
fn root_privileges_accept_root_or_both_capabilities() {
    let config = config("");
    for (status, expected) in [
        ("Uid:\t0\t0\t0\t0\nCapEff:\t0000000000000000\n", Status::Met),
        // CAP_NET_ADMIN (bit 12) and CAP_NET_BIND_SERVICE (bit 10).
        (
            "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t0000000000001400\n",
            Status::Met,
        ),
        (
            "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t0000000000001000\n",
            Status::Warn,
        ),
        (
            "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t0000000000000000\n",
            Status::Warn,
        ),
    ] {
        let probe = probe().with_file("/proc/self/status", status);
        let requirement = find(&check(&config, &probe), "root-privileges").clone();
        assert_eq!(requirement.status, expected, "{status}");
        assert!(!requirement.fatal);
        if expected == Status::Warn {
            assert!(requirement.detail.contains("CAP_NET_ADMIN"));
            assert!(requirement.detail.contains("libvirt group"));
            assert!(fix_argv(&requirement).is_empty());
        }
    }
}

#[test]
fn a_runner_without_root_only_warns() {
    let config = config("");
    let probe = probe().with_file("/proc/self/status", "Uid:\t1000\t1000\t1000\t1000\n");
    let mut input = input(&config);
    input.role = Role::Runner;
    let report = run(&input, &probe);
    let requirement = find(&report, "root-privileges");
    assert_eq!(requirement.status, Status::Warn);
    assert!(!requirement.fatal);
}

#[test]
fn a_container_marker_or_a_shifted_uid_map_warns() {
    let config = config("");
    assert!(identity_uid_map("         0          0 4294967295\n"));
    assert!(!identity_uid_map(
        "         0       1000          1\n         1     100000      65536\n"
    ));
    for probe in [
        probe().with_file("/run/.containerenv", ""),
        probe().with_file("/.dockerenv", ""),
        probe().with_file("/proc/self/uid_map", "0 1000 1\n1 100000 65536\n"),
    ] {
        let requirement = find(&check(&config, &probe), "container").clone();
        assert_eq!(requirement.status, Status::Warn);
        assert!(
            requirement
                .detail
                .contains("nft cannot change the host firewall")
        );
    }
}

#[test]
fn hypervisor_failures_keep_their_severity_and_get_a_fix() {
    let config = config("");
    let report = check(&config, &probe().without("/dev/kvm"));
    let kvm = find(&report, "kvm-device");
    assert_eq!((kvm.status, kvm.fatal), (Status::Unmet, true));
    assert!(!report.ok());

    let report = check(&config, &probe().with_file("/sys/kernel/mm/ksm/run", "0\n"));
    let ksm = find(&report, "ksm");
    assert_eq!(ksm.status, Status::Warn);
    assert!(ksm.fix.as_ref().unwrap().root);
    assert_eq!(
        fix_argv(ksm),
        vec![
            argv(&["dnf", "install", "-y", "ksmtuned"]).remove(0),
            argv(&["systemctl", "enable", "--now", "ksmtuned"]).remove(0),
        ]
    );
    assert_eq!(
        ksm.detail,
        "/sys/kernel/mm/ksm/run is 0. KSM does not share memory between guests, so the host holds fewer guests (SPEC 5.4)."
    );
    assert!(report.ok());

    // Elsewhere the fix names the package and runs nothing.
    let debian = probe()
        .with_file("/sys/kernel/mm/ksm/run", "0\n")
        .with_file("/etc/os-release", "ID=debian\n");
    let fix = find(&check(&config, &debian), "ksm").fix.clone().unwrap();
    assert!(fix.commands.is_empty());
    assert!(fix.summary.contains("ksmtuned package"), "{}", fix.summary);

    // A kernel without KSM gets a sentence too.
    let report = check(&config, &probe().without("/sys/kernel/mm/ksm/run"));
    let ksm = find(&report, "ksm");
    assert!(
        ksm.detail
            .starts_with("Cannot read /sys/kernel/mm/ksm/run: "),
        "{}",
        ksm.detail
    );
}

#[test]
fn nested_virtualization_is_checked_only_when_wanted() {
    let config = config("");
    let probe = probe().with_file("/sys/module/kvm_intel/parameters/nested", "N\n");
    assert!(
        check(&config, &probe)
            .get("nested-virtualization")
            .is_none()
    );
    let mut wanted = input(&config);
    wanted.nested_wanted = true;
    let report = run(&wanted, &probe);
    assert_eq!(find(&report, "nested-virtualization").status, Status::Warn);
    let report = run(&wanted, &self::probe());
    assert_eq!(find(&report, "nested-virtualization").status, Status::Met);
}

#[test]
fn missing_binaries_get_a_dnf_fix_on_fedora_and_a_manual_fix_elsewhere() {
    let config = config("");
    for (binary, id, package) in [
        ("qemu-img", "qemu-img", "qemu-img"),
        ("xorriso", "xorriso", "xorriso"),
        ("nft", "nft-binary", "nftables"),
    ] {
        let report = check(&config, &probe().without(binary));
        let requirement = find(&report, id);
        assert_eq!(requirement.status, Status::Unmet, "{id}");
        assert!(requirement.fatal);
        assert_eq!(
            fix_argv(requirement),
            argv(&["dnf", "install", "-y", package])
        );
        assert!(requirement.fix.as_ref().unwrap().root);

        let debian = probe()
            .without(binary)
            .with_file("/etc/os-release", "ID=debian\n");
        let report = check(&config, &debian);
        let fix = find(&report, id).fix.clone().unwrap();
        assert!(fix.commands.is_empty());
        assert!(fix.summary.contains(package));
    }
    assert!(fedora_like(
        "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"
    ));
    assert!(!fedora_like("ID=debian\n"));
}

#[test]
fn podman_is_fatal_only_with_a_static_oci_image() {
    let without = probe().without("podman");
    let report = check(&config(""), &without);
    assert_eq!(find(&report, "podman").status, Status::Warn);

    let oci = config(&format!(
        "[bootc]\nbuilder_image = \"quay.example.org/builder@sha256:{}\"\n\n\
         [[images]]\nname = \"fedora-bootc\"\noci = \"quay.example.org/fedora-bootc:44\"\n",
        "a".repeat(64)
    ));
    let report = check(&oci, &without);
    let podman = find(&report, "podman");
    assert_eq!((podman.status, podman.fatal), (Status::Unmet, true));
}

#[test]
fn a_silent_libvirt_socket_suggests_virtqemud_when_it_exists() {
    let config = config("");
    let mut probe = probe().without(bento_hypervisor::DEFAULT_SOCKET_PATH);
    let report = check(&config, &probe);
    let socket = find(&report, "libvirt-socket");
    assert_eq!(socket.status, Status::Unmet);
    assert_eq!(
        fix_argv(socket),
        argv(&[
            "systemctl",
            "enable",
            "--now",
            "virtqemud.socket",
            "virtnetworkd.socket"
        ])
    );

    probe.sockets.insert("/run/libvirt/virtqemud-sock".into());
    let report = check(&config, &probe);
    let fix = find(&report, "libvirt-socket").fix.clone().unwrap();
    assert!(fix.commands.is_empty());
    assert!(
        fix.summary
            .contains("qemu:///system?socket=/run/libvirt/virtqemud-sock")
    );

    let modular = config_from(&format!(
        "libvirt_uri = \"qemu:///system?socket=/run/libvirt/virtqemud-sock\"\n{HEALTHY}"
    ));
    assert_eq!(
        find(&check(&modular, &probe), "libvirt-socket").status,
        Status::Met
    );
}

#[test]
fn only_libvirtd_installed_gets_the_libvirtd_socket() {
    let config = config("");
    for probe in [
        {
            let mut probe = probe().without(bento_hypervisor::DEFAULT_SOCKET_PATH);
            probe.binaries.insert("libvirtd".into());
            probe
        },
        // Fedora puts the daemons in /usr/sbin, which PATH often lacks.
        probe()
            .without(bento_hypervisor::DEFAULT_SOCKET_PATH)
            .with_file("/usr/sbin/libvirtd", ""),
    ] {
        let fix = find(&check(&config, &probe), "libvirt-socket")
            .fix
            .clone()
            .unwrap();
        assert!(fix.root);
        assert_eq!(
            fix.commands,
            argv(&["systemctl", "enable", "--now", "libvirtd.socket"])
        );
    }
}

#[test]
fn virtqemud_with_the_default_uri_gets_the_uri_fix() {
    let config = config("");
    // virtqemud is installed and answers: only the URI is wrong, and no
    // systemctl command makes libvirt-sock appear.
    let mut probe = probe()
        .without(bento_hypervisor::DEFAULT_SOCKET_PATH)
        .with_file("/usr/sbin/virtqemud", "")
        .with_file("/usr/sbin/libvirtd", "");
    probe.sockets.insert("/run/libvirt/virtqemud-sock".into());
    let fix = find(&check(&config, &probe), "libvirt-socket")
        .fix
        .clone()
        .unwrap();
    assert!(fix.commands.is_empty(), "{fix:?}");
    assert!(
        fix.summary
            .contains("libvirt_uri = \"qemu:///system?socket=/run/libvirt/virtqemud-sock\""),
        "{}",
        fix.summary
    );

    // virtqemud-sock does not answer either: the URI fix, and the command
    // that starts the modular daemons.
    probe
        .sockets
        .remove(std::path::Path::new("/run/libvirt/virtqemud-sock"));
    let fix = find(&check(&config, &probe), "libvirt-socket")
        .fix
        .clone()
        .unwrap();
    assert!(fix.summary.contains("libvirt_uri"), "{}", fix.summary);
    assert!(fix.root);
    assert_eq!(
        fix.commands,
        argv(&[
            "systemctl",
            "enable",
            "--now",
            "virtqemud.socket",
            "virtnetworkd.socket"
        ])
    );
}

#[test]
fn other_bridges_warn_that_the_host_is_not_dedicated() {
    let config = config("");
    let mut probe = probe();
    probe.dirs.insert(
        "/sys/class/net".into(),
        vec![
            "lo".into(),
            "bento0".into(),
            "virbr0".into(),
            "tailscale0".into(),
        ],
    );
    let requirement = find(&check(&config, &probe), "dedicated-host").clone();
    assert_eq!(requirement.status, Status::Warn);
    assert!(requirement.detail.contains("tailscale0, virbr0"));
    assert!(!requirement.detail.contains("bento0"));
    assert!(requirement.detail.contains("policy drop"));
    assert_eq!(
        foreign_bridges(&[
            "docker0".into(),
            "br-1a2b".into(),
            "cni-podman0".into(),
            "podman1".into(),
            "eth0".into()
        ]),
        ["br-1a2b", "cni-podman0", "docker0", "podman1"]
    );
}

#[test]
fn a_refused_nft_check_is_fatal_and_a_missing_nft_is_skipped() {
    let config = config("");
    let mut refused = probe();
    refused.nft = NftOutcome::Refused(
        "netlink: Error: cache initialization failed: Operation not permitted".into(),
    );
    let report = check(&config, &refused);
    let nft = find(&report, "nft-permitted");
    assert_eq!((nft.status, nft.fatal), (Status::Unmet, true));
    assert!(nft.detail.contains("Operation not permitted"));
    assert!(!report.ok());

    let report = check(&config, &probe().without("nft"));
    assert_eq!(find(&report, "nft-permitted").status, Status::Skipped);
    assert_eq!(
        check_nft_permitted(&refused).status,
        Status::Unmet,
        "serve uses the same decision"
    );
}

#[test]
fn ip_forward_off_warns_with_a_sysctl_fix() {
    let config = config("");
    let report = check(
        &config,
        &probe().with_file("/proc/sys/net/ipv4/ip_forward", "0\n"),
    );
    let requirement = find(&report, "ip-forward");
    assert_eq!(requirement.status, Status::Warn);
    assert_eq!(
        fix_argv(requirement),
        argv(&["sysctl", "-w", "net.ipv4.ip_forward=1"])
    );
}

#[test]
fn a_busy_port_names_its_holder_and_bentod_counts_as_met() {
    let config = config("");
    let mut probe = probe();
    probe.bind_errors.insert(22, ErrorKind::AddrInUse);
    probe.bind_errors.insert(443, ErrorKind::AddrInUse);
    probe.bind_errors.insert(10080, ErrorKind::AddrInUse);
    probe.holders.insert(
        22,
        Holder {
            pid: 812,
            comm: "sshd".into(),
        },
    );
    probe.holders.insert(
        443,
        Holder {
            pid: 900,
            comm: "bentod".into(),
        },
    );
    let report = check(&config, &probe);
    let ssh = find(&report, "port-ssh");
    assert_eq!(ssh.status, Status::Unmet);
    assert!(ssh.detail.contains("sshd (pid 812)"), "{}", ssh.detail);
    assert_eq!(find(&report, "port-https").status, Status::Met);
    let control = find(&report, "port-control-plane");
    assert_eq!(control.status, Status::Unmet);
    assert!(control.detail.contains("cannot see"));
    assert_eq!(probe.holder_scans(), 1, "one /proc scan for the whole run");
}

#[test]
fn a_privileged_port_without_the_capability_is_unmet() {
    let config = config("");
    let mut probe = probe();
    probe.bind_errors.insert(443, ErrorKind::PermissionDenied);
    let report = check(&config, &probe);
    let https = find(&report, "port-https");
    assert_eq!(https.status, Status::Unmet);
    assert!(https.detail.contains("CAP_NET_BIND_SERVICE"));
    assert_eq!(probe.holder_scans(), 0, "no scan without a busy port");
}

#[test]
fn busy_high_ports_are_counted_and_cockpit_gets_its_fix() {
    let config = config("");
    let mut probe = probe();
    for port in [4000, 5355, 9090] {
        probe.bind_errors.insert(port, ErrorKind::AddrInUse);
    }
    probe.holders.insert(
        9090,
        Holder {
            pid: 1,
            comm: "systemd".into(),
        },
    );
    probe.holders.insert(
        5355,
        Holder {
            pid: 640,
            comm: "systemd-resolve".into(),
        },
    );
    probe.holders.insert(
        4000,
        Holder {
            pid: 77,
            comm: "bentod".into(),
        },
    );
    let report = check(&config, &probe);
    let high = find(&report, "proxy-high-ports");
    assert_eq!(high.status, Status::Unmet);
    assert!(
        high.detail.starts_with("2 of 7000 ports"),
        "{}",
        high.detail
    );
    assert!(high.detail.contains("5355 (systemd-resolve pid 640)"));
    assert!(high.detail.contains("9090 (systemd pid 1)"));
    assert_eq!(
        fix_argv(high),
        argv(&["systemctl", "disable", "--now", "cockpit.socket"])
    );
    assert!(high.fix.as_ref().unwrap().summary.contains("LLMNR=no"));
    assert_eq!(probe.holder_scans(), 1);

    probe.bind_errors.remove(&9090);
    let report = check(&config, &probe);
    let fix = find(&report, "proxy-high-ports").fix.clone().unwrap();
    assert!(fix.commands.is_empty());
    assert!(fix.summary.contains("LLMNR=no"));

    probe.holders.insert(
        5355,
        Holder {
            pid: 5,
            comm: "python3".into(),
        },
    );
    let report = check(&config, &probe);
    let fix = find(&report, "proxy-high-ports").fix.clone().unwrap();
    assert!(fix.summary.contains("proxy_port_min"));
}

#[test]
fn many_busy_high_ports_name_only_the_first_few() {
    let config = config("");
    let mut probe = probe();
    for port in 3000..3008 {
        probe.bind_errors.insert(port, ErrorKind::AddrInUse);
    }
    let high = find(&check(&config, &probe), "proxy-high-ports").clone();
    assert!(
        high.detail.contains("3004 (holder unknown) and 3 more"),
        "{}",
        high.detail
    );
    assert!(!high.detail.contains("3005"));
}

#[test]
fn a_control_plane_inside_the_proxy_range_is_unmet() {
    // The configuration refuses this at load, so the fake config is built
    // by hand to reach the decision.
    let mut config = config("");
    config.listen.http = "127.0.0.1:8080".into();
    let requirement = find(&check(&config, &probe()), "control-plane-outside-range").clone();
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Unmet, true)
    );
}

#[test]
fn a_low_open_files_limit_warns() {
    let config = config("");
    let probe = probe().with_file(
        "/proc/self/limits",
        "Limit                     Soft Limit           Hard Limit           Units     \n\
         Max open files            1024                 524288               files     \n",
    );
    let requirement = find(&check(&config, &probe), "open-files-limit").clone();
    assert_eq!(requirement.status, Status::Warn);
    assert!(
        requirement.detail.contains("7064"),
        "{}",
        requirement.detail
    );
    assert!(requirement.detail.contains("LimitNOFILE=65536"));

    let narrow = self::config("[listen]\nproxy_port_min = 13000\nproxy_port_max = 13009\n");
    assert_eq!(
        find(&check(&narrow, &probe), "open-files-limit").status,
        Status::Met
    );
    assert_eq!(
        soft_nofile("Max open files  unlimited  unlimited  files\n"),
        Some(u64::MAX)
    );
}

#[test]
fn a_runner_checks_only_its_own_port_and_no_controller_settings() {
    let config = config("[runner]\nlisten = \"192.0.2.10:10443\"\n");
    let mut probe = probe();
    probe.bind_errors.insert(10443, ErrorKind::AddrNotAvailable);
    let mut input = input(&config);
    input.role = Role::Runner;
    let report = run(&input, &probe);
    assert_eq!(report.role, "runner");
    let runner = find(&report, "port-runner");
    assert_eq!(runner.status, Status::Unmet);
    for id in [
        "port-https",
        "proxy-high-ports",
        "open-files-limit",
        "base-domain",
        "oidc-settings",
        "image-allowlist",
    ] {
        assert!(report.get(id).is_none(), "{id} is a controller requirement");
    }
    probe.bind_errors.clear();
    assert_eq!(
        find(&run(&input, &probe), "port-runner").status,
        Status::Met
    );
}

#[test]
fn missing_directories_get_mkdir_and_skip_the_write_probe() {
    let config = config("");
    let report = check(&config, &probe().without("/var/lib/bento/storage"));
    let storage = find(&report, "storage-dir");
    assert_eq!(storage.status, Status::Unmet);
    assert_eq!(
        fix_argv(storage),
        argv(&["mkdir", "-p", "/var/lib/bento/storage"])
    );
    assert_eq!(
        find(&report, "storage-dir-writable").status,
        Status::Skipped
    );

    let mut probe = probe();
    probe.unwritable.insert("/var/lib/bento/images".into());
    let report = check(&config, &probe);
    let writable = find(&report, "image-dir-writable");
    assert_eq!((writable.status, writable.fatal), (Status::Unmet, true));
}

#[test]
fn a_documentation_base_domain_warns() {
    let config = config_from(&HEALTHY.replace(
        "base_domain = \"example.org\"",
        "base_domain = \"bento.example.org\"",
    ));
    let mut probe = probe();
    probe.resolvable.insert("bento.example.org".into());
    probe
        .resolvable
        .insert("preflight-check.bento.example.org".into());
    let requirement = find(&check(&config, &probe), "base-domain").clone();
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Warn, false)
    );

    let mut empty = self::config("");
    empty.base_domain.clear();
    let requirement = find(&check(&empty, &probe), "base-domain").clone();
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Unmet, true)
    );
}

#[test]
fn plain_http_on_a_public_address_warns() {
    let public = config_from(&HEALTHY.replace("https = \"127.0.0.1:443\"", "https = \":443\""));
    let report = check(&public, &probe());
    let requirement = find(&report, "front-proxy");
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Warn, false)
    );
    assert!(requirement.detail.contains("not a loopback address"));
    assert!(report.ok());

    assert_eq!(
        find(&check(&config(""), &probe()), "front-proxy").status,
        Status::Met
    );
}

/// Keys from the Bento that terminated TLS still load (SPEC 8), and the
/// check asks for their removal without stopping startup.
#[test]
fn retired_tls_keys_warn() {
    let retired = config(
        "[listen]\ntls = \"off\"\n\n[acme]\nemail = \"ops@example.org\"\ncloudflare_token = \"token\"\n",
    );
    let report = check(&retired, &probe());
    let requirement = find(&report, "front-proxy");
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Warn, false)
    );
    assert!(requirement.detail.contains("listen.tls and [acme]"));
    assert!(!requirement.detail.contains("loopback"));
    assert!(report.ok());
}

#[test]
fn oidc_needs_every_setting_but_only_warns() {
    // serve starts without OIDC, so the requirement is not fatal. The
    // detail says what does not work.
    let config = config_from(&HEALTHY.replace("client_secret = \"secret\"", ""));
    let report = check(&config, &probe());
    let requirement = find(&report, "oidc-settings").clone();
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Warn, false)
    );
    assert!(requirement.detail.starts_with("oidc.client_secret"));
    assert!(requirement.detail.contains("nobody can sign in"));
    assert!(report.ok());
}

#[test]
fn oidc_discovery_runs_only_online_and_warns_on_failure() {
    let config = config("");
    let mut offline = input(&config);
    offline.online = false;
    let report = run(&offline, &probe());
    assert_eq!(find(&report, "oidc-discovery").status, Status::Skipped);
    assert_eq!(find(&report, "dns").status, Status::Skipped);

    let url = "https://id.example.org/.well-known/openid-configuration";
    for response in [
        Err("connection refused".to_string()),
        Ok((404, String::new())),
        Ok((200, "<html>".to_string())),
        Ok((200, r#"{"issuer":"https://other.example.org"}"#.to_string())),
    ] {
        let mut probe = probe();
        probe.http.insert(url.into(), response.clone());
        let requirement = find(&check(&config, &probe), "oidc-discovery").clone();
        assert_eq!(
            (requirement.status, requirement.fatal),
            (Status::Warn, false),
            "{response:?}"
        );
    }
}

#[test]
fn an_unresolvable_name_warns() {
    let config = config("");
    let mut probe = probe();
    probe.resolvable.remove("preflight-check.example.org");
    let requirement = find(&check(&config, &probe), "dns").clone();
    assert_eq!(requirement.status, Status::Warn);
    assert!(requirement.detail.contains("preflight-check.example.org"));
    assert!(!requirement.detail.starts_with("example.org"));
}

#[test]
fn an_empty_allowlist_only_warns() {
    // No process refuses an empty allowlist.
    let config = config_from(&HEALTHY[..HEALTHY.find("[[images]]").unwrap()]);
    let report = check(&config, &probe());
    let requirement = find(&report, "image-allowlist");
    assert_eq!(
        (requirement.status, requirement.fatal),
        (Status::Warn, false)
    );
    assert!(requirement.detail.contains("nobody can make an instance"));
    assert!(report.ok());
}

#[test]
fn an_image_for_the_other_architecture_warns() {
    let config = config("");
    let mut probe = probe();
    probe.arch = "aarch64".into();
    let requirement = find(&check(&config, &probe), "image-arch").clone();
    assert_eq!(requirement.status, Status::Warn);
    assert!(requirement.detail.contains("debian-13"));

    let arm = config_from(&HEALTHY.replace("amd64", "arm64"));
    assert_eq!(find(&check(&arm, &probe), "image-arch").status, Status::Met);
    probe.arch = "x86_64".into();
    assert_eq!(
        find(&check(&arm, &probe), "image-arch").status,
        Status::Warn
    );
}

#[test]
fn unfetched_images_suggest_fetch_images_with_the_config_path() {
    let config = config("");
    let mut probe = probe();
    probe
        .dirs
        .insert("/var/lib/bento/images".into(), vec!["partial.tmp".into()]);
    let report = check(&config, &probe);
    let requirement = find(&report, "images-fetched");
    assert_eq!(requirement.status, Status::Warn);
    assert_eq!(requirement.title, "Image files present");
    assert_eq!(
        fix_argv(requirement),
        argv(&["bentod", "-config", "/etc/bento/bento.toml", "fetch-images"])
    );

    let mut no_path = input(&config);
    no_path.config_path = None;
    let report = run(&no_path, &probe);
    assert_eq!(
        fix_argv(find(&report, "images-fetched")),
        argv(&["bentod", "fetch-images"])
    );
}

#[test]
fn report_ok_unmet_and_counts_follow_the_statuses() {
    let requirement = |id: &str, status, fatal| Requirement {
        id: id.into(),
        group: Group::Host,
        title: id.into(),
        status,
        fatal,
        detail: String::new(),
        fix: None,
    };
    let report = Report {
        role: "controller".into(),
        requirements: vec![
            requirement("a", Status::Met, true),
            requirement("b", Status::Unmet, false),
            requirement("c", Status::Warn, false),
            requirement("d", Status::Unmet, true),
            requirement("e", Status::Skipped, true),
        ],
    };
    assert!(!report.ok());
    let unmet: Vec<&str> = report.unmet().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(unmet, ["d", "b"]);
    assert_eq!(report.counts(), (1, 2, 1));

    let mut warn_only = report.clone();
    warn_only.requirements.remove(3);
    assert!(warn_only.ok(), "a non-fatal Unmet does not stop Bento");
}

#[test]
fn detect_role_needs_an_underlay_listener_and_no_runner_list() {
    assert_eq!(detect_role(None), Role::Controller);
    assert_eq!(detect_role(Some(&config(""))), Role::Controller);
    let runner = config("[runner]\nlisten = \"192.0.2.10:10443\"\n");
    assert_eq!(detect_role(Some(&runner)), Role::Runner);
    let controller = config(
        "[runner]\nlisten = \"192.0.2.10:10443\"\n\n\
         [[runners]]\nname = \"r1\"\nendpoint = \"192.0.2.11:10443\"\n",
    );
    assert_eq!(detect_role(Some(&controller)), Role::Controller);
}

#[test]
fn a_requirement_serializes_to_the_contract_shape() {
    let requirement = Requirement {
        id: "ip-forward".into(),
        group: Group::Network,
        title: "IPv4 forwarding".into(),
        status: Status::Warn,
        fatal: false,
        detail: "net.ipv4.ip_forward is 0.".into(),
        fix: Some(Fix {
            summary: "Turn on IPv4 forwarding.".into(),
            commands: argv(&["sysctl", "-w", "net.ipv4.ip_forward=1"]),
            root: true,
        }),
    };
    let report = Report {
        role: "controller".into(),
        requirements: vec![requirement],
    };
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "role": "controller",
            "requirements": [{
                "id": "ip-forward",
                "group": "network",
                "title": "IPv4 forwarding",
                "status": "warn",
                "fatal": false,
                "detail": "net.ipv4.ip_forward is 0.",
                "fix": {
                    "summary": "Turn on IPv4 forwarding.",
                    "commands": [["sysctl", "-w", "net.ipv4.ip_forward=1"]],
                    "root": true
                }
            }]
        })
    );
    let met = find(&check(&config(""), &probe()), "kvm-device").clone();
    assert_eq!(
        serde_json::to_value(&met).unwrap()["fix"],
        serde_json::Value::Null
    );
}

#[test]
fn a_met_image_check_says_that_it_only_counts_files() {
    let requirement = find(&check(&config(""), &probe()), "images-fetched").clone();
    assert_eq!(requirement.status, Status::Met);
    assert!(
        requirement
            .detail
            .contains("The Images step of the Install tab checks each allowlist entry"),
        "{}",
        requirement.detail
    );
}

/// The host as a user that is not root sees it: no capabilities, EPERM
/// from nft, EACCES in the root-owned directories and on port 22 and 443,
/// and root holders that it cannot name.
fn unprivileged(mut probe: FakeProbe) -> FakeProbe {
    probe.files.insert(
        "/proc/self/status".into(),
        b"Uid:\t1000\t1000\t1000\t1000\nCapEff:\t0000000000000000\n".to_vec(),
    );
    probe.nft = NftOutcome::Refused(
        "netlink: Error: cache initialization failed: Operation not permitted".into(),
    );
    for directory in [
        "/var/lib/bento/images",
        "/var/lib/bento/storage",
        bento_config::defaults::CONTAINER_STORAGE,
    ] {
        probe.unwritable.insert(directory.into());
    }
    probe.bind_errors.insert(22, ErrorKind::PermissionDenied);
    probe.bind_errors.insert(443, ErrorKind::PermissionDenied);
    // A root bentod holds the control-plane port and part of the proxy
    // range, and this user cannot see it.
    probe.bind_errors.insert(10080, ErrorKind::AddrInUse);
    probe.bind_errors.insert(4000, ErrorKind::AddrInUse);
    probe
}

fn observer_input(config: &Config) -> Input<'_> {
    Input {
        privilege: Privilege::Observer,
        ..input(config)
    }
}

#[test]
fn an_observer_skips_exactly_the_checks_that_need_root() {
    let config = config("");
    let probe = unprivileged(probe());
    let report = run(&observer_input(&config), &probe);
    for id in [
        "root-privileges",
        "nft-permitted",
        "port-https",
        "port-ssh",
        "image-dir-writable",
        "storage-dir-writable",
        "container-storage-writable",
    ] {
        let requirement = find(&report, id);
        assert_eq!(requirement.status, Status::Skipped, "{requirement:#?}");
        assert!(
            requirement
                .detail
                .ends_with("Run the check as root to test this."),
            "{requirement:#?}"
        );
    }
    assert!(
        find(&report, "root-privileges")
            .detail
            .contains("The units run as root.")
    );
    // A busy port whose holder is hidden only warns, and says how to see
    // the holder.
    for id in ["port-control-plane", "proxy-high-ports"] {
        let requirement = find(&report, id);
        assert_eq!(
            (requirement.status, requirement.fatal),
            (Status::Warn, false),
            "{requirement:#?}"
        );
        assert!(
            requirement
                .detail
                .contains("Run the check as root to see the holder."),
            "{requirement:#?}"
        );
    }
    // Every other requirement is as the healthy host has it.
    let changed = [
        "root-privileges",
        "nft-permitted",
        "port-https",
        "port-ssh",
        "port-control-plane",
        "proxy-high-ports",
        "image-dir-writable",
        "storage-dir-writable",
        "container-storage-writable",
    ];
    for requirement in &report.requirements {
        if !changed.contains(&requirement.id.as_str()) {
            assert_eq!(requirement.status, Status::Met, "{requirement:#?}");
        }
    }

    // The same host checked for this process itself fails each of them.
    let report = run(&input(&config), &probe);
    // Container storage is not fatal without a bootc OCI entry, so it
    // warns.
    for id in changed {
        let status = find(&report, id).status;
        assert!(
            matches!(status, Status::Unmet | Status::Warn),
            "{id}: {status:?}"
        );
    }
}

#[test]
fn the_gate_opens_for_an_observer_on_a_host_that_only_lacks_root() {
    let config = config("");
    let probe = unprivileged(probe());
    assert!(run(&observer_input(&config), &probe).ok());
    assert!(!run(&input(&config), &probe).ok());
}

#[test]
fn an_observer_still_fails_what_root_does_not_fix() {
    let config = config("");
    // An nft refusal that is not EPERM.
    let mut probe = unprivileged(probe());
    probe.nft =
        NftOutcome::Refused("Error: Could not process rule: No such file or directory".into());
    // A hidden holder next to a named one: the named one is a real
    // conflict.
    probe.bind_errors.insert(9090, ErrorKind::AddrInUse);
    probe.holders.insert(
        9090,
        Holder {
            pid: 1,
            comm: "systemd".into(),
        },
    );
    // A write probe that fails for a reason other than EACCES.
    probe.unwritable.clear();
    probe.bind_errors.insert(22, ErrorKind::AddrNotAvailable);
    let report = run(&observer_input(&config), &probe);
    assert_eq!(find(&report, "nft-permitted").status, Status::Unmet);
    assert_eq!(find(&report, "proxy-high-ports").status, Status::Unmet);
    assert_eq!(find(&report, "port-ssh").status, Status::Unmet);
    assert!(!report.ok());
}

#[test]
fn an_observer_in_a_user_namespace_still_fails_an_nft_refusal() {
    // Root in a rootless container gets the same EPERM, so the observer
    // must not hide it (DEPLOYING.md section 9).
    let config = config("");
    let probe = unprivileged(probe()).with_file(
        "/proc/self/uid_map",
        b"         0       1000          1\n      1000          0          1\n",
    );
    let report = run(&observer_input(&config), &probe);
    assert_eq!(find(&report, "nft-permitted").status, Status::Unmet);
    assert!(!report.ok());
}

#[test]
fn a_run_without_binds_skips_every_port_bind() {
    let config = config("");
    let probe = probe();
    let report = run(
        &Input {
            binds: false,
            ..input(&config)
        },
        &probe,
    );
    for id in [
        "port-control-plane",
        "port-https",
        "port-ssh",
        "proxy-high-ports",
    ] {
        let requirement = find(&report, id);
        assert_eq!(requirement.status, Status::Skipped, "{id}");
        assert!(requirement.detail.contains("F5"), "{id}");
    }
    assert!(report.ok());
}
