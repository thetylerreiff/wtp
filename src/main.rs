mod agent;
mod app;
mod control;
mod format;
mod project;
mod scan;
mod sys;
mod ui;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

use scan::{ScanEngine, Server, Status};

const USAGE: &str = "Every dev server on your Mac, in the terminal.

Usage:
  wtp                      Browse, open and stop servers
  wtp list [--all]         Print servers and exit
  wtp list --json [--all]  Print servers as JSON
  wtp kill <port>... [-9]  Stop whatever is listening on each port
  wtp --version            Print the version

--all includes every listening port, not just dev servers on 3000 and up.
Press ? in wtp for keys.";

pub fn home() -> &'static str {
    static HOME: OnceLock<String> = OnceLock::new();
    HOME.get_or_init(|| std::env::var("HOME").unwrap_or_default())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    match args.first().map(String::as_str) {
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => app::run(),
        None => list(false, false),
        Some("list" | "ls") => list(flag("--json"), flag("--all")),
        Some("--json") => list(true, flag("--all")),
        Some("kill" | "stop") => kill(&args[1..]),
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version" | "-v" | "--version") => {
            println!("wtp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("wtp: unknown command ‘{other}’\n\n{USAGE}");
            ExitCode::from(64)
        }
    }
}

fn list(json: bool, all: bool) -> ExitCode {
    let mut engine = ScanEngine::default();
    engine.agents.refresh_codex();
    // CPU is measured between two scans.
    engine.scan();
    std::thread::sleep(Duration::from_millis(250));
    let servers: Vec<Server> = engine.scan().servers.into_iter().filter(|s| all || s.dev).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&servers.iter().map(to_json).collect::<Vec<_>>()).unwrap_or_default());
    } else if servers.is_empty() {
        println!("Nothing listening on dev ports ({}+). Try `wtp list --all`.", scan::MIN_DEV_PORT);
    } else {
        print_table(&servers);
    }
    ExitCode::SUCCESS
}

fn print_table(servers: &[Server]) {
    let headers = ["PORT", "NAME", "BRANCH", "MEMORY", "CPU", "UP", "SESSION"];
    let rows: Vec<[String; 7]> = servers
        .iter()
        .map(|s| {
            [
                format!(":{}", s.port),
                s.name(),
                s.project.branch.clone().unwrap_or_default(),
                format::bytes(s.memory),
                format::percent(s.cpu),
                s.uptime().map(format::duration).unwrap_or_default(),
                s.agent.as_ref().map(|a| [Some(a.kind.label()), a.title.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" · ")).unwrap_or_default(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..7).map(|c| rows.iter().map(|r| r[c].chars().count()).chain([headers[c].len()]).max().unwrap_or(0)).collect();
    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(c, cell)| if (3..=5).contains(&c) { format!("{cell:>w$}", w = widths[c]) } else { format!("{cell:<w$}", w = widths[c]) })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let color = std::io::stdout().is_terminal();
    let (dim, reset) = if color { ("\x1b[2m", "\x1b[0m") } else { ("", "") };
    println!("{dim}{}{reset}", line(&headers.map(String::from)));
    for (server, row) in servers.iter().zip(&rows) {
        let text = line(row);
        if color && server.status() == Status::Attention {
            println!("\x1b[33m{text}{reset}");
        } else {
            println!("{text}");
        }
    }
}

fn to_json(s: &Server) -> serde_json::Value {
    let status = match s.status() {
        Status::Running => "running",
        Status::Attention => "attention",
        Status::Idle => "idle",
    };
    let mut value = serde_json::json!({
        "port": s.port,
        "url": s.url(),
        "pid": s.pid,
        "name": s.name(),
        "branch": s.project.branch,
        "framework": s.project.framework,
        "folder": s.cwd,
        "command": s.command,
        "startedAt": s.started_at.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()),
        "memoryBytes": s.memory,
        "cpuPercent": (s.cpu * 10.0).round() / 10.0,
        "status": status,
        "protected": s.protected,
        "devServer": s.dev,
        "processes": s.processes.iter().map(|p| serde_json::json!({"pid": p.pid, "name": p.name, "memoryBytes": p.memory})).collect::<Vec<_>>(),
    });
    if let Some(agent) = &s.agent {
        value["session"] = serde_json::json!({"kind": agent.kind.label(), "id": agent.id, "title": agent.title});
    }
    if let Some(ws) = &s.conductor {
        value["conductorWorkspace"] = ws.clone().into();
    }
    value
}

/// `wtp kill 3000 5173`: stops the whole tree behind each port, whatever it is.
fn kill(args: &[String]) -> ExitCode {
    let force = args.iter().any(|a| a == "-9" || a == "--force");
    let ports: Vec<u16> = args.iter().filter_map(|a| a.trim_start_matches(':').parse().ok()).collect();
    if ports.is_empty() {
        eprintln!("wtp: kill needs at least one port, e.g. `wtp kill 3000`");
        return ExitCode::from(64);
    }
    let servers = ScanEngine::default().scan().servers;
    // Stop every port at once, so stubborn servers share one grace period.
    let results: Vec<(String, bool)> = std::thread::scope(|scope| {
        let handles: Vec<_> = ports
            .iter()
            .map(|&port| {
                let server = servers.iter().find(|s| s.port == port);
                scope.spawn(move || match server {
                    None => (format!(":{port}  nothing listening (or owned by another user)"), false),
                    Some(server) => {
                        let count = server.starts.len();
                        let label = format!(":{port}  {} · {count} {}", server.title(), if count == 1 { "process" } else { "processes" });
                        if !control::stop(&server.starts, force) {
                            (format!("{label} · still running"), false)
                        } else if !control::port_released(port) {
                            (format!("{label} · stopped, but something else still holds :{port}"), false)
                        } else {
                            (format!("{label} · stopped"), true)
                        }
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| (String::new(), false))).collect()
    });
    let mut failed = false;
    for (line, ok) in results {
        if ok {
            println!("{line}");
        } else {
            eprintln!("{line}");
            failed = true;
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
