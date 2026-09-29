//! Works out what a server is for from its working directory: the project
//! manifest, framework, git branch and whether it lives in a worktree.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Project {
    pub name: String,
    pub root: Option<String>,
    pub framework: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
}

struct Cached {
    project: Project,
    git_head: Option<String>,
    at: Instant,
}

#[derive(Default)]
pub struct ProjectResolver {
    cache: HashMap<String, Cached>,
}

impl ProjectResolver {
    pub fn resolve(&mut self, cwd: Option<&str>, command: Option<&str>) -> Project {
        let Some(cwd) = cwd else {
            return Project { name: "Unknown".into(), ..Default::default() };
        };
        if let Some(cached) = self.cache.get(cwd).filter(|c| c.at.elapsed() < Duration::from_secs(15)) {
            let mut project = cached.project.clone();
            // Branches change often; HEAD is cheap to re-read every scan.
            if let Some(head) = &cached.git_head {
                project.branch = branch_from_head(head);
            }
            return project;
        }

        let home = crate::home();
        let mut project = Project { name: file_name(cwd), ..Default::default() };
        let mut git_head = None;
        let mut found_manifest = false;
        let mut dir = Some(Path::new(cwd));
        while let Some(path) = dir {
            let text = path.to_string_lossy();
            if text == "/" || text == home {
                break;
            }
            if !found_manifest && let Some((name, framework)) = read_manifest(path, command) {
                project.name = name.unwrap_or_else(|| file_name(&text));
                project.framework = framework;
                project.root = Some(text.to_string());
                found_manifest = true;
            }
            if git_head.is_none() && let Some((head, is_worktree)) = git_head_path(path) {
                git_head = Some(head);
                if is_worktree {
                    project.worktree = Some(file_name(&text));
                }
                project.root.get_or_insert_with(|| text.to_string());
            }
            if found_manifest && git_head.is_some() {
                break;
            }
            dir = path.parent();
        }
        if let Some(head) = &git_head {
            project.branch = branch_from_head(head);
        }
        self.cache.insert(cwd.to_string(), Cached { project: project.clone(), git_head, at: Instant::now() });
        project
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn read_manifest(dir: &Path, command: Option<&str>) -> Option<(Option<String>, Option<String>)> {
    let command = command.unwrap_or("").to_lowercase();
    if let Ok(data) = fs::read(dir.join("package.json")) {
        let json: serde_json::Value = serde_json::from_slice(&data).unwrap_or_default();
        let name = json["name"].as_str().map(String::from);
        let dep = |key: &str| -> Option<String> {
            ["dependencies", "devDependencies"].iter().find_map(|section| json[section][key].as_str().map(String::from))
        };
        if command.contains("storybook") {
            return Some((name, Some(labelled("Storybook", dep("storybook")))));
        }
        let candidates = [
            ("next", "Next.js"),
            ("nuxt", "Nuxt"),
            ("@remix-run/dev", "Remix"),
            ("astro", "Astro"),
            ("@sveltejs/kit", "SvelteKit"),
            ("expo", "Expo"),
            ("@angular/core", "Angular"),
            ("vite", "Vite"),
            ("react-scripts", "Create React App"),
            ("hono", "Hono"),
            ("express", "Express"),
        ];
        let framework = candidates.iter().find_map(|(key, label)| dep(key).map(|v| labelled(label, Some(v))));
        return Some((name, framework));
    }
    if let Ok(text) = fs::read_to_string(dir.join("pyproject.toml")) {
        let name = toml_name(&text, "project").or_else(|| toml_name(&text, "tool.poetry"));
        let haystack = command + &text.to_lowercase();
        let framework = [("django", "Django"), ("fastapi", "FastAPI"), ("flask", "Flask"), ("uvicorn", "Uvicorn"), ("gunicorn", "Gunicorn")]
            .iter()
            .find(|(key, _)| haystack.contains(key))
            .map_or("Python", |(_, label)| label);
        return Some((name, Some(framework.into())));
    }
    if let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) {
        return Some((toml_name(&text, "package"), Some("Rust".into())));
    }
    if let Ok(text) = fs::read_to_string(dir.join("go.mod")) {
        let module = text.lines().find_map(|l| l.strip_prefix("module ")).map(|m| m.trim().rsplit('/').next().unwrap_or(m).to_string());
        return Some((module, Some("Go".into())));
    }
    if dir.join("Gemfile").exists() {
        let rails = dir.join("config/application.rb").exists();
        return Some((None, Some(if rails { "Rails" } else { "Ruby" }.into())));
    }
    None
}

/// "Next.js 15" from "^15.2.0".
fn labelled(name: &str, version: Option<String>) -> String {
    let major: String = version
        .unwrap_or_default()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if major.is_empty() { name.to_string() } else { format!("{name} {major}") }
}

fn toml_name(text: &str, section: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut in_section = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line == header;
            continue;
        }
        if in_section && line.starts_with("name") {
            let rest = line.split_once('=')?.1.trim();
            return Some(rest.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    None
}

/// Path to the HEAD file, and whether the directory is a linked worktree.
fn git_head_path(dir: &Path) -> Option<(String, bool)> {
    let git = dir.join(".git");
    let meta = fs::metadata(&git).ok()?;
    if meta.is_dir() {
        return Some((git.join("HEAD").to_string_lossy().into_owned(), false));
    }
    // Worktrees and submodules have a `.git` file pointing at the real git dir.
    let contents = fs::read_to_string(&git).ok()?;
    let gitdir = contents.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let gitdir = if gitdir.starts_with('/') { gitdir.to_string() } else { dir.join(gitdir).to_string_lossy().into_owned() };
    Some((format!("{gitdir}/HEAD"), gitdir.contains("/worktrees/")))
}

fn branch_from_head(head: &str) -> Option<String> {
    let text = fs::read_to_string(head).ok()?;
    let text = text.trim();
    if let Some(branch) = text.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    (!text.is_empty()).then(|| text.chars().take(7).collect())
}
