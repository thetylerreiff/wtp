//! Builds the server list from sockets and the process table. Holds the state
//! that persists between scans (CPU deltas, history, caches), so it lives on
//! one thread.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::{Agent, AgentResolver};
use crate::project::{Project, ProjectResolver};
use crate::sys::{self, Proc, ProcArgs, TcpState};

pub const MIN_DEV_PORT: u16 = 3000;
pub const ALERT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const LEAK_BYTES: i64 = 500 * 1024 * 1024;
pub const IDLE_AFTER: Duration = Duration::from_secs(3600);
pub const CLEANUP_IDLE: Duration = Duration::from_secs(4 * 3600);
pub const CLEANUP_UPTIME: Duration = Duration::from_secs(3 * 86400);
const HISTORY_WINDOW: Duration = Duration::from_secs(600);

/// Process names that count as dev servers. Names are compared after
/// dropping version suffixes, so `python3.12` and `node22` match too.
const DEV_PROCESSES: &[&str] = &[
    "node", "npm", "npx", "deno", "bun", "Python", "python", "uvicorn", "gunicorn", "flask", "django",
    "ruby", "rails", "puma", "unicorn", "php", "php-fpm", "java", "gradle", "mvn", "go", "air", "cargo",
    "rustc", "dotnet", "beam.smp", "elixir", "mix", "nginx", "httpd", "apache", "postgres", "mysql",
    "mysqld", "redis-server", "mongod", "docker-proxy", "workerd", "wrangler", "vite", "esbuild",
];
pub const PROTECTED: &[&str] = &["postgres", "redis-server", "mongod", "mysqld", "mysql"];
/// Processes between a shell and the actual server, e.g. `npm run dev`.
const RUNNERS: &[&str] = &[
    "node", "npm", "npx", "pnpm", "yarn", "bun", "bunx", "deno", "turbo", "nx", "python", "Python", "uv",
    "poetry", "pipenv", "ruby", "bundle", "rails", "go", "air", "cargo", "java", "gradle", "mvn", "dotnet",
    "php", "mix", "beam.smp", "elixir",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish"];
const AGENT_NAMES: &[&str] = &["claude", "codex", "Conductor"];
const AGENT_PATH_MARKERS: &[&str] = &["@anthropic-ai/claude-code", "com.conductor.app", "/codex/"];

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub at: Instant,
    pub memory: u64,
    pub cpu: f64,
}

#[derive(Clone, Debug)]
pub struct TreeProc {
    pub pid: i32,
    pub name: String,
    pub depth: usize,
    pub memory: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Attention,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CleanUpReason {
    WorktreeDeleted,
    Idle(Duration),
    LongRunning(Duration),
    Leaking(u64),
}

impl CleanUpReason {
    pub fn is_leak(self) -> bool {
        matches!(self, CleanUpReason::Leaking(_))
    }

