//! The screen. Every function here reads [`App`] and writes widgets; none
//! of them touches the host.

use bento_config::Config;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Padding, Paragraph, Tabs, Wrap};

use crate::app::{App, Modal, TABS, Tab};
use crate::fleet::{self, Fence, Fleet, View};
use crate::host::{Disk, human_bytes, human_duration};
use crate::role::Role;
use crate::systemd::UnitStatus;

const GOOD: Color = Color::Green;
const BAD: Color = Color::Red;
const WARN: Color = Color::Yellow;
const MUTED: Color = Color::DarkGray;

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    match app.tab {
        Tab::Services => draw_services(frame, app, body),
        Tab::Fleet => draw_fleet(frame, app, body),
        Tab::Install => draw_install(frame, app, body),
        Tab::Config => draw_config(frame, app, body),
        Tab::Host => draw_host(frame, app, body),
    }
    draw_footer(frame, app, footer);

    if let Some(modal) = &app.modal {
        draw_modal(frame, modal, app.modal_scroll, frame.area());
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let [title, tabs, right] = Layout::horizontal([
        Constraint::Length(15),
        Constraint::Min(20),
        Constraint::Length(28),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(Line::from(" bento-monitor ".bold())), title);
    frame.render_widget(
        Tabs::new(TABS.to_vec())
            .select(app.tab.index())
            .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).bold())
            .divider(" "),
        tabs,
    );
    let who = if app.euid == 0 {
        Span::styled("root", Style::new().fg(GOOD))
    } else {
        Span::styled("via sudo", Style::new().fg(WARN))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            // The role decides which units this machine is meant to run,
            // so it belongs where it is read on every screen.
            Span::styled(app.role.label(), Style::new().fg(Color::Cyan)),
            Span::styled("  ", Style::new()),
            who,
            Span::raw(" "),
        ]))
        .alignment(Alignment::Right),
        right,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let keys = match app.tab {
        Tab::Services => {
            "s start  t stop  r restart  e enable  d disable  l logs  f follow  D daemon-reload"
        }
        // A runner has no fleet to act on: the controller holds the
        // lease and the slot table (MULTI-NODE 11.3).
        Tab::Fleet if app.role == Role::Runner => "F5 refresh now",
        Tab::Fleet => "s slots  p slot plan for the selected machine  c reconcile",
        Tab::Install => "enter open step  a open every missing step",
        Tab::Config => "e edit  f fetch-images  i images  c reconcile  b backup  r restore",
        Tab::Host => "F5 refresh now",
    };
    let [top, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ", Style::new()),
            Span::styled(keys, Style::new().fg(MUTED)),
        ])),
        top,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" tab/1-5 screens  ? help  q quit  ", Style::new().fg(MUTED)),
            Span::styled(app.status.clone(), Style::new().fg(Color::Cyan)),
        ])),
        bottom,
    );
}

