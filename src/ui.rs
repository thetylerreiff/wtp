//! Rendering: a memory headline and bar, then
//! two-line server rows with a colored port, a sparkline and memory.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Confirm, Page, Tone};
use crate::format;
use crate::scan::{Server, Status};

const MAX_WIDTH: u16 = 88;

/// Port identity colors from the app's palette. They avoid the red, amber and
/// green status families.
const PORTS: [Color; 7] = [
    Color::Rgb(0x6E, 0xC7, 0xED), // sky
    Color::Rgb(0xB5, 0x99, 0xF0), // lavender
    Color::Rgb(0xEB, 0x9C, 0xD4), // pink
    Color::Rgb(0x7D, 0x9C, 0xF2), // periwinkle
    Color::Rgb(0x7D, 0xDB, 0xE0), // cyan
    Color::Rgb(0xD9, 0xA3, 0xF2), // lilac
    Color::Rgb(0xAB, 0xC2, 0xE0), // slate
];
const AMBER: Color = Color::Rgb(0xFF, 0xB2, 0x24);
const RED: Color = Color::Rgb(0xFF, 0x69, 0x61);
const GREEN: Color = Color::Rgb(0x6C, 0xD4, 0x8C);

fn text1() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}
fn text2() -> Style {
    Style::new()
}
fn text3() -> Style {
    Style::new().fg(Color::DarkGray)
}
fn port_style(app: &App, port: u16) -> Style {
    Style::new().fg(PORTS[app.color_index(port) % PORTS.len()])
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let full = frame.area();
    let width = full.width.min(MAX_WIDTH);
    let area = Rect { x: full.x + (full.width - width) / 2, width, ..full };
    let inner = Rect { x: area.x + 2, width: area.width.saturating_sub(4), ..area };

    match app.page {
        Page::List => draw_list(frame, app, inner),
        Page::Detail(port) => match app.servers.iter().find(|s| s.port == port).cloned() {
            Some(server) => draw_detail(frame, app, &server, inner),
            None => draw_list(frame, app, inner),
        },
        Page::Help => draw_help(frame, app, inner),
    }
}

// MARK: Shared pieces

/// Left spans, then right spans flush to the right edge; the left side is
/// truncated when they don't both fit.
fn split<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: u16) -> Line<'a> {
    let right_width: usize = right.iter().map(Span::width).sum();
    let mut spans = fit(left, (width as usize).saturating_sub(right_width + 1));
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(" ".repeat((width as usize).saturating_sub(used + right_width))));
    spans.extend(right);
    Line::from(spans)
}

/// Truncates spans to `width` columns with an ellipsis.
fn fit(spans: Vec<Span<'_>>, width: usize) -> Vec<Span<'_>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = span.width();
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let room = width.saturating_sub(used + 1);
        let mut text = String::new();
        for c in span.content.chars() {
            if text.width() + c.to_string().width() > room {
                break;
            }
            text.push(c);
        }
        if width > used {
            out.push(Span::styled(text + "…", span.style));
        }
        break;
    }
    out
}

fn centered<'a>(spans: Vec<Span<'a>>) -> Line<'a> {
    Line::from(spans).centered()
}

fn divider(width: u16) -> Line<'static> {
    Line::styled("─".repeat(width as usize), text3())
}

/// Key hints; lower-priority hints are dropped when the line is too narrow.
fn hints(pairs: &[(&str, &str)], width: u16, destructive: Option<&str>) -> Line<'static> {
    let mut kept: Vec<(&str, &str)> = pairs.to_vec();
    let total = |kept: &[(&str, &str)]| kept.iter().map(|(k, l)| k.width() + l.width() + 4).sum::<usize>();
    // Drop from the middle-right, but always keep help and quit.
    while total(&kept) > width as usize && kept.len() > 2 {
        let index = kept.len().saturating_sub(3);
        kept.remove(index);
    }
    let mut spans = Vec::new();
    for (i, (key, label)) in kept.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        let danger = destructive == Some(*key);
        spans.push(Span::styled(key.to_string(), if danger { Style::new().fg(RED).bold() } else { text1() }));
        spans.push(Span::styled(format!(" {label}"), if danger { Style::new().fg(RED) } else { text3() }));
    }
    Line::from(spans)
}

