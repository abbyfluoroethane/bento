//! What part this machine plays in the deployment (MULTI-NODE 4).
//!
//! A Bento deployment has one controller and one runner service for each
//! machine that holds guests, including the controller's own machine. The
//! two run different units, so a screen that expected all four units
//! everywhere would report a healthy runner as a half-installed
//! controller.
//!
//! The role is read from facts the machine already carries. Nothing new
//! is written into the configuration for it: a machine that answers to
//! the definition below is that kind of machine.

use bento_config::Config;

use crate::systemd::{RUNNER, SERVE, UNITS, Unit, UnitStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Runs `serve`, the proxy, the SSH frontend, and its own runner.
    Controller,
    /// Runs the runner service alone. The control plane, the proxy, and
    /// the SSH frontend live on the controller (MULTI-NODE 19).
    Runner,
}

impl Role {
    /// Decides the role from the loaded configuration and what systemd
    /// already has.
    ///
    /// The order of these tests is the definition:
    ///
    /// 1. A configuration that names runner endpoints belongs to the
    ///    machine that calls them, which is the controller. Only the
    ///    controller dispatches (MULTI-NODE 11.2).
    /// 2. A machine that already has a `serve` unit file is a controller,
    ///    which is what a single-host version-1 deployment looks like.
    /// 3. A runner listener bound to an underlay address, with neither of
    ///    those, is a machine deliberately pointed at a controller
    ///    elsewhere.
    /// 4. Anything else is a controller. That is the default a fresh host
    ///    installs into, and the listener still on loopback says nothing
    ///    has pointed it anywhere else yet.
    pub fn detect(config: Option<&Config>, units: &[UnitStatus]) -> Role {
        if let Some(config) = config
            && !config.runners.is_empty()
        {
            return Role::Controller;
        }
        if units
            .iter()
            .any(|unit| unit.name == SERVE && unit.installed())
        {
            return Role::Controller;
        }
        match config {
            Some(config) if is_underlay_listener(&config.runner.listen) => Role::Runner,
            _ => Role::Controller,
        }
    }

    /// The units this machine is meant to run, in screen order.
    pub fn units(self) -> Vec<&'static Unit> {
        UNITS.iter().filter(|unit| self.wants(unit.name)).collect()
    }

    pub fn wants(self, unit: &str) -> bool {
        match self {
            Role::Controller => true,
            Role::Runner => unit == RUNNER,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Controller => "controller",
            Role::Runner => "runner",
        }
    }

    /// What the screen says when an action names a unit this machine does
    /// not run.
    pub fn not_here(self, unit: &str) -> String {
        format!(
            "{unit} does not run on a {} machine. The control plane, the proxy,\n\
             and the SSH frontend run on the controller (MULTI-NODE 19).",
            self.label()
        )
    }
}

/// Whether the runner listener has been pointed at this machine's own
/// underlay address rather than left on loopback. A loopback listener
/// reaches no other machine, so it is not evidence of anything.
fn is_underlay_listener(listen: &str) -> bool {
    let Some((host, _)) = listen.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(address) => !address.is_loopback() && !address.is_unspecified(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::merge_units;
    use crate::install::UNIT_DIR;
    use crate::systemd::PROXY;

    fn installed(name: &str) -> UnitStatus {
        UnitStatus {
            name: name.to_string(),
            load_state: "loaded".to_string(),
            fragment_path: format!("{UNIT_DIR}/{name}"),
            ..Default::default()
        }
    }

    fn config(listen: &str, runners: usize) -> Config {
        let mut config = Config::default();
        listen.clone_into(&mut config.runner.listen);
        config.runners = (0..runners)
            .map(|index| bento_config::RunnerEntry {
                name: format!("runner-{index}"),
                endpoint: format!("http://10.0.0.2{index}:10443"),
                underlay: String::new(),
            })
            .collect();
        config
    }

    #[test]
    fn a_configuration_that_names_runners_belongs_to_the_controller() {
        let config = config("10.0.0.188:10443", 2);
        assert_eq!(Role::detect(Some(&config), &[]), Role::Controller);
        assert_eq!(Role::detect(Some(&config), &[]).units().len(), UNITS.len());
    }

    #[test]
    fn a_runner_listener_on_an_underlay_address_and_nothing_else_is_a_runner() {
        let config = config("10.0.0.97:10443", 0);
        let role = Role::detect(Some(&config), &[]);
        assert_eq!(role, Role::Runner);
        let units = role.units();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, RUNNER);
        assert!(!role.wants(PROXY));
    }

    #[test]
    fn a_serve_unit_file_outranks_the_listener() {
        // A single-host version-1 controller binds no underlay address
        // and names no runners, and one that has been pointed at an
        // underlay address is still the machine that serves.
        let config = config("10.0.0.97:10443", 0);
        let units = merge_units(&[installed(SERVE)], Role::Controller);
        assert_eq!(Role::detect(Some(&config), &units), Role::Controller);
    }

    #[test]
    fn a_fresh_host_with_no_configuration_installs_as_a_controller() {
        assert_eq!(Role::detect(None, &[]), Role::Controller);
        let config = config(bento_config::defaults::RUNNER_LISTEN, 0);
        assert_eq!(Role::detect(Some(&config), &[]), Role::Controller);
    }

    #[test]
    fn a_wildcard_listener_says_nothing_about_the_role() {
        for listen in ["0.0.0.0:10443", "[::]:10443", "10443", ""] {
            let config = config(listen, 0);
            assert_eq!(
                Role::detect(Some(&config), &[]),
                Role::Controller,
                "{listen}"
            );
        }
    }
}