fn draw_services(frame: &mut Frame, app: &App, area: Rect) {
    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Min(30)]).areas(area);

    let mut rows = Vec::new();
    for (index, unit) in app.units.iter().enumerate() {
        let selected = index == app.unit_cursor;
        let (mark, color) = unit_mark(unit);
        let mut style = Style::new();
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        rows.push(Line::from(vec![
            Span::styled(if selected { " > " } else { "   " }, style),
            Span::styled(mark, Style::new().fg(color)),
            Span::styled(format!(" {:<24}", short_name(&unit.name)), style),
            Span::styled(unit_state(unit), Style::new().fg(color)),
        ]));
    }
    if let Some(error) = &app.systemctl_error {
        rows.push(Line::from(""));
        rows.push(Line::from(Span::styled(
            format!(" systemctl: {error}"),
            Style::new().fg(BAD),
        )));
    }
    frame.render_widget(
        Paragraph::new(rows).block(Block::bordered().title(" Units ")),
        list,
    );

    let unit = app.selected_unit();
    let mut lines = vec![
        field("unit", unit.name.clone()),
        field(
            "description",
            if unit.description.is_empty() {
                "-".to_string()
            } else {
                unit.description.clone()
            },
        ),
        field("state", unit_state(unit)),
        field(
            "at boot",
            if unit.file_state.is_empty() {
                "not installed".to_string()
            } else {
                unit.file_state.clone()
            },
        ),
    ];
    if let Some(uptime) = app.host.uptime.and_then(|host| unit.uptime(host)) {
        lines.push(field("active for", human_duration(uptime)));
    }
    if unit.main_pid > 0 {
        lines.push(field("main pid", unit.main_pid.to_string()));
    }
    if let Some(memory) = unit.memory {
        lines.push(field("memory", human_bytes(memory)));
    }
    if let Some(tasks) = unit.tasks {
        lines.push(field("tasks", tasks.to_string()));
    }
    lines.push(field("restarts", unit.restarts.to_string()));
    lines.push(field(
        "unit file",
        if unit.fragment_path.is_empty() {
            "none".to_string()
        } else {
            unit.fragment_path.clone()
        },
    ));
    if unit.failed() {
        lines.push(Line::from(Span::styled(
            format!(" last result: {}", unit.result),
            Style::new().fg(BAD),
        )));
        lines.push(Line::from(Span::styled(
            " press l to read the last 200 log lines",
            Style::new().fg(MUTED),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Detail "))
            .wrap(Wrap { trim: false }),
        detail,
    );
}

/// Every machine of the deployment (MULTI-NODE 20).
///
/// A machine that does not answer keeps its row. It shows the size it
/// last reported and what is provisioned on it, because a runner that
/// vanished from the screen would read as a runner that no longer exists.
fn draw_fleet(frame: &mut Frame, app: &App, area: Rect) {
    match &app.fleet {
        View::Controller(Ok(fleet)) => draw_fleet_table(frame, app, fleet, area),
        View::Controller(Err(error)) => frame.render_widget(note(" Fleet ", error, WARN), area),
        View::Runner(Ok(fence)) => frame.render_widget(runner_widget(fence), area),
        View::Runner(Err(error)) => frame.render_widget(note(" This runner ", error, WARN), area),
    }
}

fn draw_fleet_table(frame: &mut Frame, app: &App, fleet: &Fleet, area: Rect) {
    // Three lines and its border. A short box would cut the row counts,
    // which are the part that says whether anything is misplaced.
    let [summary, body] = Layout::vertical([Constraint::Length(5), Constraint::Min(5)]).areas(area);
    frame.render_widget(deployment_widget(fleet), summary);

    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Min(30)]).areas(body);

    let mut rows = Vec::new();
    for (index, runner) in fleet.runners.iter().enumerate() {
        let selected = index == app.runner_cursor;
        let mut style = Style::new();
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let (mark, color) = health_mark(runner);
        let slots = if runner.slots.is_empty() {
            "no slots".to_string()
        } else {
            format!(
                "slot {}",
                runner
                    .slots
                    .iter()
                    .map(|(slot, _)| slot.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        rows.push(Line::from(vec![
            Span::styled(if selected { " > " } else { "   " }, style),
            Span::styled(mark, Style::new().fg(color)),
            Span::styled(
                format!(" {:<16}", truncate(&runner.name, 16)),
                if runner.is_local {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                },
            ),
            Span::styled(format!("{:<12}", slots), Style::new().fg(MUTED)),
            Span::styled(
                format!("{:>3} vm  ", runner.instances),
                Style::new().fg(MUTED),
            ),
            Span::styled(runner.health.as_str().to_string(), Style::new().fg(color)),
        ]));
    }
    if fleet.runners.is_empty() {
        rows.push(Line::from(Span::styled(
            " no machines registered yet. `bentod serve` writes one row for each\n              [[runners]] entry when it starts.",
            Style::new().fg(WARN),
        )));
    }
    frame.render_widget(
        Paragraph::new(rows)
            .block(Block::bordered().title(" Machines "))
            .wrap(Wrap { trim: false }),
        list,
    );

    let lines = match app.selected_runner() {
        Some(runner) => runner_lines(runner, fleet),
        None => vec![Line::from(Span::styled(
            " nothing to show",
            Style::new().fg(MUTED),
        ))],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Machine "))
            .wrap(Wrap { trim: false }),
        detail,
    );
}

/// The facts that belong to the deployment rather than to one machine:
/// the slot prefix, the controller lease, and the row counts.
fn deployment_widget(fleet: &Fleet) -> Paragraph<'static> {
    let now = time::OffsetDateTime::now_utc();
    let lease = match &fleet.lease {
        Some(lease) => {
            // The lease is renewed on a tick, so an expiry in the past is
            // a control plane that has stopped dispatching, not a clock
            // to read carefully (MULTI-NODE 11.3).
            let (word, color) = if lease.expires_at > now {
                ("holds", GOOD)
            } else {
                ("expired", BAD)
            };
            Line::from(vec![
                Span::styled(" lease         ", Style::new().fg(MUTED)),
                Span::styled(
                    format!("epoch {}  {word}  ", lease.epoch),
                    Style::new().fg(color),
                ),
                Span::styled(
                    format!(
                        "{}  {}",
                        truncate(&lease.holder_id, 12),
                        fleet::until(lease.expires_at, now)
                    ),
                    Style::new().fg(MUTED),
                ),
            ])
        }
        None => Line::from(vec![
            Span::styled(" lease         ", Style::new().fg(MUTED)),
            Span::styled(
                "nobody holds it: no control plane is dispatching",
                Style::new().fg(WARN),
            ),
        ]),
    };
    let mut counts = format!(
        "{}  {}  {}",
        count(fleet.runners.len(), "machine"),
        count(fleet.instances, "instance"),
        count(fleet.images, "image in the allowlist")
    );
    if fleet.orphans > 0 {
        counts.push_str(&format!("  {} on no known machine", fleet.orphans));
    }
    Paragraph::new(vec![
        field(
            "slot prefix",
            format!(
                "/{}  ({} slot(s) per user /24)",
                fleet.deployment.runner_prefix,
                fleet.deployment.slot_count()
            ),
        ),
        lease,
        field("deployment", counts),
    ])
    .block(Block::bordered().title(" Deployment "))
}

fn runner_lines(runner: &fleet::Runner, fleet: &Fleet) -> Vec<Line<'static>> {
    let now = time::OffsetDateTime::now_utc();
    let mut lines = vec![
        // The row id is shown because the controller logs it as
        // `runner_id` on every distributed action (MULTI-NODE 20), and
        // matching a log line to a machine is what an operator does next.
        field(
            "machine",
            if runner.is_local {
                format!("{} (this one), runner_id {}", runner.name, runner.id)
            } else {
                format!("{}, runner_id {}", runner.name, runner.id)
            },
        ),
        field(
            "machine id",
            runner
                .machine_id
                .clone()
                .unwrap_or_else(|| "not reported yet".to_string()),
        ),
        field("endpoint", runner.endpoint_text()),
        field(
            "guest route",
            runner
                .underlay
                .clone()
                .unwrap_or_else(|| "none: no machine can route to this one".to_string()),
        ),
    ];

    let (mark, color) = health_mark(runner);
    lines.push(Line::from(vec![
        Span::styled(" health        ", Style::new().fg(MUTED)),
        Span::styled(
            format!("{mark} {}", runner.health.as_str()),
            Style::new().fg(color),
        ),
        Span::styled(
            format!(
                "  last answered {}",
                fleet::ago(runner.last_contact_at, now)
            ),
            Style::new().fg(MUTED),
        ),
    ]));
    lines.push(match runner.refusal() {
        Some(reason) => Line::from(vec![
            Span::styled(" placement     ", Style::new().fg(MUTED)),
            Span::styled(
                format!("takes nothing new: {reason}"),
                Style::new().fg(WARN),
            ),
        ]),
        None => field("placement", "takes new instances".to_string()),
    });

    // The runner records the highest controller epoch it has accepted. A
    // runner behind the lease has not been reached since the controller
    // last restarted (MULTI-NODE 11.3).
    let epoch = match &fleet.lease {
        Some(lease) if runner.accepted_epoch < lease.epoch => Line::from(vec![
            Span::styled(" epoch         ", Style::new().fg(MUTED)),
            Span::styled(
                format!(
                    "accepted {}, controller is at {}",
                    runner.accepted_epoch, lease.epoch
                ),
                Style::new().fg(WARN),
            ),
        ]),
        _ => field("epoch", format!("accepted {}", runner.accepted_epoch)),
    };
    lines.push(epoch);

    lines.push(field(
        "slots",
        if runner.slots.is_empty() {
            "none: this machine holds no guest addresses".to_string()
        } else {
            runner
                .slots
                .iter()
                .map(|(slot, state)| format!("{slot} ({})", state.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        },
    ));
    lines.push(field(
        "instances",
        format!("{} rows, {} running", runner.instances, runner.running),
    ));
    // Provisioned against what the machine reported. There is no
    // deployment total: memory added across machines that cannot share it
    // describes a machine that does not exist (MULTI-NODE 20).
    lines.push(field(
        "provisioned",
        format!(
            "{} vcpu  {} memory  {} disk",
            runner.vcpu,
            mib(runner.memory_mib),
            gib(runner.disk_gib)
        ),
    ));
    lines.push(field(
        "machine size",
        match (runner.cpu_count, runner.memory_total_mib) {
            (Some(cpus), Some(memory)) => format!(
                "{cpus} cpu  {} memory  {}",
                mib(memory),
                match (runner.storage_available_gib, runner.storage_total_gib) {
                    (Some(free), Some(total)) => format!("{} free of {}", gib(free), gib(total)),
                    _ => "storage unknown".to_string(),
                }
            ),
            _ => "not reported yet".to_string(),
        },
    ));
    lines.push(field(
        "architecture",
        runner
            .arch
            .clone()
            .unwrap_or_else(|| "not reported yet".to_string()),
    ));
    if let Some(version) = &runner.hypervisor_version {
        lines.push(field("hypervisor", version.clone()));
    }
    lines.push(Line::from(vec![
        Span::styled(" images        ", Style::new().fg(MUTED)),
        Span::styled(
            format!("{} of {} ready", runner.images_ready, runner.images_wanted),
            Style::new().fg(if runner.images_ready == runner.images_wanted {
                GOOD
            } else {
                WARN
            }),
        ),
    ]));
    if let Some(error) = &runner.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" last error: {error}"),
            Style::new().fg(BAD),
        )));
    }
    lines
}