/// The footer: a pending confirmation, the filter being typed, a recent
/// message, or key hints — in that order.
fn footer(app: &App, fallback: Line<'static>, width: u16) -> Line<'static> {
    if let Some(confirm) = &app.confirm {
        let (question, verb) = confirm_text(app, confirm);
        let keys = vec![
            Span::styled("y", Style::new().fg(RED).bold()),
            Span::styled(format!(" {verb}   "), Style::new().fg(RED)),
            Span::styled("n", text1()),
            Span::styled(" Cancel", text3()),
        ];
        return split(vec![Span::styled(question, text1())], keys, width);
    }
    if app.filtering {
        return Line::from(vec![
            Span::styled("/", text1()),
            Span::raw(app.filter.clone()),
            Span::styled("▏", text3()),
            Span::styled("   ⏎ Done   esc Clear", text3()),
        ]);
    }
    if let Some(toast) = &app.toast {
        let color = match toast.tone {
            Tone::Info => Style::new(),
            Tone::Good => Style::new().fg(GREEN),
            Tone::Bad => Style::new().fg(RED),
        };
        return Line::from(fit(vec![Span::styled(toast.text.clone(), color)], width as usize));
    }
    fallback
}

fn confirm_text(app: &App, confirm: &Confirm) -> (String, &'static str) {
    let find = |port: &u16| app.servers.iter().find(|s| s.port == *port);
    match confirm {
        Confirm::Stop { targets, force } => {
            let ports: Vec<u16> = targets.iter().map(|(port, _)| *port).collect();
            let verb = if *force { "Kill" } else { "Stop" };
            let question = match ports.as_slice() {
                [port] => {
                    let server = find(port);
                    let count = server.map_or(1, |s| s.starts.len());
                    let name = server.map(|s| s.title()).unwrap_or_default();
                    let protected = if server.is_some_and(|s| s.protected) { " (protected)" } else { "" };
                    // One process can hold several ports (Docker's port forwards, for one).
                    let siblings: Vec<String> = server
                        .map(|s| app.servers.iter().filter(|o| o.root_pid == s.root_pid && o.port != s.port).map(|o| format!(":{}", o.port)).collect())
                        .unwrap_or_default();
                    let also = match siblings.len() {
                        0 => String::new(),
                        1..=3 => format!(" · also closes {}", siblings.join(" ")),
                        n => format!(" · also closes {} +{}", siblings[..2].join(" "), n - 2),
                    };
                    format!("{verb} :{port}{protected} {name} · {count} {}{also}?", plural(count, "process", "processes"))
                }
                many => {
                    let memory = memory_of(many.iter().filter_map(find));
                    format!("{verb} {} servers · free {}?", many.len(), format::bytes(memory))
                }
            };
            (question, verb)
        }
        Confirm::Restart(port, _) => {
            let agent = find(port).and_then(|s| s.agent.as_ref()).map(|a| format!(" · runs outside {}", a.kind.label())).unwrap_or_default();
            (format!("Restart :{port}{agent}?"), "Restart")
        }
    }
}

/// Total memory, counting each process tree once: one process can hold
/// several ports (Docker's port forwards, for one).
fn memory_of<'a>(servers: impl IntoIterator<Item = &'a Server>) -> u64 {
    let mut seen = std::collections::HashSet::new();
    servers.into_iter().filter(|s| seen.insert(s.root_pid)).map(|s| s.memory).sum()
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

fn port_label(app: &App, server: &Server) -> Vec<Span<'static>> {
    let style = port_style(app, server.port);
    let colon = match server.status() {
        Status::Running => style,
        Status::Attention => style.bold(),
        Status::Idle => style.add_modifier(Modifier::DIM),
    };
    vec![Span::styled(":", colon), Span::styled(server.port.to_string(), style.bold())]
}

