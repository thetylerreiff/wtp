//! The interactive TUI: state, input handling and the background threads.
//! The first frame is drawn before the first scan finishes, and scanning,
//! stopping and restarting all run off the UI thread.

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;

use crate::control;
use crate::scan::{ScanEngine, Server, Snapshot};

const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const TOAST_FOR: Duration = Duration::from_secs(4);

pub enum Msg {
    Scan(Snapshot),
    Input(Event),
    Done { port: u16, text: String, tone: Tone },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Good,
    Bad,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    List,
    Detail(u16),
    Help,
}

/// Each target is a port and the root pid seen when it was chosen, so a
/// different server that takes the port meanwhile is never stopped.
pub enum Confirm {
    Stop { targets: Vec<(u16, i32)>, force: bool },
    Restart(u16, i32),
}

pub struct Toast {
    pub text: String,
    pub tone: Tone,
    pub at: Instant,
}

pub struct App {
    pub servers: Vec<Server>,
    pub scanned: bool,
    pub memory_total: u64,
    pub memory_used: u64,
    pub selected: Option<u16>,
    pub page: Page,
    pub cleaning: bool,
    /// Ticked in Clean up: port → root pid when ticked.
    pub picked: HashMap<u16, i32>,
    pub confirm: Option<Confirm>,
    pub filter: String,
    pub filtering: bool,
    pub show_all: bool,
    pub toast: Option<Toast>,
    /// Ports with a stop or restart in flight, and what's happening.
    pub busy: HashMap<u16, &'static str>,
    colors: HashMap<u16, usize>,
    pub list_offset: usize,
    pub detail_offset: u16,
    pub help_offset: u16,
    help_return: Option<Page>,
    /// Screen rows of the list, for mouse clicks: (y, port).
    pub hits: Vec<(u16, u16)>,
    tx: Sender<Msg>,
    rescan: Sender<()>,
    quit: bool,
    /// Quit asked for while stops were in flight; leave when they finish.
    pub quitting: bool,
}

pub fn run() -> ExitCode {
    let (tx, rx) = mpsc::channel();
    let (rescan_tx, rescan_rx) = mpsc::channel();
    spawn_scanner(tx.clone(), rescan_rx);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    spawn_input(tx.clone());

    let mut app = App::new(tx, rescan_tx);
    let result = app.event_loop(&mut terminal, &rx);

    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wtp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn spawn_scanner(tx: Sender<Msg>, rescan: Receiver<()>) {
    thread::spawn(move || {
        let mut engine = ScanEngine::default();
        // A quick second scan so CPU has a reading within half a second.
        let mut wait = Duration::from_millis(500);
        loop {
            if tx.send(Msg::Scan(engine.scan())).is_err() {
                return;
            }
            // Codex sessions are indexed after the first result is on screen.
            if engine.agents.needs_codex_index() {
                engine.agents.refresh_codex();
            }
            match rescan.recv_timeout(wait) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while rescan.try_recv().is_ok() {}
            wait = SCAN_INTERVAL;
        }
    });
}

fn spawn_input(tx: Sender<Msg>) {
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                return;
            }
        }
    });
}

impl App {
    fn new(tx: Sender<Msg>, rescan: Sender<()>) -> Self {
        App {
            servers: Vec::new(),
            scanned: false,
            memory_total: 0,
            memory_used: 0,
            selected: None,
            page: Page::List,
            cleaning: false,
            picked: HashMap::new(),
            confirm: None,
            filter: String::new(),
            filtering: false,
            show_all: false,
            toast: None,
            busy: HashMap::new(),
            colors: HashMap::new(),
            list_offset: 0,
            detail_offset: 0,
            help_offset: 0,
            help_return: None,
            hits: Vec::new(),
            tx,
            rescan,
            quit: false,
            quitting: false,
        }
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal, rx: &Receiver<Msg>) -> std::io::Result<()> {
        terminal.draw(|f| crate::ui::draw(f, self))?;
        while !self.quit {
            // Tick once a second so uptimes and toasts stay current.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Coalesce bursts (key repeat, scan + input) into one frame.
            while let Ok(msg) = rx.try_recv() {
                self.handle(msg);
            }
            if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > TOAST_FOR) {
                self.toast = None;
            }
            if !self.quit {
                terminal.draw(|f| crate::ui::draw(f, self))?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Scan(snapshot) => self.apply_scan(snapshot),
            Msg::Input(Event::Key(key)) if key.kind != KeyEventKind::Release => self.on_key(key),
            Msg::Input(Event::Mouse(mouse)) => match mouse.kind {
                MouseEventKind::ScrollDown => self.on_scroll(1),
                MouseEventKind::ScrollUp => self.on_scroll(-1),
                MouseEventKind::Down(MouseButton::Left) => self.on_click(mouse.row),
                _ => {}
            },
            Msg::Input(_) => {}
            Msg::Done { port, text, tone } => {
                self.busy.remove(&port);
                self.say(text, tone);
                if self.quitting && self.busy.is_empty() {
                    self.quit = true;
                }
            }
        }
    }