/// What a machine with no controller database can say for itself: which
/// controller it is following, and what that controller has had it do.
fn runner_widget(fence: &Fence) -> Paragraph<'static> {
    let now = time::OffsetDateTime::now_utc();
    let mut lines = vec![
        Line::from(Span::styled(
            " This machine holds guests for a controller elsewhere. The fleet, the",
            Style::new().fg(MUTED),
        )),
        Line::from(Span::styled(
            " slots, and the placement rules are the controller's to report.",
            Style::new().fg(MUTED),
        )),
        Line::from(""),
        field(
            "machine id",
            fence
                .machine_id
                .clone()
                .unwrap_or_else(|| "/etc/machine-id could not be read".to_string()),
        ),
        field("listening on", fence.listen.clone()),
        field("fence", fence.fence_db.clone()),
        field("epoch", format!("accepted {}", fence.accepted_epoch)),
        field("objects", fence.objects.to_string()),
        field(
            "changes",
            format!(
                "{} recorded, last {}",
                fence.outcomes,
                fleet::ago(fence.last_change_at, now)
            ),
        ),
    ];
    if fence.accepted_epoch == 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " No controller has changed anything here yet. Until one does, this\n              machine has accepted no epoch and holds no guests it was told to build.",
            Style::new().fg(WARN),
        )));
    }
    Paragraph::new(lines)
        .block(Block::bordered().title(" This runner "))
        .wrap(Wrap { trim: false })
}