// MARK: List

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let [top, body, _, bottom] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Paragraph::new(list_header(app, top.width)), top);
    draw_rows(frame, app, body);

    let fallback = if app.cleaning {
        let picked: Vec<&Server> = app.servers.iter().filter(|s| app.picked.contains_key(&s.port)).collect();
        let label = if picked.is_empty() {
            "Stop servers".to_string()
        } else {
            let memory = memory_of(picked.iter().copied());
            format!("Stop {} {} · free {}", picked.len(), plural(picked.len(), "server", "servers"), format::bytes(memory))
        };
        hints(&[("space", "Select"), ("a", "All"), ("⏎", &label), ("esc", "Cancel")], bottom.width, (!picked.is_empty()).then_some("⏎"))
    } else if app.visible().is_empty() {
        hints(&[("a", if app.show_all { "Dev servers only" } else { "All ports" }), ("?", "Keys"), ("q", "Quit")], bottom.width, None)
    } else {
        let suggested = app.visible().iter().filter(|s| s.cleanup_reason().is_some_and(|r| !r.is_leak())).count();
        let cleanup = if suggested > 0 { format!("Clean up {suggested}") } else { "Clean up".into() };
        hints(
            &[
                ("⏎", "Details"),
                ("o", "Open"),
                ("s", "Stop"),
                ("K", "Kill"),
                ("c", &cleanup),
                ("/", "Filter"),
                ("a", if app.show_all { "Dev only" } else { "All ports" }),
                ("?", "Keys"),
                ("q", "Quit"),
            ],
            bottom.width,
            None,
        )
    };
    frame.render_widget(Paragraph::new(footer(app, fallback, bottom.width)), bottom);
}

fn list_header(app: &App, width: u16) -> Vec<Line<'static>> {
    let visible = app.visible();
    let mut title = vec![Span::styled(if app.cleaning { "Clean up" } else { "Servers" }, text1())];
    if app.show_all && !app.cleaning {
        title.push(Span::styled("  all ports", text3()));
    }
    if !app.filter.is_empty() && !app.filtering {
        title.push(Span::styled(format!("  /{}", app.filter), text3()));
    }

    let (left, right) = if app.cleaning {
        let picked = memory_of(visible.iter().copied().filter(|s| app.picked.contains_key(&s.port)));
        let (n, unit) = format::total(picked);
        let count = app.picked.len();
        let right = if count == 0 { "Pick servers to stop".to_string() } else { format!("freed by stopping {count}") };
        (vec![Span::styled(n, text1()), Span::styled(format!(" {unit}"), text3())], vec![Span::styled(right, text2())])
    } else {
        let memory = memory_of(visible.iter().copied());
        let cpu: f64 = visible.iter().map(|s| s.cpu).sum::<f64>() / crate::sys::cpu_count() as f64;
        let (n, unit) = format::total(memory);
        let count = format!("   {} {}", visible.len(), plural(visible.len(), "server", "servers"));
        (
            vec![Span::styled(n, text1()), Span::styled(format!(" {unit}"), text3()), Span::styled(count, text3())],
            vec![Span::styled("CPU  ", text3()), Span::styled(format::percent(cpu), text2())],
        )
    };

    vec![
        centered(title),
        Line::default(),
        split(left, right, width),
        memory_bar(app, &visible, width),
        memory_legend(app, &visible),
        divider(width),
    ]
}

