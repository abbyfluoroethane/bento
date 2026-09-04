//! The screen. Every function here reads [`App`] and writes widgets; none
//! of them touches the host.

use bento_config::Config;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Padding, Paragraph, Tabs, Wrap};

use crate::app::{App, Modal, TABS, Tab};
use crate::host::{Disk, human_bytes, human_duration};
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
        Tab::Install => draw_install(frame, app, body),
        Tab::Config => draw_config(frame, app, body),
        Tab::Host => draw_host(frame, app, body),
    }
    draw_footer(frame, app, footer);

    if let Some(modal) = &app.modal {
        draw_modal(frame, modal, frame.area());
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let [title, tabs, right] = Layout::horizontal([
        Constraint::Length(15),
        Constraint::Min(20),
        Constraint::Length(18),
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
        Paragraph::new(Line::from(who)).alignment(Alignment::Right),
        right,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let keys = match app.tab {
        Tab::Services => {
            "s start  t stop  r restart  e enable  d disable  l logs  f follow  D daemon-reload"
        }
        Tab::Install => "enter run step  a run every missing step",
        Tab::Config => "e edit  f fetch-images  i images  c reconcile",
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
            Span::styled(" tab/1-4 screens  ? help  q quit  ", Style::new().fg(MUTED)),
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
            Span::styled(format!(" {:<22}", step.title), style),
            Span::styled(step.detail.clone(), Style::new().fg(MUTED)),
        ]));
        if let Some(reason) = &step.blocked {
            lines.push(Line::from(Span::styled(
                format!("               {reason}"),
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
/// ACME token and the OIDC client secret are reported as set or missing,
/// never printed.
fn config_lines(config: &Config) -> Vec<Line<'static>> {
    let mut lines = vec![
        field("base domain", config.base_domain.clone()),
        field("libvirt", config.libvirt_uri.clone()),
        field("tls", config.listen.tls.as_str().to_string()),
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
        field("private range", config.private_range.clone()),
        field(
            "operators",
            if config.operators.is_empty() {
                "none".to_string()
            } else {
                config.operators.join(", ")
            },
        ),
        field("acme email", or_none(&config.acme.email)),
        field("acme token", present(&config.acme.cloudflare_token)),
        field("oidc issuer", or_none(&config.oidc.issuer)),
        field("oidc client", or_none(&config.oidc.client_id)),
        field("oidc secret", present(&config.oidc.client_secret)),
        field("oidc signup", config.oidc.allow_signup.to_string()),
    ];
    if !config.bootc.builder_image.is_empty() {
        lines.push(field("bootc builder", config.bootc.builder_image.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" allowlist ({})", config.images.len()),
        Style::new().add_modifier(Modifier::BOLD),
    )));
    for image in &config.images {
        let source = if image.oci.is_empty() {
            image.url.clone()
        } else {
            format!("oci {}", image.oci)
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

fn draw_modal(frame: &mut Frame, modal: &Modal, area: Rect) {
    let (title, mut lines, footer) = match modal {
        Modal::Help => (
            " Help ".to_string(),
            help_lines(),
            "any key closes".to_string(),
        ),
        Modal::Message { title, body } => (
            format!(" {title} "),
            vec![Line::from(Span::raw(body.clone()))],
            "any key closes".to_string(),
        ),
        Modal::Confirm { title, commands } => {
            let mut lines = vec![Line::from(Span::raw(format!("{title}:"))), Line::from("")];
            for command in commands {
                lines.push(Line::from(vec![
                    Span::styled("  $ ", Style::new().fg(MUTED)),
                    Span::styled(command.display(), Style::new().fg(Color::Cyan)),
                ]));
                if let Some(dir) = &command.dir {
                    lines.push(Line::from(Span::styled(
                        format!("      in {dir}"),
                        Style::new().fg(MUTED),
                    )));
                }
            }
            (
                " Run this? ".to_string(),
                lines,
                "y or enter runs it in this terminal, any other key cancels".to_string(),
            )
        }
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(footer, Style::new().fg(MUTED))));

    let height = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);
    let width = area.width.saturating_sub(8).clamp(20, 96);
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(title)
                    .padding(Padding::horizontal(1))
                    .border_style(Style::new().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn help_lines() -> Vec<Line<'static>> {
    [
        "bento-monitor drives the three systemd units of a Bento host.",
        "It runs no privileged step by itself: each one is shown first and",
        "then runs in this terminal, so sudo can ask for a password.",
        "",
        "  tab / shift-tab / 1-4   move between screens",
        "  up down j k             move inside a screen",
        "  F5                      reread the host now (it also rereads every 2s)",
        "",
        "Services  s start  t stop  r restart  e enable  d disable",
        "          l last 200 log lines  f follow the log  D daemon-reload",
        "Install   enter run the selected step  a run every missing step",
        "Config    e edit  f fetch-images  i images  c reconcile",
        "",
        "  q  quit",
    ]
    .iter()
    .map(|line| Line::from(Span::raw(*line)))
    .collect()
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
    if !unit.installed() {
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
        return "not installed".to_string();
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
    fn a_secret_is_reported_as_set_and_never_printed() {
        let mut config = Config::default();
        config.acme.cloudflare_token = "cf-secret-value".to_string();
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
}