fn note(title: &'static str, body: &str, color: Color) -> Paragraph<'static> {
    Paragraph::new(vec![Line::from(Span::styled(
        format!(" {body}"),
        Style::new().fg(color),
    ))])
    .block(Block::bordered().title(title))
    .wrap(Wrap { trim: false })
}

fn health_mark(runner: &fleet::Runner) -> (&'static str, Color) {
    use bento_store::HostHealth;
    if runner.refusal().is_some() && runner.health == HostHealth::Ok {
        // Reachable, but drained or disabled by an operator.
        return ("[-]", WARN);
    }
    match runner.health {
        HostHealth::Ok => ("[*]", GOOD),
        HostHealth::Unknown => ("[ ]", MUTED),
        HostHealth::Unreachable => ("[!]", BAD),
        HostHealth::Mismatched => ("[!]", BAD),
    }
}

/// A count and the thing counted, in the number the count calls for.
fn count(number: usize, thing: &str) -> String {
    if number == 1 {
        format!("{number} {thing}")
    } else {
        // Every noun this is used with takes a plain -s, and the phrase
        // that is not one word is written so its first word takes it.
        match thing.split_once(' ') {
            Some((head, rest)) => format!("{number} {head}s {rest}"),
            None => format!("{number} {thing}s"),
        }
    }
}

fn mib(value: u64) -> String {
    human_bytes(value.saturating_mul(1024 * 1024))
}

fn gib(value: u64) -> String {
    human_bytes(value.saturating_mul(1024 * 1024 * 1024))
}

/// A name cut to fit its column, with the cut marked.
fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let kept: String = value.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

fn draw_install(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    for (index, step) in app.steps.iter().enumerate() {
        let selected = index == app.step_cursor;
        let mut style = Style::new();
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let (mark, color) = if step.done {
            ("[done]   ", GOOD)
        } else if step.blocked.is_some() {
            ("[waits]  ", MUTED)
        } else {
            ("[missing]", WARN)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { " > " } else { "   " }, style),
            Span::styled(mark, Style::new().fg(color)),
            // Wide enough for the longest title, so a step never runs
            // into its own detail.
            Span::styled(format!(" {:<26}", step.title), style),
            Span::styled(step.detail.clone(), Style::new().fg(MUTED)),
        ]));
        if let Some(reason) = &step.blocked {
            lines.push(Line::from(Span::styled(
                format!("                   {reason}"),
                Style::new().fg(MUTED),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("   binary   {}", app.paths.binary.display()),
        Style::new().fg(MUTED),
    )));
    lines.push(Line::from(Span::styled(
        format!("   monitor  {}", app.paths.monitor.display()),
        Style::new().fg(MUTED),
    )));
    lines.push(Line::from(Span::styled(
        format!("   config   {}", app.paths.config.display()),
        Style::new().fg(MUTED),
    )));
    lines.push(Line::from(Span::styled(
        match &app.paths.source {
            Some(source) => format!("   source   {}", source.display()),
            None => "   source   none found; --source names the tree to build from".to_string(),
        },
        Style::new().fg(MUTED),
    )));
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Install ")),
        area,
    );
}