    fn apply_scan(&mut self, snapshot: Snapshot) {
        let index = self.selected_index();
        self.servers = snapshot.servers;
        self.memory_total = snapshot.memory_total;
        self.memory_used = snapshot.memory_used;
        self.scanned = true;
        // Port colors stay put across reordering, filtering and restarts.
        for port in self.servers.iter().map(|s| s.port) {
            let next = self.colors.len();
            self.colors.entry(port).or_insert(next);
        }
        let ports: HashSet<u16> = self.servers.iter().map(|s| s.port).collect();
        let roots: HashSet<(u16, i32)> = self.servers.iter().map(|s| (s.port, s.root_pid)).collect();
        self.picked.retain(|port, root| roots.contains(&(*port, *root)));
        self.keep_selection(index);
        if let Page::Detail(port) = self.page
            && !ports.contains(&port)
            && !self.busy.contains_key(&port)
        {
            self.page = Page::List;
        }
    }

    pub fn color_index(&self, port: u16) -> usize {
        self.colors.get(&port).copied().unwrap_or(0)
    }

    pub fn visible(&self) -> Vec<&Server> {
        let query = self.filter.to_lowercase();
        self.servers
            .iter()
            .filter(|s| self.show_all || s.dev)
            // Clean up is for dev servers; databases are never offered.
            .filter(|s| !self.cleaning || (s.dev && !s.protected))
            .filter(|s| {
                query.is_empty()
                    || [
                        Some(s.port.to_string()),
                        Some(s.project.name.clone()),
                        s.project.branch.clone(),
                        s.command.clone(),
                        Some(s.process_name.clone()),
                        s.project.framework.clone(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|field| field.to_lowercase().contains(&query))
            })
            .collect()
    }

    fn selected_index(&self) -> Option<usize> {
        self.visible().iter().position(|s| Some(s.port) == self.selected)
    }

    /// Keeps the selection on the same port, or at the same place in the list
    /// when that port went away.
    fn keep_selection(&mut self, previous_index: Option<usize>) {
        let visible: Vec<u16> = self.visible().iter().map(|s| s.port).collect();
        if self.selected.is_some_and(|p| visible.contains(&p)) {
            return;
        }
        self.selected = previous_index.map(|i| i.min(visible.len().saturating_sub(1))).and_then(|i| visible.get(i).copied()).or(visible.first().copied());
    }

    fn move_selection(&mut self, delta: isize) {
        let visible: Vec<u16> = self.visible().iter().map(|s| s.port).collect();
        if visible.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, visible.len() as isize - 1) as usize;
        self.selected = Some(visible[next]);
    }

    fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.toast = Some(Toast { text: text.into(), tone, at: Instant::now() });
    }

    // MARK: Input

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        if self.confirm.is_some() {
            return self.on_confirm_key(key);
        }
        if self.filtering {
            return self.on_filter_key(key);
        }
        match self.page {
            Page::Help => self.on_help_key(key),
            Page::Detail(port) => self.on_detail_key(key, port),
            Page::List if self.cleaning => self.on_cleanup_key(key),
            Page::List => self.on_list_key(key),
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.keep_selection(None);
            }
            KeyCode::Esc => self.request_quit(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-5),
            KeyCode::PageDown => self.move_selection(5),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(port) = self.selected {
                    self.detail_offset = 0;
                    self.page = Page::Detail(port);
                }
            }
            KeyCode::Char('c') => self.start_cleanup(),
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.keep_selection(self.selected_index());
                self.say(if self.show_all { "Showing every listening port" } else { "Showing dev servers only" }, Tone::Info);
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('?') => self.show_help(),
            _ => {
                if let Some(port) = self.selected {
                    self.on_action_key(key, port);
                }
            }
        }
    }

    /// Keys that act on one server, shared by the list and details.
    fn on_action_key(&mut self, key: KeyEvent, port: u16) {
        let Some(server) = self.servers.iter().find(|s| s.port == port).cloned() else { return };
        let signals = matches!(key.code, KeyCode::Char('s' | 'x' | 'K' | 'r') | KeyCode::Delete);
        if signals && self.busy.contains_key(&port) {
            return self.say(format!(":{port} is already {}", self.busy[&port].trim_end_matches('…').to_lowercase()), Tone::Info);
        }
        let target = vec![(port, server.root_pid)];
        match key.code {
            KeyCode::Char('o') => {
                if !control::open(&server.url()) {
                    self.say("Couldn't open the browser", Tone::Bad);
                }
            }
            KeyCode::Char('s') | KeyCode::Char('x') | KeyCode::Delete => {
                self.confirm = Some(Confirm::Stop { targets: target, force: false });
            }
            KeyCode::Char('K') => self.confirm = Some(Confirm::Stop { targets: target, force: true }),
            KeyCode::Char('r') => {
                if control::can_restart(&server) {
                    self.confirm = Some(Confirm::Restart(port, server.root_pid));
                } else {
                    self.say("Can't restart: the original command or folder is gone", Tone::Bad);
                }
            }
            KeyCode::Char('y') => self.copy(&server.url(), "URL"),
            KeyCode::Char('Y') => match server.command.clone() {
                Some(command) => self.copy(&command, "command"),
                None => self.say("No command to copy", Tone::Bad),
            },
            KeyCode::Char('A') => match &server.agent {
                Some(agent) => self.copy(&agent.resume_command(), "resume command"),
                None => self.say("No agent session for this server", Tone::Info),
            },
            KeyCode::Char('e') => match server.project.root.as_ref().or(server.cwd.as_ref()) {
                Some(dir) => {
                    if !control::open(dir) {
                        self.say("Couldn't open the folder", Tone::Bad);
                    }
                }
                None => self.say("No folder for this server", Tone::Bad),
            },
            _ => {}
        }
    }

    fn copy(&mut self, text: &str, label: &str) {
        if control::copy(text) {
            self.say(format!("Copied {label}"), Tone::Good);
        } else {
            self.say("Couldn't copy to the clipboard", Tone::Bad);
        }
    }

    fn on_detail_key(&mut self, key: KeyEvent, port: u16) {
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.page = Page::List,
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Up | KeyCode::Char('k') => self.detail_offset = self.detail_offset.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.detail_offset = self.detail_offset.saturating_add(1),
            KeyCode::Enter => {
                let url = format!("http://localhost:{port}");
                if !control::open(&url) {
                    self.say("Couldn't open the browser", Tone::Bad);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.selected = Some(port);
                self.move_selection(if key.code == KeyCode::Tab { 1 } else { -1 });
                if let Some(next) = self.selected {
                    self.detail_offset = 0;
                    self.page = Page::Detail(next);
                }
            }
            KeyCode::Char('?') => self.show_help(),
            _ => self.on_action_key(key, port),
        }
    }

    fn start_cleanup(&mut self) {
        self.cleaning = true;
        // Preselect what's safe to stop; leaking servers are shown, not ticked.
        self.picked = self
            .visible()
            .iter()
            .filter(|s| s.cleanup_reason().is_some_and(|r| !r.is_leak()))
            .map(|s| (s.port, s.root_pid))
            .collect();
    }

    fn on_cleanup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('c') => {
                self.cleaning = false;
                self.picked.clear();
            }
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if let Some(port) = self.selected {
                    self.toggle_pick(port);
                }
            }
            KeyCode::Char('a') => {
                let all: HashMap<u16, i32> = self.visible().iter().map(|s| (s.port, s.root_pid)).collect();
                self.picked = if self.picked == all { HashMap::new() } else { all };
            }
            KeyCode::Enter if !self.picked.is_empty() => {
                let mut targets: Vec<(u16, i32)> =
                    self.picked.iter().map(|(p, r)| (*p, *r)).filter(|(p, _)| !self.busy.contains_key(p)).collect();
                targets.sort_unstable();
                self.confirm = Some(Confirm::Stop { targets, force: false });
            }
            KeyCode::Char('?') => self.show_help(),
            _ => {}
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) {
        let Some(confirm) = self.confirm.take() else { return };
        let accept = matches!(
            (&confirm, key.code),
            (_, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter)
                | (Confirm::Stop { force: false, .. }, KeyCode::Char('s'))
                | (Confirm::Stop { force: true, .. }, KeyCode::Char('K'))
                | (Confirm::Restart(..), KeyCode::Char('r'))
        );
        if !accept {
            return;
        }
        match confirm {
            Confirm::Stop { targets, force } => {
                for (port, root) in targets {
                    self.stop(port, root, force);
                }
                self.cleaning = false;
                self.picked.clear();
            }
            Confirm::Restart(port, root) => self.restart(port, root),
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
        self.keep_selection(None);
    }

    fn on_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.help_offset = self.help_offset.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.help_offset = self.help_offset.saturating_add(1),
            KeyCode::Char('q') => self.request_quit(),
            _ => self.page = self.return_page(),
        }
    }

    fn show_help(&mut self) {
        self.help_offset = 0;
        self.help_return = Some(self.page);
        self.page = Page::Help;
    }

    fn return_page(&mut self) -> Page {
        self.help_return.take().unwrap_or(Page::List)
    }

    fn on_scroll(&mut self, delta: isize) {
        match self.page {
            Page::List => self.move_selection(delta),
            Page::Detail(_) => self.detail_offset = self.detail_offset.saturating_add_signed(delta as i16),
            Page::Help => self.help_offset = self.help_offset.saturating_add_signed(delta as i16),
        }
    }

    fn on_click(&mut self, row: u16) {
        if self.page != Page::List || self.confirm.is_some() {
            return;
        }
        let Some(&(_, port)) = self.hits.iter().find(|(y, _)| *y == row) else { return };
        if self.cleaning {
            self.toggle_pick(port);
            self.selected = Some(port);
        } else if self.selected == Some(port) {
            self.detail_offset = 0;
            self.page = Page::Detail(port);
        } else {
            self.selected = Some(port);
        }
    }

    // MARK: Actions

    fn toggle_pick(&mut self, port: u16) {
        if self.picked.remove(&port).is_none()
            && let Some(server) = self.servers.iter().find(|s| s.port == port)
        {
            self.picked.insert(port, server.root_pid);
        }
    }

    fn request_quit(&mut self) {
        // A second quit leaves anyway; otherwise let stops reach their SIGKILL.
        if self.busy.is_empty() || self.quitting {
            self.quit = true;
        } else {
            self.quitting = true;
            self.say(format!("Quitting when {} finishes · q to quit now", plural_ports(self.busy.len())), Tone::Info);
        }
    }

    /// The server on `port`, if it's still the one with `root` behind it.
    fn same_server(&mut self, port: u16, root: i32) -> Option<Server> {
        let server = self.servers.iter().find(|s| s.port == port && s.root_pid == root).cloned();
        if server.is_none() {
            self.say(format!(":{port} changed since you chose it, so nothing was stopped"), Tone::Bad);
        }
        server
    }

    fn stop(&mut self, port: u16, root: i32, force: bool) {
        let Some(server) = self.same_server(port, root) else { return };
        let starts = server.starts;
        let (tx, rescan) = (self.tx.clone(), self.rescan.clone());
        self.busy.insert(port, if force { "Killing…" } else { "Stopping…" });
        thread::spawn(move || {
            let (text, tone) = if !control::stop(&starts, force) {
                (format!(":{port} is still running"), Tone::Bad)
            } else if !control::port_released(port) {
                (format!("Stopped the tree, but something else still holds :{port}"), Tone::Bad)
            } else {
                (format!("Stopped :{port}"), Tone::Good)
            };
            let _ = rescan.send(());
            let _ = tx.send(Msg::Done { port, text, tone });
        });
    }

    fn restart(&mut self, port: u16, root: i32) {
        let Some(server) = self.same_server(port, root) else { return };
        let (tx, rescan) = (self.tx.clone(), self.rescan.clone());
        self.busy.insert(port, "Restarting…");
        thread::spawn(move || {
            let (text, tone) = match control::restart(&server) {
                Ok(log) => (format!("Restarted :{port} · log in {log}"), Tone::Good),
                Err(error) => (error, Tone::Bad),
            };
            // Servers take a moment to bind again.
            let _ = rescan.send(());
            thread::sleep(Duration::from_millis(1500));
            let _ = rescan.send(());
            let _ = tx.send(Msg::Done { port, text, tone });
        });
    }
}

fn plural_ports(n: usize) -> String {
    if n == 1 { "1 stop".into() } else { format!("{n} stops") }
}
