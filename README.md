# wtp

Every dev server on your Mac, in the terminal. Stop the ones you forgot about.

A standalone terminal app, written in Rust for instant startup. It's modelled on the `wtp` view of [WhatThePort](https://github.com/tomjohndesign/what-the-port) and needs nothing else installed.

```
                                  Servers

   1.4 GB   3 servers                                             CPU  2.1%
   ██████▅▅▅▅▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂
   ▅ Servers 1.4 GB   ▂ Other apps 30.1 GB   ▂ Free 16.5 of 48.0 GB
   ─────────────────────────────────────────────────────────────────────
   ▌ :3000  feat/export-svg                              ▁▂▃▅▆█   1.21 GB
   ▌        ✳ paper plugins · Next.js 15 · up 3h

     :5173  main                                         ▁▁▁▁▁▁    184 MB
            web · Vite 6 · idle 5h
```

## Why it's fast

The original shells out to `lsof` on every scan, which takes about 45 ms before any of its own work. This version reads sockets, the process table, argv, environment, and memory straight from `libproc` and `sysctl`, and spawns no subprocess. A full scan takes about 5 ms. The first frame draws before the first scan finishes. Scans, stops, and restarts run on background threads, so keys never wait on them.

## Install

macOS on Apple Silicon or Intel:

```bash
curl -fsSL https://raw.githubusercontent.com/thetylerreiff/wtp/main/install.sh | sh
```

The script downloads the latest release for your Mac, checks its SHA-256, and installs `wtp` to `~/.local/bin` without sudo. Set `WTP_INSTALL_DIR` to put it elsewhere, or `WTP_VERSION=v0.1.0` to install a specific release:

```bash
curl -fsSL https://raw.githubusercontent.com/thetylerreiff/wtp/main/install.sh | WTP_INSTALL_DIR=/usr/local/bin sh
```

To download by hand, pick `wtp-aarch64-apple-darwin.tar.gz` (Apple Silicon) or `wtp-x86_64-apple-darwin.tar.gz` (Intel) from [Releases](https://github.com/thetylerreiff/wtp/releases/latest):

```bash
curl -fsSLO https://github.com/thetylerreiff/wtp/releases/latest/download/wtp-aarch64-apple-darwin.tar.gz
tar -xzf wtp-aarch64-apple-darwin.tar.gz
mv wtp ~/.local/bin/
```

To build from source with Rust 1.88 or later:

```bash
cargo install --git https://github.com/thetylerreiff/wtp
```

## Release

Push a version tag, and the [Release workflow](.github/workflows/release.yml) builds both architectures, runs clippy and the tests, and publishes the tarballs, their checksums and `install.sh`:

```bash
git tag v0.1.1 && git push origin v0.1.1
```

Bump `version` in `Cargo.toml` first so `wtp --version` matches.

## Use

```
wtp                      Browse, open and stop servers
wtp list [--all]         Print servers and exit
wtp list --json [--all]  Print servers as JSON, for scripts and agents
wtp kill <port>... [-9]  Stop whatever is listening on each port
```

In the TUI:

| Key | |
| --- | --- |
| `↑` `↓` / `j` `k` | Select |
| `⏎` | Details: folder, command, session, memory and CPU history, process tree |
| `o` | Open in browser |
| `s` | Stop: SIGTERM the whole process tree, then SIGKILL after 3 s |
| `K` | Kill: SIGKILL the whole process tree now |
| `r` | Restart with the same command, folder and environment |
| `c` | Clean up: tick servers and stop them together |
| `/` | Filter by port, name, branch or command |
| `a` | Every listening port, or dev servers only |
| `y` / `Y` / `A` | Copy URL, command, or agent resume command |
| `?` | All keys |

The mouse works too: click a row to select it, click again for details, and scroll.

## What it shows

- **Dev servers** are dev processes (node, bun, deno, python, ruby, go, cargo, java, php, and more) listening on port 3000 or above. Press `a` to see everything listening, such as Docker's port forwards and AirPlay on 5000/7000.
- **Names** come from the project manifest (package.json, pyproject.toml, Cargo.toml, go.mod, Gemfile), with the framework and git branch, including worktrees.
- **Agent sessions**: servers started by Claude Code (`CLAUDE_CODE_SESSION_ID`), Codex (`~/.codex/sessions`), or Conductor link back to that session.
- **Memory and CPU** are summed across the whole process tree, with 10 minutes of history. Rows turn amber over 2 GB or after growing 500 MB.
- **Clean up** preselects servers from deleted worktrees, idle for 4 h, or up for 3 days. Growing servers are listed but not ticked. Postgres, Redis, MongoDB and MySQL are never preselected.

## Safety

- Each pid is checked against its start time before it's signalled, so a reused pid is never hit.
- Stopping walks up from the listener to the command you ran (`npm run dev`), and stops at shells, terminals and coding agents. Stopping a server never takes its launcher with it.
- When one process holds several ports (Docker, for one), the confirmation names the other ports it will close.
- Restarted servers run in their own session, detached from your terminal. They log to `~/Library/Logs/wtp/port-<port>.log`.

Like `lsof` without `sudo`, it only sees processes owned by you.

## License

MIT. Modelled on [WhatThePort](https://github.com/tomjohndesign/what-the-port) by Tomjohn, also MIT.