fn draw_config(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(format!(" {} ", app.paths.config.display()));
    let lines = match &app.config {
        Err(error) => vec![
            Line::from(Span::styled(" does not load", Style::new().fg(BAD))),
            Line::from(""),
            Line::from(Span::raw(format!(" {error}"))),
            Line::from(""),
            Line::from(Span::styled(
                " press e to edit it, or install it from the Install tab",
                Style::new().fg(MUTED),
            )),
        ],
        Ok(config) => config_lines(config),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The settings an operator checks, and nothing that carries a secret: the
/// OIDC client secret is reported as set or missing, never printed.
fn config_lines(config: &Config) -> Vec<Line<'static>> {
    let mut lines = vec![
        field("base domain", config.base_domain.clone()),
        field("libvirt", config.libvirt_uri.clone()),
        field(
            "listen",
            format!(
                "http {}  https {}  ssh {}  ports {}-{}",
                config.listen.http,
                config.listen.https,
                config.listen.ssh,
                config.listen.proxy_port_min,
                config.listen.proxy_port_max
            ),
        ),
        field("database", config.db_path.clone()),
        field("images", config.image_dir.clone()),
        field("storage", config.storage_dir.clone()),
        field("keys", config.key_dir.clone()),
        field("overcommit", format!("{:.2}", config.overcommit_ratio)),
        field("name cooldown", human_duration(config.name_cooldown.0)),
        field("restore batch", config.restore_batch_size.to_string()),
        field("private range", config.private_range.to_string()),
        field(
            "operators",
            if config.operators.is_empty() {
                "none".to_string()
            } else {
                config.operators.join(", ")
            },
        ),
        field(
            "oidc issuer",
            or_none(config.oidc.issuer.as_deref().unwrap_or_default()),
        ),
        field("oidc client", or_none(&config.oidc.client_id)),
        field("oidc secret", present(&config.oidc.client_secret)),
        field("oidc signup", config.oidc.allow_signup.to_string()),
    ];
    if let Some(builder) = &config.bootc.builder_image {
        lines.push(field("bootc builder", builder.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" allowlist ({})", config.images.len()),
        Style::new().add_modifier(Modifier::BOLD),
    )));
    for image in &config.images {
        let source = match &image.source {
            bento_config::ImageSource::Url(url) => url.clone(),
            bento_config::ImageSource::Oci(oci) => format!("oci {oci}"),
        };
        let pin = match &image.pinned_checksum {
            Some(_) => " (pinned)",
            None => "",
        };
        lines.push(Line::from(vec![
            Span::raw(format!("   {:<18}", image.name)),
            Span::styled(format!("{source}{pin}"), Style::new().fg(MUTED)),
        ]));
    }
    lines
}

fn draw_host(frame: &mut Frame, app: &App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    let block = Block::bordered().title(" Host ");
    let inner = block.inner(left);
    frame.render_widget(block, left);
    // Every gauge is one row. The checks take what is left, so that a tall
    // terminal does not stretch the storage bar down the whole pane.
    let [facts, cpu, memory, swap, images, storage, checks] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(inner);

    let load = match app.host.load {
        Some(load) => format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2]),
        None => "unknown".to_string(),
    };
    frame.render_widget(
        Paragraph::new(vec![
            field("cores", app.host.cores.to_string()),
            field("load", load),
            field(
                "up",
                app.host
                    .uptime
                    .map_or("unknown".to_string(), human_duration),
            ),
        ]),
        facts,
    );
    gauge(
        frame,
        cpu,
        "cpu    ",
        app.host.busy.unwrap_or(0.0),
        match app.host.busy {
            Some(busy) => format!("{:.0}%", busy * 100.0),
            None => "measuring".to_string(),
        },
    );
    gauge(
        frame,
        memory,
        "memory ",
        ratio(app.host.memory.used(), app.host.memory.total),
        format!(
            "{} of {} used",
            human_bytes(app.host.memory.used()),
            human_bytes(app.host.memory.total)
        ),
    );
    gauge(
        frame,
        swap,
        "swap   ",
        ratio(app.host.memory.swap_used(), app.host.memory.swap_total),
        if app.host.memory.swap_total == 0 {
            "none".to_string()
        } else {
            format!(
                "{} of {} used",
                human_bytes(app.host.memory.swap_used()),
                human_bytes(app.host.memory.swap_total)
            )
        },
    );
    disk_gauge(frame, images, "images ", app.host.image_disk);
    disk_gauge(frame, storage, "storage", app.host.storage_disk);

    frame.render_widget(checks_widget(app), checks);
    // The domain list fills the second column, less the border and the
    // four counts above it.
    frame.render_widget(
        census_widget(app, right.height.saturating_sub(6) as usize),
        right,
    );
}

fn census_widget(app: &App, room: usize) -> Paragraph<'static> {
    let lines = match &app.census {
        Ok(report) => {
            let mut lines = vec![
                field("domains", report.total().to_string()),
                field("running", report.running.to_string()),
                field("starting", report.starting.to_string()),
                field("stopped", report.stopped.to_string()),
            ];
            // As many names as the pane holds. A truncated list says how
            // much it left out, so that the count above it stays the
            // number to trust.
            let shown = if report.names.len() > room {
                room.saturating_sub(1)
            } else {
                report.names.len()
            };
            for (name, state) in report.names.iter().take(shown) {
                lines.push(Line::from(Span::styled(
                    format!("   {name} ({state})"),
                    Style::new().fg(MUTED),
                )));
            }
            if shown < report.names.len() {
                lines.push(Line::from(Span::styled(
                    format!("   and {} more", report.names.len() - shown),
                    Style::new().fg(MUTED),
                )));
            }
            lines
        }
        Err(error) => vec![Line::from(Span::styled(
            format!(" {error}"),
            Style::new().fg(WARN),
        ))],
    };
    Paragraph::new(lines)
        .block(Block::bordered().title(" libvirt "))
        .wrap(Wrap { trim: false })
}