/// Servers as raised blocks in their port colors, other apps on a lower
/// baseline, free memory as the track. The selected server stands taller.
fn memory_bar(app: &App, servers: &[&Server], width: u16) -> Line<'static> {
    let columns = width as usize;
    let mut seen = std::collections::HashSet::new();
    let servers: Vec<&Server> = servers.iter().copied().filter(|s| seen.insert(s.root_pid)).collect();
    let servers_total = memory_of(servers.iter().copied());
    let others = app.memory_used.saturating_sub(servers_total);
    let capacity = app.memory_total.max(servers_total + others).max(1) as f64;

    let mut server_cols: Vec<usize> =
        servers.iter().map(|s| ((s.memory as f64 / capacity * columns as f64).round() as usize).max(1)).collect();
    let mut other_cols = (others as f64 / capacity * columns as f64).round() as usize;
    while server_cols.iter().sum::<usize>() + other_cols > columns {
        if other_cols > 0 {
            other_cols -= 1;
        } else if let Some(i) = (0..server_cols.len()).max_by_key(|&i| server_cols[i]).filter(|&i| server_cols[i] > 1) {
            server_cols[i] -= 1;
        } else {
            break;
        }
    }

    let mut spans = Vec::new();
    for (server, count) in servers.iter().zip(server_cols) {
        let focused = app.selected.is_some_and(|p| app.servers.iter().any(|o| o.port == p && o.root_pid == server.root_pid));
        let mut style = port_style(app, server.port);
        let dimmed = if app.cleaning { !app.picked.contains_key(&server.port) && !focused } else { !focused };
        if dimmed {
            style = style.add_modifier(Modifier::DIM);
        }
        spans.push(Span::styled((if focused { "█" } else { "▅" }).repeat(count), style));
    }
    let used: usize = spans.iter().map(Span::width).sum::<usize>() + other_cols;
    spans.push(Span::styled("▂".repeat(other_cols), Style::new().fg(Color::Gray)));
    spans.push(Span::styled("▂".repeat(columns.saturating_sub(used)), text3()));
    Line::from(spans)
}

fn memory_legend(app: &App, servers: &[&Server]) -> Line<'static> {
    let servers_total = memory_of(servers.iter().copied());
    let others = app.memory_used.saturating_sub(servers_total);
    let amount = |b: u64| {
        let (n, unit) = format::total(b);
        format!("{n} {unit}")
    };
    let mut spans = vec![
        Span::styled("▅ ", text2()),
        Span::styled("Servers ", text2()),
        Span::styled(amount(servers_total), text3()),
        Span::styled("   ▂ ", Style::new().fg(Color::Gray)),
        Span::styled("Other apps ", text2()),
        Span::styled(amount(others), text3()),
    ];
    if app.memory_total > 0 {
        let free = app.memory_total.saturating_sub(app.memory_used);
        spans.extend([
            Span::styled("   ▂ ", text3()),
            Span::styled("Free ", text2()),
            Span::styled(format!("{} of {}", format::total(free).0, amount(app.memory_total)), text3()),
        ]);
    }
    Line::from(spans)
}