    pub fn label(self) -> String {
        match self {
            CleanUpReason::WorktreeDeleted => "Worktree deleted".into(),
            CleanUpReason::Idle(d) => format!("Idle for {}", crate::format::duration(d)),
            CleanUpReason::LongRunning(d) => format!("Running for {}", crate::format::duration(d)),
            CleanUpReason::Leaking(b) => format!("Growing +{} · not preselected", crate::format::bytes(b)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Server {
    pub port: u16,
    pub pid: i32,
    pub root_pid: i32,
    pub process_name: String,
    pub addresses: Vec<String>,
    pub cwd: Option<String>,
    pub cwd_exists: bool,
    pub command: Option<String>,
    pub launch: Option<Arc<ProcArgs>>,
    pub launch_dir: Option<String>,
    pub started_at: Option<SystemTime>,
    pub project: Project,
    pub conductor: Option<String>,
    pub agent: Option<Agent>,
    pub processes: Vec<TreeProc>,
    /// Identity of every process in the tree, so a reused pid is never signalled.
    pub starts: Vec<(i32, u64)>,
    pub memory: u64,
    pub cpu: f64,
    pub history: Vec<Sample>,
    pub last_active: Instant,
    pub protected: bool,
    /// The app bundle behind the process, e.g. "Docker" for com.docker.backend.
    pub app: Option<String>,
    /// A dev process on a dev port. Everything else shows only in "all ports".
    pub dev: bool,
}

impl Server {
    /// What a row is called: the branch when there is one, else the name.
    pub fn title(&self) -> String {
        match (&self.project.root, &self.project.branch) {
            (Some(_), Some(branch)) => branch.clone(),
            _ => self.name(),
        }
    }

    /// The project, or the app or process for things that aren't projects.
    pub fn name(&self) -> String {
        if self.project.root.is_some() {
            return self.project.name.clone();
        }
        if let Some(app) = &self.app {
            return app.clone();
        }
        match self.cwd.as_deref() {
            Some("/") | None => self.process_name.clone(),
            Some(_) => self.project.name.clone(),
        }
    }

    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    pub fn uptime(&self) -> Option<Duration> {
        self.started_at.and_then(|s| s.elapsed().ok())
    }

    pub fn idle_for(&self) -> Duration {
        self.last_active.elapsed()
    }

    /// Memory growth across the recorded history (up to ten minutes).
    pub fn growth(&self) -> (i64, Duration) {
        match (self.history.first(), self.history.last()) {
            (Some(first), Some(last)) if last.at - first.at >= Duration::from_secs(120) => {
                (last.memory as i64 - first.memory as i64, last.at - first.at)
            }
            _ => (0, Duration::ZERO),
        }
    }

    pub fn is_leaking(&self) -> bool {
        self.growth().0 >= LEAK_BYTES
    }

    pub fn status(&self) -> Status {
        if !self.cwd_exists {
            Status::Idle
        } else if self.memory >= ALERT_BYTES || self.is_leaking() {
            Status::Attention
        } else if self.idle_for() > IDLE_AFTER {
            Status::Idle
        } else {
            Status::Running
        }
    }

    pub fn cleanup_reason(&self) -> Option<CleanUpReason> {
        if self.protected || !self.dev {
            return None;
        }
        if !self.cwd_exists {
            return Some(CleanUpReason::WorktreeDeleted);
        }
        if self.idle_for() >= CLEANUP_IDLE {
            return Some(CleanUpReason::Idle(self.idle_for()));
        }
        if let Some(up) = self.uptime().filter(|up| *up >= CLEANUP_UPTIME) {
            return Some(CleanUpReason::LongRunning(up));
        }
        self.is_leaking().then(|| CleanUpReason::Leaking(self.growth().0.max(0) as u64))
    }

    /// Short location: Conductor workspace, git worktree, or parent folder.
    pub fn location(&self) -> String {
        if let Some(ws) = &self.conductor {
            return ws.clone();
        }
        if let Some(wt) = &self.project.worktree {
            return wt.clone();
        }
        match self.project.root.as_deref().or(self.cwd.as_deref()) {
            Some(root) => crate::format::tilde(std::path::Path::new(root).parent().and_then(|p| p.to_str()).unwrap_or(root)),
            None => self.project.name.clone(),
        }
    }
}

pub struct Snapshot {
    pub servers: Vec<Server>,
    pub memory_total: u64,
    pub memory_used: u64,
}

#[derive(Default)]
pub struct ScanEngine {
    projects: ProjectResolver,
    pub agents: AgentResolver,
    previous_cpu: HashMap<i32, (u64, Instant)>,
    histories: HashMap<(u16, i32), Vec<Sample>>,
    last_active: HashMap<(u16, i32), Instant>,
    args_cache: HashMap<i32, (u64, Option<Arc<ProcArgs>>)>,
}

/// Drops a version suffix: `python3.12` → `python`, `node22` → `node`.
fn base_name(comm: &str) -> &str {
    let trimmed = comm.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if trimmed.is_empty() { comm } else { trimmed }
}

fn is_one_of(comm: &str, list: &[&str]) -> bool {
    list.contains(&comm) || list.contains(&base_name(comm))
}

impl ScanEngine {
    pub fn scan(&mut self) -> Snapshot {
        let now = Instant::now();
        let processes = sys::all_processes();

        // Lowest pid first, so a forking server's master owns the port its
        // workers inherited, and ownership doesn't shift between scans.
        let mut pids: Vec<i32> = processes.keys().copied().collect();
        pids.sort_unstable();
        let mut sockets = Vec::new();
        for pid in pids {
            sys::tcp_sockets(pid, &mut sockets);
        }
        let lineage = sys::own_lineage();
        // Each port once, held by its lowest-pid listener; addresses merged.
        let mut listeners: HashMap<u16, (i32, Vec<String>)> = HashMap::new();
        let mut connections: HashMap<u16, usize> = HashMap::new();
        for socket in &sockets {
            match socket.state {
                TcpState::Listen => {
                    let entry = listeners.entry(socket.port).or_insert_with(|| (socket.pid, Vec::new()));
                    if entry.0 == socket.pid && !entry.1.contains(&socket.address) {
                        entry.1.push(socket.address.clone());
                    }
                }
                TcpState::Established => *connections.entry(socket.port).or_default() += 1,
            }
        }

        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for p in processes.values() {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        for list in children.values_mut() {
            list.sort_unstable();
        }

        let mut ports: Vec<_> = listeners.into_iter().collect();
        ports.sort_unstable_by_key(|(port, _)| *port);

        let mut servers = Vec::with_capacity(ports.len());
        let mut seen = HashSet::new();
        let mut live_pids = HashSet::new();
        for (port, (pid, addresses)) in ports {
            let Some(listener) = processes.get(&pid) else { continue };
            let root = self.root_process(listener, &processes, &lineage);
            let tree = descendants(root.pid, &children, &processes);

            let mut memory = 0;
            let mut cpu = 0.0;
            let mut nodes = Vec::with_capacity(tree.len());
            let mut starts = Vec::with_capacity(tree.len());
            for (p, depth) in &tree {
                live_pids.insert(p.pid);
                let usage = sys::usage(p.pid);
                let footprint = usage.map_or(0, |u| u.footprint);
                memory += footprint;
                if let Some(u) = usage {
                    if let Some((prev, at)) = self.previous_cpu.get(&p.pid)
                        && u.cpu_ns >= *prev
                    {
                        let elapsed = now.duration_since(*at).as_secs_f64();
                        if elapsed > 0.0 {
                            cpu += (u.cpu_ns - prev) as f64 / (elapsed * 1e9) * 100.0;
                        }
                    }
                    self.previous_cpu.insert(p.pid, (u.cpu_ns, now));
                }
                starts.push((p.pid, p.start));
                nodes.push(TreeProc { pid: p.pid, name: self.display_name(p), depth: *depth, memory: footprint });
            }

            let root_args = self.args(root);
            let listener_args = self.args(listener);
            let cwd = sys::current_dir(listener.pid).or_else(|| sys::current_dir(root.pid));
            // Apps that embed a node or python backend aren't dev servers.
            let app = [&listener_args, &root_args].iter().find_map(|a| a.as_ref().and_then(|a| app_bundle(&a.exe)));
            let is_app = cwd.as_deref() == Some("/") || app.is_some();
            let dev = !is_app && port >= MIN_DEV_PORT && is_one_of(&listener.comm, DEV_PROCESSES);

            let env = self.inherited_env(listener, root, &processes);
            let command = root_args.as_ref().map(|a| pretty_command(&a.args, &root.comm));
            let project = self.projects.resolve(cwd.as_deref(), command.as_deref());
            // Restart from the highest process whose argv wasn't overwritten by
            // a title, e.g. the `sh -c "next dev -p 3000"` that npm spawns.
            let launcher = tree.iter().map(|(p, _)| *p).find(|p| has_intact_args(self.args(p).as_deref())).unwrap_or(root);

            let key = (port, root.pid);
            seen.insert(key);
            let conns = connections.get(&port).copied().unwrap_or(0);
            if cpu >= 2.0 || conns > 0 || !self.last_active.contains_key(&key) {
                self.last_active.insert(key, now);
            }
            let history = self.histories.entry(key).or_default();
            history.push(Sample { at: now, memory, cpu });
            history.retain(|s| now.duration_since(s.at) <= HISTORY_WINDOW);
            let history = history.clone();

            servers.push(Server {
                port,
                pid: listener.pid,
                root_pid: root.pid,
                process_name: listener.comm.clone(),
                addresses,
                cwd_exists: cwd.as_deref().is_none_or(|c| std::path::Path::new(c).exists()),
                command,
                launch: self.args(launcher),
                launch_dir: sys::current_dir(launcher.pid).or_else(|| cwd.clone()),
                started_at: Some(UNIX_EPOCH + Duration::from_micros(root.start)),
                project,
                conductor: env.get("CONDUCTOR_WORKSPACE_NAME").cloned(),
                agent: self.agents.resolve(&env, cwd.as_deref()),
                cwd,
                processes: nodes,
                starts,
                memory,
                cpu,
                history,
                last_active: self.last_active[&key],
                protected: is_one_of(&listener.comm, PROTECTED),
                app,
                dev,
            });
        }

        self.histories.retain(|k, _| seen.contains(k));
        self.last_active.retain(|k, _| seen.contains(k));
        self.previous_cpu.retain(|pid, _| live_pids.contains(pid));
        self.args_cache.retain(|pid, _| processes.contains_key(pid));

        let (memory_total, memory_used) = sys::system_memory();
        Snapshot { servers, memory_total, memory_used }
    }

    /// Climbs from the listener to the command the user actually ran, e.g.
    /// from `next-server` up to `npm run dev`. Stops at shells, terminals and
    /// coding agents so stopping a server never takes its launcher with it.
    /// Never climbs into wtp's own lineage, whatever runner it looks like.
    fn root_process<'a>(&mut self, listener: &'a Proc, processes: &'a HashMap<i32, Proc>, lineage: &HashSet<i32>) -> &'a Proc {
        let mut current = listener;
        while let Some(parent) = processes.get(&current.ppid).filter(|p| current.ppid > 1 && !lineage.contains(&p.pid)) {
            if is_one_of(&parent.comm, RUNNERS) && !self.is_agent(parent) {
                current = parent;
                continue;
            }
            // Package managers run scripts through `sh -c`; step over that
            // shell only when a package manager sits directly above it.
            if SHELLS.contains(&parent.comm.as_str())
                && parent.ppid > 1
                && let Some(grandparent) = processes.get(&parent.ppid)
                && !lineage.contains(&grandparent.pid)
                && is_one_of(&grandparent.comm, RUNNERS)
                && !self.is_agent(grandparent)
            {
                current = grandparent;
                continue;
            }
            break;
        }
        current
    }

    fn is_agent(&mut self, p: &Proc) -> bool {
        if AGENT_NAMES.contains(&p.comm.as_str()) {
            return true;
        }
        let Some(args) = self.args(p) else { return false };
        let joined = std::iter::once(args.exe.as_str()).chain(args.args.iter().take(3).map(String::as_str)).collect::<Vec<_>>().join(" ");
        AGENT_PATH_MARKERS.iter().any(|m| joined.contains(m))
    }

    /// Merges environments from the listener up through its launcher. Tools
    /// that rename their process overwrite the memory their environment is
    /// read from, so session variables often only survive on a parent.
    fn inherited_env(&mut self, listener: &Proc, root: &Proc, processes: &HashMap<i32, Proc>) -> HashMap<String, String> {
        let mut chain = vec![listener];
        let mut current = listener;
        let mut extra_hops = 2;
        let mut passed_root = listener.pid == root.pid;
        while current.ppid > 1 && chain.len() < 10 {
            let Some(parent) = processes.get(&current.ppid) else { break };
            if passed_root {
                if extra_hops == 0 {
                    break;
                }
                extra_hops -= 1;
            }
            chain.push(parent);
            passed_root |= parent.pid == root.pid;
            current = parent;
        }
        let mut env = HashMap::new();
        for p in chain.into_iter().rev() {
            if let Some(args) = self.args(p) {
                env.extend(args.env.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        env
    }

    fn args(&mut self, p: &Proc) -> Option<Arc<ProcArgs>> {
        if let Some((start, args)) = self.args_cache.get(&p.pid)
            && *start == p.start
        {
            return args.clone();
        }
        let args = sys::arguments(p.pid).map(Arc::new);
        self.args_cache.insert(p.pid, (p.start, args.clone()));
        args
    }

    fn display_name(&mut self, p: &Proc) -> String {
        match self.args(p).filter(|a| !a.args.is_empty()) {
            // `process.title` renames like "next-server (v16.0.0)" read better without the version.
            Some(args) => {
                let command = pretty_command(&args.args, &p.comm);
                command.split(" (").next().unwrap_or(&command).to_string()
            }
            None => p.comm.clone(),
        }
    }
}

fn descendants<'a>(root: i32, children: &HashMap<i32, Vec<i32>>, processes: &'a HashMap<i32, Proc>) -> Vec<(&'a Proc, usize)> {
    let mut result = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((pid, depth)) = stack.pop() {
        let Some(p) = processes.get(&pid) else { continue };
        result.push((p, depth));
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().rev().map(|&k| (k, depth + 1)));
        }
    }
    result
}

/// "Docker" from /Applications/Docker.app/Contents/MacOS/com.docker.backend.
/// Runtimes shipped as frameworks (Python.framework/…/Python.app) don't count.
fn app_bundle(exe: &str) -> Option<String> {
    if exe.contains(".framework/") {
        return None;
    }
    let (before, _) = exe.split_once(".app/")?;
    Some(last_component(before).to_string())
}

fn has_intact_args(args: Option<&ProcArgs>) -> bool {
    let Some(args) = args else { return false };
    let parts: Vec<_> = args.args.iter().filter(|a| !a.trim().is_empty()).collect();
    parts.len() > 1 || (parts.len() == 1 && !parts[0].contains(' '))
}

fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Turns raw argv into what someone would have typed, e.g.
/// `node /…/npm-cli.js run dev` becomes `npm run dev`.
pub fn pretty_command(arguments: &[String], comm: &str) -> String {
    let mut parts: Vec<&str> = arguments.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
    let Some(first) = parts.first().copied() else { return comm.to_string() };
    let executable = last_component(first);
    if matches!(executable, "node" | "bun" | "deno") && parts.len() > 1 {
        let script = parts[1];
        const TOOLS: &[&str] = &["npm", "npx", "pnpm", "yarn", "vite", "next", "astro", "nuxt", "storybook", "tsx", "turbo"];
        if let Some(tool) = TOOLS.iter().find(|t| {
            script.contains(&format!("/{t}/")) || script.contains(&format!("/{t}-cli")) || last_component(script) == **t
        }) {
            parts.splice(0..2, [*tool]);
        } else if script.starts_with('/') || script.starts_with('.') {
            parts.splice(0..2, [last_component(script)]);
        }
    } else {
        parts[0] = executable;
    }
    parts.iter().map(|p| if p.starts_with('/') { last_component(p) } else { p }).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pretty_command_names_the_tool() {
        assert_eq!(pretty_command(&args(&["/usr/local/bin/node", "/x/lib/node_modules/npm/bin/npm-cli.js", "run", "dev"]), "node"), "npm run dev");
        assert_eq!(pretty_command(&args(&["node", "/app/node_modules/.bin/vite", "--port", "5173"]), "node"), "vite --port 5173");
        assert_eq!(pretty_command(&args(&["/opt/homebrew/bin/python3", "-m", "http.server"]), "python3"), "python3 -m http.server");
        assert_eq!(pretty_command(&[], "node"), "node");
    }

    #[test]
    fn version_suffixes_match_dev_processes() {
        assert!(is_one_of("python3.12", DEV_PROCESSES));
        assert!(is_one_of("node", DEV_PROCESSES));
        assert!(!is_one_of("Spotify", DEV_PROCESSES));
    }

    #[test]
    fn names_the_outermost_app_bundle() {
        assert_eq!(app_bundle("/Applications/Docker.app/Contents/MacOS/com.docker.backend").as_deref(), Some("Docker"));
        assert_eq!(app_bundle("/Applications/Linear.app/Contents/Frameworks/Linear Helper.app/Contents/MacOS/Linear Helper").as_deref(), Some("Linear"));
        assert_eq!(app_bundle("/opt/homebrew/bin/node"), None);
        let python = "/opt/homebrew/Cellar/python@3.12/3.12.4/Frameworks/Python.framework/Versions/3.12/Resources/Python.app/Contents/MacOS/Python";
        assert_eq!(app_bundle(python), None);
    }
}