fn checks_widget(app: &App) -> Paragraph<'static> {
    if app.checks.is_empty() {
        return Paragraph::new(Line::from(Span::styled(
            " no configuration, so nothing to check",
            Style::new().fg(MUTED),
        )))
        .block(Block::bordered().title(" Requirements (SPEC 4.2) "));
    }
    let lines = app
        .checks
        .iter()
        .map(|check| {
            let (mark, color) = match (check.ok, check.fatal) {
                (true, _) => ("ok  ", GOOD),
                (false, true) => ("fail", BAD),
                (false, false) => ("warn", WARN),
            };
            Line::from(vec![
                Span::raw(" "),
                Span::styled(mark, Style::new().fg(color)),
                Span::raw(format!(" {:<30} ", check.name)),
                Span::styled(check.detail.clone(), Style::new().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines)
        .block(Block::bordered().title(" Requirements (SPEC 4.2) "))
        .wrap(Wrap { trim: false })
}

/// What a modal shows: the wrapped body, which scrolls, and the footer,
/// which does not.
struct ModalLayout {
    popup: Rect,
    title: String,
    body: Vec<Line<'static>>,
    /// How many body lines the popup shows at once.
    view: u16,
    /// How far the body scrolls: the first line of the last page.
    max_scroll: u16,
}

/// Lays a modal out on `area`.
///
/// The body is wrapped here, by hand, rather than by the widget: a
/// widget that wraps cannot say how many lines it made, and a prompt
/// that is taller than the screen would then cut off its last commands
/// with no way to know. With the lines counted, the popup scrolls, and
/// the caller can tell when the operator has seen the last one.
fn modal_layout(modal: &Modal, area: Rect) -> ModalLayout {
    let width = area.width.saturating_sub(8).clamp(20, 96);
    // Two columns of border and two of padding.
    let inner = usize::from(width.saturating_sub(4)).max(1);
    let (title, body) = match modal {
        Modal::Help => (" Help ".to_string(), plain(&help_text(), inner)),
        Modal::Message { title, body } => (format!(" {title} "), plain(body, inner)),
        Modal::Confirm { title, commands } => {
            let mut lines: Vec<Line<'static>> = plain(&format!("{title}:"), inner);
            lines.push(Line::from(""));
            for command in commands {
                for (index, part) in wrap(&command.display(), inner.saturating_sub(4).max(1))
                    .into_iter()
                    .enumerate()
                {
                    let lead = if index == 0 { "  $ " } else { "    " };
                    lines.push(Line::from(vec![
                        Span::styled(lead, Style::new().fg(MUTED)),
                        Span::styled(part, Style::new().fg(Color::Cyan)),
                    ]));
                }
                if let Some(dir) = &command.dir {
                    for part in wrap(&format!("      in {dir}"), inner) {
                        lines.push(Line::from(Span::styled(part, Style::new().fg(MUTED))));
                    }
                }
            }
            (" Run this? ".to_string(), lines)
        }
    };
    // Borders, the blank line, and the footer take four rows.
    let height = (body.len() as u16)
        .saturating_add(4)
        .min(area.height.saturating_sub(2))
        .max(5);
    let view = height - 4;
    let max_scroll = (body.len() as u16).saturating_sub(view);
    ModalLayout {
        popup: centered(area, width, height),
        title,
        body,
        view,
        max_scroll,
    }
}

/// How far a modal scrolls on a screen of this size, and how many lines
/// a page is. Zero means it fits.
pub fn modal_scroll_limit(modal: &Modal, screen: Rect) -> (u16, u16) {
    let layout = modal_layout(modal, screen);
    (layout.max_scroll, layout.view.max(1))
}

fn plain(text: &str, width: usize) -> Vec<Line<'static>> {
    wrap(text, width).into_iter().map(Line::from).collect()
}

/// Wraps `text` at `width` characters. Words break at spaces; a word that
/// is longer than a line breaks where the line ends. Leading spaces carry
/// to every line made from that one, so an indented command stays
/// indented.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let lead = line.len() - line.trim_start_matches(' ').len();
        let room = width.saturating_sub(lead).max(1);
        let mut current = String::new();
        let mut made = Vec::new();
        for mut word in line.split_whitespace() {
            loop {
                let used = current.chars().count();
                let size = word.chars().count();
                if used == 0 && size <= room {
                    current.push_str(word);
                    break;
                } else if used == 0 {
                    let cut = word.char_indices().nth(room).map_or(word.len(), |(i, _)| i);
                    made.push(word[..cut].to_string());
                    word = &word[cut..];
                } else if used + 1 + size <= room {
                    current.push(' ');
                    current.push_str(word);
                    break;
                } else {
                    made.push(std::mem::take(&mut current));
                }
            }
        }
        if !current.is_empty() || made.is_empty() {
            made.push(current);
        }
        let indent = " ".repeat(lead.min(width.saturating_sub(1)));
        lines.extend(made.into_iter().map(|part| format!("{indent}{part}")));
    }
    lines
}