fn draw_rows(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.clear();
    let servers: Vec<Server> = app.visible().into_iter().cloned().collect();
    if servers.is_empty() {
        let mut lines = vec![Line::default(); (area.height / 2).saturating_sub(2) as usize];
        if !app.scanned {
            lines.push(centered(vec![Span::styled("Scanning…", text2())]));
        } else if app.cleaning {
            lines.push(centered(vec![Span::styled("Nothing to clean up", text2())]));
            lines.push(centered(vec![Span::styled("Clean up lists dev servers; databases are never offered.", text3())]));
        } else if !app.filter.is_empty() {
            lines.push(centered(vec![Span::styled(format!("Nothing matches “{}”", app.filter), text2())]));
            lines.push(centered(vec![Span::styled("esc clears the filter", text3())]));
        } else {
            lines.push(centered(vec![Span::styled("Nothing listening", text2())]));
            let hint = if app.show_all { "No listening ports you can see." } else { "Dev servers on ports 3000 and up show up here." };
            lines.push(centered(vec![Span::styled(hint, text3())]));
        }
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    // Each row is two lines plus a gap; keep the selection in view.
    let per_row = 3usize;
    let fits = ((area.height as usize) / per_row).max(1);
    let index = servers.iter().position(|s| Some(s.port) == app.selected).unwrap_or(0);
    if index < app.list_offset {
        app.list_offset = index;
    } else if index >= app.list_offset + fits {
        app.list_offset = index + 1 - fits;
    }
    app.list_offset = app.list_offset.min(servers.len().saturating_sub(fits));

    let mut lines = Vec::new();
    for (i, server) in servers.iter().skip(app.list_offset).take(fits).enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        let y = area.y + lines.len() as u16;
        app.hits.extend([(y, server.port), (y + 1, server.port)]);
        lines.extend(server_row(app, server, area.width));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn server_row(app: &App, server: &Server, width: u16) -> [Line<'static>; 2] {
    let status = server.status();
    let selected = app.selected == Some(server.port);
    let attention = status == Status::Attention;
    let marker_style = port_style(app, server.port);
    let marker = |on: bool| Span::styled(if on { "▌" } else { " " }, marker_style);

    let mut first = vec![marker(selected), Span::raw(" ")];
    let mut second = vec![marker(selected), Span::raw(" ")];
    if app.cleaning {
        let on = app.picked.contains_key(&server.port);
        first.push(Span::styled(if on { "[✓] " } else { "[ ] " }, if on { text1() } else { text3() }));
        second.push(Span::raw("    "));
    }
    first.extend(port_label(app, server));
    let pad = 7usize.saturating_sub(1 + server.port.to_string().len()).max(1);
    first.push(Span::raw(" ".repeat(pad)));
    second.push(Span::raw(" ".repeat(7)));
    let name_style = if selected { text1().add_modifier(Modifier::UNDERLINED) } else { text1() };
    first.push(Span::styled(server.title(), name_style));

    let right = match app.busy.get(&server.port) {
        Some(label) => vec![Span::styled(label.to_string(), Style::new().fg(RED))],
        None => {
            let memories: Vec<f64> = server.history.iter().map(|s| s.memory as f64).collect();
            let spark = Style::new().fg(if attention { AMBER } else { Color::DarkGray });
            let memory = if attention { Style::new().fg(AMBER) } else { text2() };
            vec![Span::styled(format::sparkline(&memories, 8, None), spark), Span::styled(format!("{:>10}", format::bytes(server.memory)), memory)]
        }
    };
    let used: usize = second.iter().map(Span::width).sum();
    let room = (width as usize).saturating_sub(used);
    second.extend(fit(context(app, server, status, room), room));

    let dim_row = !server.cwd_exists;
    let finish = |line: Line<'static>| if dim_row { line.patch_style(Modifier::DIM) } else { line };
    [finish(split(first, right, width)), finish(Line::from(second))]
}

fn context(app: &App, server: &Server, status: Status, width: usize) -> Vec<Span<'static>> {
    if app.cleaning {
        if let Some(reason) = server.cleanup_reason() {
            let style = if reason.is_leak() { Style::new().fg(AMBER) } else { text2() };
            return vec![Span::styled(reason.label(), style)];
        }
        if server.protected {
            return vec![Span::styled(format!("{} · protected", server.process_name), text3())];
        }
    }
    if status == Status::Attention {
        let (growth, over) = server.growth();
        let text = if server.is_leaking() {
            format!("+{} in {}", format::bytes(growth.max(0) as u64), format::duration(over))
        } else {
            format!("Over {} GB", crate::scan::ALERT_BYTES / (1024 * 1024 * 1024))
        };
        return vec![Span::styled(text, Style::new().fg(AMBER))];
    }
    let time = if !server.cwd_exists {
        return vec![Span::styled(format!("Worktree deleted · idle {}", format::duration(server.idle_for())), text3())];
    } else if server.idle_for() > crate::scan::IDLE_AFTER {
        format!("idle {}", format::duration(server.idle_for()))
    } else {
        server.uptime().map(|u| format!("up {}", format::duration(u))).unwrap_or_default()
    };

    let mut spans = Vec::new();
    if let Some(agent) = &server.agent {
        spans.push(Span::styled(format!("{} ", agent.kind.glyph()), text2()));
    }
    let what = if !server.dev {
        format!("{} · pid {}", server.process_name, server.pid)
    } else if server.project.branch.is_some() {
        server.project.name.clone()
    } else {
        server.location()
    };
    // The time always shows; a long location gives way from the left.
    let tail: String = [server.project.framework.clone().unwrap_or_default(), time].into_iter().filter(|p| !p.is_empty()).map(|p| format!(" · {p}")).collect();
    let used: usize = spans.iter().map(Span::width).sum::<usize>() + tail.width();
    spans.push(Span::styled(truncate_left(&what, width.saturating_sub(used)) + &tail, text3()));
    spans
}

/// Keeps the end of `text`, which is the telling part of a path.
fn truncate_left(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1;
    for c in text.chars().rev() {
        let w = c.to_string().width();
        if used + w > width {
            break;
        }
        used += w;
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

// MARK: Details

fn draw_detail(frame: &mut Frame, app: &mut App, server: &Server, area: Rect) {
    let [top, body, _, bottom] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let mut heading = vec![Span::styled("‹  ", text3())];
    heading.extend(port_label(app, server));
    heading.push(Span::styled(format!("  {}", server.project.name), text1()));
    let right = vec![Span::styled(server.project.framework.clone().unwrap_or_default(), text3())];
    frame.render_widget(Paragraph::new(vec![split(heading, right, top.width), Line::default(), divider(top.width)]), top);

    let lines = detail_lines(app, server, body.width);
    let max_offset = (lines.len() as u16).saturating_sub(body.height);
    app.detail_offset = app.detail_offset.min(max_offset);
    frame.render_widget(Paragraph::new(lines).scroll((app.detail_offset, 0)), body);

    let mut keys = vec![("o", "Open"), ("s", "Stop"), ("K", "Kill")];
    if crate::control::can_restart(server) {
        keys.push(("r", "Restart"));
    }
    keys.extend([("y", "Copy URL"), ("Y", "Copy command")]);
    if server.agent.is_some() {
        keys.push(("A", "Copy resume"));
    }
    keys.extend([("tab", "Next"), ("esc", "Back"), ("?", "Keys")]);
    let fallback = hints(&keys, bottom.width, None);
    frame.render_widget(Paragraph::new(footer(app, fallback, bottom.width)), bottom);
}

fn detail_lines(app: &App, server: &Server, width: u16) -> Vec<Line<'static>> {
    let label_width = 11;
    let row = |label: &str, value: Vec<Span<'static>>| -> Line<'static> {
        let mut spans = vec![Span::styled(format!("{label:<label_width$}"), text3())];
        spans.extend(fit(value, (width as usize).saturating_sub(label_width)));
        Line::from(spans)
    };
    let plain = |s: String| vec![Span::styled(s, text2())];

    let mut lines = Vec::new();
    let status = match (server.status(), app.busy.get(&server.port)) {
        (_, Some(busy)) => Span::styled(busy.to_string(), Style::new().fg(RED)),
        (Status::Running, _) => Span::styled("Running", Style::new().fg(GREEN)),
        (Status::Attention, _) => Span::styled("Needs attention", Style::new().fg(AMBER)),
        (Status::Idle, _) => Span::styled(format!("Idle {}", format::duration(server.idle_for())), text3()),
    };
    let mut status_spans = vec![status];
    if server.protected {
        status_spans.push(Span::styled(" · protected", text3()));
    }
    lines.push(row("Status", status_spans));
    lines.push(row("URL", vec![Span::styled(server.url(), port_style(app, server.port)), Span::styled(format!("  {}", server.addresses.join(" ")), text3())]));
    if let Some(branch) = &server.project.branch {
        let mut spans = vec![Span::styled(branch.clone(), text1())];
        if let Some(wt) = &server.project.worktree {
            spans.push(Span::styled(format!("  worktree {wt}"), text3()));
        }
        lines.push(row("Branch", spans));
    }
    let folder = server.cwd.as_deref().map(format::tilde).unwrap_or_else(|| "Unknown".into());
    let folder = truncate_left(&folder, (width as usize).saturating_sub(label_width + 9));
    let folder = if server.cwd_exists { plain(folder) } else { vec![Span::styled(folder, text3()), Span::styled("  deleted", Style::new().fg(AMBER))] };
    lines.push(row("Folder", folder));
    if let Some(command) = &server.command {
        lines.push(row("Command", plain(command.clone())));
    }
    let pid = if server.pid == server.root_pid { server.pid.to_string() } else { format!("{} · root {}", server.pid, server.root_pid) };
    lines.push(row("PID", plain(pid)));
    if let Some(up) = server.uptime() {
        lines.push(row("Up", plain(format::duration(up))));
    }
    if let Some(ws) = &server.conductor {
        lines.push(row("Conductor", plain(ws.clone())));
    }
    if let Some(agent) = &server.agent {
        let mut spans = vec![Span::styled(format!("{} {}", agent.kind.glyph(), agent.kind.label()), text2())];
        if let Some(title) = &agent.title {
            spans.push(Span::styled(format!(" · {title}"), text2()));
        }
        lines.push(row("Session", spans));
        lines.push(row("", vec![Span::styled(agent.resume_command(), text3())]));
    }

    lines.push(Line::default());
    let chart_width = (width as usize).saturating_sub(label_width + 12).clamp(8, 60);
    let memories: Vec<f64> = server.history.iter().map(|s| s.memory as f64).collect();
    let cpus: Vec<f64> = server.history.iter().map(|s| s.cpu).collect();
    let attention = server.status() == Status::Attention;
    let chart_style = Style::new().fg(if attention { AMBER } else { PORTS[app.color_index(server.port) % PORTS.len()] });
    lines.push(row("Memory", vec![Span::styled(format!("{:<10}", format::bytes(server.memory)), if attention { Style::new().fg(AMBER) } else { text1() }), Span::styled(format::sparkline(&memories, chart_width, None), chart_style)]));
    lines.push(row("CPU", vec![Span::styled(format!("{:<10}", format::percent(server.cpu)), text1()), Span::styled(format::sparkline(&cpus, chart_width, Some(10.0)), chart_style)]));
    let span = server.history.first().zip(server.history.last()).map(|(a, b)| b.at - a.at).unwrap_or_default();
    lines.push(row("", vec![Span::styled(format!("last {} · whole process tree", format::duration(span)), text3())]));

    lines.push(Line::default());
    let count = server.processes.len();
    lines.push(Line::styled(format!("{count} {}", plural(count, "process", "processes")), text2()));
    for p in &server.processes {
        let indent = "  ".repeat(p.depth);
        let left = vec![Span::styled(format!("{:>7}  ", p.pid), text3()), Span::styled(format!("{indent}{}", p.name), text2())];
        let right = vec![Span::styled(format::bytes(p.memory), text3())];
        lines.push(split(left, right, width));
    }
    lines
}

// MARK: Help

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let [top, body, _, bottom] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Paragraph::new(vec![centered(vec![Span::styled("Keys", text1())]), Line::default(), divider(top.width)]), top);

    let groups: &[(&str, &[(&str, &str)])] = &[
        ("Servers", &[("↑ ↓  j k", "Select"), ("⏎", "Details"), ("c", "Clean up"), ("/", "Filter by port, name, branch or command"), ("a", "Every listening port, or dev servers only")]),
        (
            "Actions",
            &[
                ("o", "Open in browser"),
                ("s", "Stop: SIGTERM the process tree, SIGKILL after 3s"),
                ("K", "Kill: SIGKILL the process tree now"),
                ("r", "Restart with the same command and folder"),
                ("y", "Copy URL"),
                ("Y", "Copy command"),
                ("A", "Copy the agent resume command"),
                ("e", "Open the project folder in Finder"),
            ],
        ),
        ("Clean up", &[("space", "Select or deselect"), ("a", "Select all or none"), ("⏎", "Stop selected"), ("esc", "Cancel")]),
        ("Details", &[("⏎", "Open in browser"), ("tab", "Next server"), ("↑ ↓", "Scroll"), ("esc", "Back")]),
        ("Anywhere", &[("?", "Keys"), ("esc", "Back, or quit from the list"), ("q", "Quit")]),
    ];
    let mut lines = Vec::new();
    for (title, keys) in groups {
        lines.push(Line::default());
        lines.push(Line::styled(title.to_string(), text2()));
        for (key, label) in *keys {
            lines.push(Line::from(vec![Span::styled(format!("{key:<12}"), text1()), Span::styled(label.to_string(), text3())]));
        }
    }
    lines.push(Line::default());
    lines.push(Line::styled("Databases (postgres, redis, mongod, mysql) are never preselected in Clean up.", text3()));
    let max_offset = (lines.len() as u16).saturating_sub(body.height);
    app.help_offset = app.help_offset.min(max_offset);
    frame.render_widget(Paragraph::new(lines).scroll((app.help_offset, 0)), body);
    frame.render_widget(Paragraph::new(hints(&[("↑ ↓", "Scroll"), ("esc", "Back"), ("q", "Quit")], bottom.width, None)), bottom);
}
