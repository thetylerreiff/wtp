//! Links a server to the coding-agent session that started it. Claude Code
//! exports `CLAUDE_CODE_SESSION_ID` to every command it runs; Codex sessions
//! are matched by working directory against `~/.codex/sessions`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Codex => "Codex",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "✳",
            AgentKind::Codex => ">_",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub kind: AgentKind,
    pub id: String,
    pub title: Option<String>,
    /// Where the agent was started, used to resume it.
    pub directory: Option<String>,
}

impl Agent {
    pub fn resume_command(&self) -> String {
        let resume = match self.kind {
            AgentKind::ClaudeCode => format!("claude --resume {}", self.id),
            AgentKind::Codex => format!("codex resume {}", self.id),
        };
        match &self.directory {
            Some(dir) => format!("cd {} && {resume}", crate::control::shell_quote(dir)),
            None => resume,
        }
    }
}

#[derive(Default)]
pub struct AgentResolver {
    claude: HashMap<String, (Agent, Instant)>,
    codex: HashMap<String, Agent>,
    codex_indexed: Option<Instant>,
}

impl AgentResolver {
    pub fn resolve(&mut self, env: &HashMap<String, String>, cwd: Option<&str>) -> Option<Agent> {
        if let Some(id) = env.get("CLAUDE_CODE_SESSION_ID").filter(|id| !id.is_empty()) {
            return Some(self.claude_session(id));
        }
        // A server started by Claude Code shouldn't be claimed by a Codex session in the same folder.
        if env.contains_key("CLAUDE_CODE_SESSION_ID") {
            return None;
        }
        // Walk up from the server's directory, but never match a session
        // started in the home folder or above; that would claim every server.
        let home = crate::home();
        let mut dir = Path::new(cwd?);
        while dir.as_os_str().len() > home.len() {
            if let Some(agent) = self.codex.get(dir.to_str()?) {
                return Some(agent.clone());
            }
            dir = dir.parent()?;
        }
        None
    }

    pub fn needs_codex_index(&self) -> bool {
        self.codex_indexed.is_none_or(|at| at.elapsed() > Duration::from_secs(60))
    }

    fn claude_session(&mut self, id: &str) -> Agent {
        if let Some((agent, _)) = self.claude.get(id).filter(|(_, at)| at.elapsed() < Duration::from_secs(30)) {
            return agent.clone();
        }
        let mut agent = Agent { kind: AgentKind::ClaudeCode, id: id.to_string(), title: None, directory: None };
        if let Some(transcript) = find_claude_transcript(id) {
            let head = read_head(&transcript, 256 * 1024);
            // Titles are appended as the session goes, so the newest is near the end.
            agent.title = last_title(&read_tail(&transcript, 512 * 1024))
                .or_else(|| last_title(&head))
                .or_else(|| first_prompt(&head));
            agent.directory = head
                .lines()
                .filter(|l| l.contains("\"cwd\""))
                .find_map(|l| json(l)?["cwd"].as_str().map(String::from));
        }
        self.claude.insert(id.to_string(), (agent.clone(), Instant::now()));
        agent
    }

    /// Indexes Codex sessions from the last three days by working directory.
    pub fn refresh_codex(&mut self) {
        self.codex_indexed = Some(Instant::now());
        let codex = PathBuf::from(crate::home()).join(".codex");
        let mut titles = HashMap::new();
        for line in read_tail(&codex.join("session_index.jsonl"), 1024 * 1024).lines() {
            if let Some(v) = json(line)
                && let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str())
            {
                titles.insert(id.to_string(), name.to_string());
            }
        }

        let cutoff = SystemTime::now() - Duration::from_secs(3 * 24 * 3600);
        let mut index: HashMap<String, (Agent, SystemTime)> = HashMap::new();
        let mut stack = vec![codex.join("sessions")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(modified) = meta.modified() else { continue };
                if modified < cutoff || path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let head = read_head(&path, 64 * 1024);
                let Some(first) = head.lines().next().and_then(json) else { continue };
                let payload = &first["payload"];
                let (Some(id), Some(cwd)) = (payload["id"].as_str(), payload["cwd"].as_str()) else { continue };
                if index.get(cwd).is_some_and(|(_, m)| *m > modified) {
                    continue;
                }
                let agent = Agent {
                    kind: AgentKind::Codex,
                    id: id.to_string(),
                    title: titles.get(id).cloned(),
                    directory: Some(cwd.to_string()),
                };
                index.insert(cwd.to_string(), (agent, modified));
            }
        }
        self.codex = index.into_iter().map(|(cwd, (agent, _))| (cwd, agent)).collect();
    }
}

fn find_claude_transcript(id: &str) -> Option<PathBuf> {
    let projects = PathBuf::from(crate::home()).join(".claude/projects");
    fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|folder| folder.path().join(format!("{id}.jsonl")))
        .find(|candidate| candidate.exists())
}

fn last_title(text: &str) -> Option<String> {
    let mut title = None;
    for line in text.lines().filter(|l| l.contains("\"type\":\"custom-title\"") || l.contains("\"type\":\"ai-title\"")) {
        let Some(v) = json(line) else { continue };
        if let Some(custom) = v["customTitle"].as_str().filter(|s| !s.is_empty()) {
            return Some(custom.to_string());
        }
        if let Some(ai) = v["aiTitle"].as_str().filter(|s| !s.is_empty()) {
            title = Some(ai.to_string());
        }
    }
    title
}

fn first_prompt(text: &str) -> Option<String> {
    text.lines().filter(|l| l.contains("\"type\":\"user\"")).find_map(|line| {
        let v = json(line)?;
        let content = v["message"]["content"].as_str()?.trim();
        if content.is_empty() || content.starts_with('<') {
            return None;
        }
        Some(content.lines().next()?.chars().take(60).collect())
    })
}

fn json(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str(line).ok()
}

fn read_head(path: &Path, bytes: u64) -> String {
    let mut data = Vec::new();
    if let Ok(file) = File::open(path) {
        let _ = file.take(bytes).read_to_end(&mut data);
    }
    String::from_utf8_lossy(&data).into_owned()
}

fn read_tail(path: &Path, bytes: u64) -> String {
    let Ok(mut file) = File::open(path) else { return String::new() };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let offset = size.saturating_sub(bytes);
    let mut data = Vec::new();
    if file.seek(SeekFrom::Start(offset)).is_ok() {
        let _ = file.read_to_end(&mut data);
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    // Drop the partial first line when we started mid-file.
    match (offset > 0, text.find('\n')) {
        (true, Some(newline)) => text[newline + 1..].to_string(),
        _ => text,
    }
}