fn draw_modal(frame: &mut Frame, modal: &Modal, scroll: u16, area: Rect) {
    let layout = modal_layout(modal, area);
    let scroll = scroll.min(layout.max_scroll);
    let more = scroll < layout.max_scroll;
    let footer = match (modal, more, layout.max_scroll > 0) {
        (Modal::Confirm { .. }, true, _) => "more below: j/k or space scrolls. y works at the end",
        (Modal::Confirm { .. }, false, _) => "y runs it in this terminal, any other key cancels",
        (_, _, true) => "j/k or space scrolls, any other key closes",
        _ => "any key closes",
    };
    frame.render_widget(Clear, layout.popup);
    let block = Block::bordered()
        .title(layout.title)
        .padding(Padding::horizontal(1))
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(layout.popup);
    frame.render_widget(block, layout.popup);
    let [body, _, footer_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(layout.body).scroll((scroll, 0)), body);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, Style::new().fg(MUTED)))),
        footer_area,
    );
}

fn help_text() -> String {
    [
        "bento-monitor drives the systemd units of one Bento machine and",
        "reports the whole deployment. It runs no privileged step by itself:",
        "each one is shown first and then runs in this terminal, so sudo can",
        "ask for a password.",
        "",
        "The header names this machine's role. A controller runs serve, the",
        "proxy, the SSH frontend, and its own runner; a runner runs the runner",
        "service alone (MULTI-NODE 19). Each screen counts only the units the",
        "role calls for.",
        "",
        "  tab / shift-tab / 1-5   move between screens",
        "  up down j k             move inside a screen",
        "  F5                      reread the host now (it also rereads every 2s)",
        "",
        "Services  s start  t stop  r restart  e enable  d disable",
        "          l last 200 log lines  f follow the log  D daemon-reload",
        "Fleet     s slots  p slot plan for the selected machine  c reconcile",
        "          It reads what the controller recorded; it never calls a",
        "          runner, because only the controller may hold the lease.",
        "Install   enter open the selected step  a open every missing step",
        "Config    e edit  f fetch-images  i images  c reconcile  b backup  r restore",
        "",
        "A prompt shows the commands first. y runs them. Any other key",
        "cancels. A long prompt scrolls (j k space), and y works only after",
        "its last line was shown. Enter never confirms.",
        "",
        "  q  quit",
    ]
    .join("\n")
}

fn gauge(frame: &mut Frame, area: Rect, label: &str, ratio: f64, text: String) {
    let [name, bar] = Layout::horizontal([Constraint::Length(8), Constraint::Min(10)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::raw(label.to_string()))),
        name,
    );
    let color = if ratio >= 0.9 {
        BAD
    } else if ratio >= 0.75 {
        WARN
    } else {
        GOOD
    };
    frame.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .label(text)
            .gauge_style(Style::new().fg(color))
            .use_unicode(true),
        bar,
    );
}

fn disk_gauge(frame: &mut Frame, area: Rect, label: &str, disk: Option<Disk>) {
    match disk {
        Some(disk) => gauge(
            frame,
            area,
            label,
            ratio(disk.used(), disk.total),
            format!(
                "{} free of {}",
                human_bytes(disk.available),
                human_bytes(disk.total)
            ),
        ),
        None => frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(label.to_string()),
                Span::styled(" directory is missing", Style::new().fg(WARN)),
            ])),
            area,
        ),
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 / whole as f64).clamp(0.0, 1.0)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn field(name: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {name:<14}"), Style::new().fg(MUTED)),
        Span::raw(value),
    ])
}

fn or_none(value: &str) -> String {
    if value.is_empty() {
        "none".to_string()
    } else {
        value.to_string()
    }
}

fn present(value: &str) -> String {
    if value.is_empty() {
        "missing".to_string()
    } else {
        "set".to_string()
    }
}

/// The unit name without the `.service` suffix every one of them carries.
fn short_name(name: &str) -> String {
    name.strip_suffix(".service").unwrap_or(name).to_string()
}

fn unit_mark(unit: &UnitStatus) -> (&'static str, Color) {
    if !unit.wanted && !unit.installed() {
        // Not missing: this machine's role does not run it.
        ("[-]", MUTED)
    } else if !unit.installed() {
        ("[ ]", MUTED)
    } else if unit.failed() {
        ("[!]", BAD)
    } else if unit.running() {
        ("[*]", GOOD)
    } else {
        ("[ ]", WARN)
    }
}

