//! Stopping, killing and restarting servers, plus the small shell-outs for
//! opening URLs and copying text.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::scan::Server;
use crate::sys;

pub const FORCE_QUIT_AFTER: Duration = Duration::from_secs(3);

/// Sends SIGTERM to every process in the tree, waits up to the grace period
/// for them to exit, then SIGKILLs whatever is left. With `force`, SIGKILL
/// straight away. Blocks; returns true when every process is gone.
///
/// The tree is re-read from the kernel before each signal, so children
/// spawned since the scan (a watcher's respawn, new workers) go too. wtp and
/// its ancestors are never signalled.
pub fn stop(targets: &[(i32, u64)], force: bool) -> bool {
    let lineage = sys::own_lineage();
    let mut targets = current_tree(targets, &lineage);
    if !force {
        for &(pid, _) in &targets {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let deadline = Instant::now() + FORCE_QUIT_AFTER;
        while Instant::now() < deadline {
            if alive(&targets).is_empty() {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        targets = current_tree(&targets, &lineage);
    }
    for &(pid, _) in &targets {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    for _ in 0..20 {
        if alive(&targets).is_empty() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn alive(targets: &[(i32, u64)]) -> Vec<(i32, u64)> {
    targets.iter().copied().filter(|&(pid, start)| sys::is_same_process(pid, start)).collect()
}

/// The targets still running, plus their descendants as of now, minus wtp's lineage.
fn current_tree(targets: &[(i32, u64)], lineage: &HashSet<i32>) -> Vec<(i32, u64)> {
    let processes = sys::all_processes();
    let mut children: HashMap<i32, Vec<(i32, u64)>> = HashMap::new();
    for p in processes.values() {
        children.entry(p.ppid).or_default().push((p.pid, p.start));
    }
    let mut tree: Vec<(i32, u64)> =
        targets.iter().copied().filter(|&(pid, start)| processes.get(&pid).is_some_and(|p| p.start == start)).collect();
    let mut index = 0;
    while index < tree.len() {
        for &child in children.get(&tree[index].0).into_iter().flatten() {
            if !tree.contains(&child) {
                tree.push(child);
            }
        }
        index += 1;
    }
    tree.retain(|(pid, _)| *pid > 1 && !lineage.contains(pid));
    tree
}

/// After a stop, waits briefly for the port to be released. False means
/// something else (a respawned child, a supervisor) still holds it.
pub fn port_released(port: u16) -> bool {
    for _ in 0..10 {
        if !port_in_use(port) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

pub fn can_restart(server: &Server) -> bool {
    restart_plan(server).is_some()
}

fn restart_plan(server: &Server) -> Option<(String, String)> {
    // Rerunning an app's binary from a shell is never what anyone wants.
    if server.app.is_some() {
        return None;
    }
    let launch = server.launch.as_ref()?;
    let dir = server.launch_dir.clone().or_else(|| server.cwd.clone())?;
    if !std::path::Path::new(&dir).exists() {
        return None;
    }
    Some((shell_command(&launch.args, &launch.exe)?, dir))
}

/// Stops the server and reruns the same command in the same directory with
/// the same environment, detached from this terminal. Output goes to
/// ~/Library/Logs/wtp/port-<port>.log.
pub fn restart(server: &Server) -> Result<String, String> {
    let (command, dir) = restart_plan(server).ok_or("Can't restart: the original command or folder is gone")?;
    if !stop(&server.starts, false) {
        return Err(format!(":{} is still running, so it wasn't restarted", server.port));
    }
    // Give the kernel a moment to release the port.
    for _ in 0..20 {
        if !port_in_use(server.port) {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let logs = format!("{}/Library/Logs/wtp", crate::home());
    let _ = fs::create_dir_all(&logs);
    let log_path = format!("{logs}/port-{}.log", server.port);
    let log = fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let mut cmd = Command::new("/bin/zsh");
    cmd.arg("-c").arg(&command).current_dir(&dir).stdin(Stdio::null());
    cmd.stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    if let Some(env) = server.launch.as_ref().map(|l| &l.env).filter(|e| !e.is_empty()) {
        cmd.env_clear().envs(env);
    }
    // A new session, so quitting wtp or closing the terminal doesn't take the server with it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // Reap the shell when it exits, so it doesn't linger as a zombie.
    thread::spawn(move || child.wait());
    Ok(crate::format::tilde(&log_path))
}

/// Something accepts connections on the port, over IPv4 or IPv6 loopback.
pub fn port_in_use(port: u16) -> bool {
    [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]
        .into_iter()
        .any(|ip| TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_millis(100)).is_ok())
}

/// The command line to rerun. Tools like npm and Next.js overwrite argv with
/// a title (`npm run dev`), which is what was typed, so that runs through a
/// shell as-is. Intact argv is re-quoted.
pub fn shell_command(args: &[String], exe: &str) -> Option<String> {
    let args: Vec<&str> = args.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
    let first = *args.first()?;
    let name = first.rsplit('/').next().unwrap_or(first);
    if ["sh", "bash", "zsh", "dash"].contains(&name) && args.len() >= 3 && args[1] == "-c" {
        return Some(args[2].to_string());
    }
    if args.len() == 1 && first.contains(' ') {
        return Some(first.split(" (").next().unwrap_or(first).to_string());
    }
    let executable = if first.starts_with('/') { first } else { exe };
    Some(std::iter::once(executable).chain(args[1..].iter().copied()).map(shell_quote).collect::<Vec<_>>().join(" "))
}

pub fn shell_quote(value: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c);
    if !value.is_empty() && value.chars().all(safe) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

pub fn open(target: &str) -> bool {
    Command::new("/usr/bin/open").arg(target).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub fn copy(text: &str) -> bool {
    let Ok(mut child) = Command::new("/usr/bin/pbcopy").stdin(Stdio::piped()).spawn() else { return false };
    let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    written && child.wait().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reruns_what_was_typed() {
        assert_eq!(shell_command(&args(&["sh", "-c", "next dev -p 3000"]), "/bin/sh").as_deref(), Some("next dev -p 3000"));
        assert_eq!(shell_command(&args(&["npm run dev"]), "/usr/local/bin/node").as_deref(), Some("npm run dev"));
        assert_eq!(shell_command(&args(&["next-server (v15.0.0)"]), "/usr/local/bin/node").as_deref(), Some("next-server"));
        assert_eq!(
            shell_command(&args(&["node", "server.js", "--name", "a b"]), "/opt/homebrew/bin/node").as_deref(),
            Some("/opt/homebrew/bin/node server.js --name 'a b'")
        );
    }

    #[test]
    fn stopping_our_own_tree_spares_us_and_takes_new_children() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().expect("spawn sleep");
        let me = sys::snapshot(std::process::id() as i32).expect("own snapshot");
        let child_start = sys::snapshot(child.id() as i32).expect("child snapshot").start;
        // The target list names only this process; its child is found at stop time.
        assert!(stop(&[(me.pid, me.start)], true));
        let _ = child.wait();
        assert!(!sys::is_same_process(child.id() as i32, child_start));
        assert!(sys::snapshot(me.ppid).is_some(), "the test runner must survive");
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("--port=3000"), "--port=3000");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