fn unit_state(unit: &UnitStatus) -> String {
    if !unit.installed() {
        return if unit.wanted {
            "not installed".to_string()
        } else {
            "not run on this machine".to_string()
        };
    }
    if !unit.wanted {
        // Installed where the role does not call for it. Worth seeing:
        // two control planes on one deployment fence each other out
        // (MULTI-NODE 11.3).
        return format!("{} (not for this machine)", unit.active_state);
    }
    if unit.sub_state.is_empty() {
        return unit.active_state.clone();
    }
    format!("{} ({})", unit.active_state, unit.sub_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemd::SERVE;

    #[test]
    fn a_unit_reads_as_its_state() {
        let mut unit = UnitStatus {
            name: SERVE.to_string(),
            wanted: true,
            ..Default::default()
        };
        assert_eq!(unit_state(&unit), "not installed");
        unit.load_state = "loaded".to_string();
        unit.fragment_path = "/etc/systemd/system/bentod-serve.service".to_string();
        unit.active_state = "active".to_string();
        unit.sub_state = "running".to_string();
        assert_eq!(unit_state(&unit), "active (running)");
        assert_eq!(unit_mark(&unit).0, "[*]");
        assert_eq!(short_name(&unit.name), "bentod-serve");
    }

    #[test]
    fn a_unit_the_role_does_not_run_reads_as_that_and_not_as_missing() {
        // A runner is not a half-installed controller (MULTI-NODE 19).
        let mut unit = UnitStatus {
            name: SERVE.to_string(),
            wanted: false,
            ..Default::default()
        };
        assert_eq!(unit_state(&unit), "not run on this machine");
        assert_eq!(unit_mark(&unit).0, "[-]");

        // One that is nonetheless running here is called out rather than
        // shown as an ordinary healthy unit.
        unit.load_state = "loaded".to_string();
        unit.fragment_path = "/etc/systemd/system/bentod-serve.service".to_string();
        unit.active_state = "active".to_string();
        assert_eq!(unit_state(&unit), "active (not for this machine)");
    }

    #[test]
    fn a_long_machine_name_is_cut_and_the_cut_is_marked() {
        assert_eq!(truncate("konata", 16), "konata");
        assert_eq!(truncate("runner-a.example.org", 10), "runner-a.\u{2026}");
    }

    #[test]
    fn a_count_reads_in_the_number_it_calls_for() {
        assert_eq!(count(1, "machine"), "1 machine");
        assert_eq!(count(2, "machine"), "2 machines");
        assert_eq!(count(0, "instance"), "0 instances");
        assert_eq!(
            count(1, "image in the allowlist"),
            "1 image in the allowlist"
        );
        assert_eq!(
            count(3, "image in the allowlist"),
            "3 images in the allowlist"
        );
    }

    #[test]
    fn reserved_sizes_read_in_the_units_libvirt_reports_them_in() {
        assert_eq!(mib(2048), "2.0 GiB");
        assert_eq!(gib(20), "20.0 GiB");
        // A machine that reported nothing reads as zero.
        assert_eq!(mib(0), "0 B");
    }

    #[test]
    fn a_secret_is_reported_as_set_and_never_printed() {
        let mut config = Config::default();
        config.oidc.client_secret = "oidc-secret-value".to_string();
        let text: String = config_lines(&config)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(!text.contains("secret-value"), "{text}");
        assert!(text.contains("set"));
    }

    #[test]
    fn an_empty_filesystem_reading_does_not_divide_by_zero() {
        assert_eq!(ratio(5, 0), 0.0);
        assert_eq!(ratio(5, 10), 0.5);
        // A used count above the total, as a reserve makes possible.
        assert_eq!(ratio(20, 10), 1.0);
    }

    #[test]
    fn the_popup_stays_inside_a_small_terminal() {
        let area = Rect::new(0, 0, 20, 6);
        let popup = centered(area, 96, 40);
        assert!(popup.width <= area.width && popup.height <= area.height);
        assert_eq!(popup.x, 0);
    }

    fn long_prompt() -> Modal {
        let commands = (0..11)
            .map(|index| {
                crate::run::Cmd::owned(
                    "sudo",
                    vec![
                        "install".to_string(),
                        "-m".to_string(),
                        "0644".to_string(),
                        format!("/tmp/bento-monitor-AbCdEf/unit-{index}.service.staged"),
                        format!("/etc/systemd/system/unit-{index}.service"),
                        format!("marker-{index}"),
                    ],
                )
            })
            .collect();
        Modal::Confirm {
            title: "install everything that is missing".to_string(),
            commands,
        }
    }

    fn screen(modal: &Modal, scroll: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| draw_modal(frame, modal, scroll, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_long_prompt_scrolls_and_shows_its_last_command_and_footer_at_the_end() {
        let modal = long_prompt();
        let (max, page) = modal_scroll_limit(&modal, Rect::new(0, 0, 80, 24));
        assert!(max > 0, "eleven long commands do not fit on 80x24");
        assert!(page > 0);

        let top = screen(&modal, 0);
        assert!(!top.contains("marker-10"), "{top}");
        assert!(top.contains("more below"), "{top}");

        let end = screen(&modal, max);
        assert!(end.contains("marker-10"), "{end}");
        assert!(end.contains("y runs it in this terminal"), "{end}");

        // Every command is on some page.
        let mut seen = String::new();
        for scroll in 0..=max {
            seen.push_str(&screen(&modal, scroll));
        }
        for index in 0..11 {
            assert!(seen.contains(&format!("marker-{index}")), "{index}");
        }
    }

    #[test]
    fn a_prompt_that_fits_has_nothing_to_scroll() {
        let modal = Modal::Message {
            title: "x".to_string(),
            body: "short".to_string(),
        };
        assert_eq!(modal_scroll_limit(&modal, Rect::new(0, 0, 80, 24)).0, 0);
    }

    #[test]
    fn wrapping_keeps_the_indent_and_breaks_a_word_longer_than_a_line() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("  aaa bbb", 6), ["  aaa", "  bbb"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
    }
}
