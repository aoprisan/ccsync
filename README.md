# ccsync

Sync and back up your **Claude Code** settings, sessions, and memory across
machines.

Claude Code keeps its state in `~/.claude/` (and `~/.claude.json`). Some of it
is portable (settings, `CLAUDE.md`, skills, agents, commands), some is
machine-specific (session transcripts whose directory names encode absolute
working-directory paths), and some is **sensitive and must never leave the
machine** (`~/.claude/.credentials.json`, OAuth tokens). A plain `rsync` of
`~/.claude` either leaks credentials or produces sessions that don't resolve on
the target machine.

`ccsync` solves this by taking a **sanitized, manifested snapshot**, transporting
it over a **git remote** or an **encrypted archive**, and **remapping absolute
paths** on restore so your conversation history shows up correctly on the new
machine.

## What gets synced

**Included (portable):**
`settings.json`, `CLAUDE.md`, `keybindings.json`, and the `rules/`, `skills/`,
`commands/`, `agents/`, `agent-memory/`, `output-styles/`, `workflows/`,
`themes/` directories, plus `plugins/` configuration (the re-fetchable plugin
checkouts under `plugins/repos`, `plugins/cache`, and `plugins/marketplaces`
are excluded).

**Included (sessions, with path remapping):**
`projects/<encoded-path>/*.jsonl` transcripts, per-repo `memory/`, and the
per-session `todos/` state (both gated by `include_sessions`).

**Included (local MCP servers):**
The `mcpServers` definitions configured on this machine — both **user scope**
(the top-level `mcpServers` in `~/.claude.json`) and **local scope** (each
project's `projects.<cwd>.mcpServers`). They are extracted into a standalone
`mcp-servers.json` inside the snapshot; on restore they are merged back into the
local `~/.claude.json` with project paths remapped just like session
directories. The rest of `~/.claude.json` is never read or written. Disable with
`include_mcp_servers = false`. Project-scope servers (a repo's own `.mcp.json`)
are not touched — they already travel with their repository.

**Never synced:**
`.credentials.json` (hard-blocked), plus machine-local/cache state
(`shell-snapshots/`, `session-env/`, `backups/`, `statsig/`, `ide/`,
`settings.local.json`, `launcher-settings.json`, `policy-limits.json`,
`remote-settings.json`, plugin checkouts). `~/.claude.json` is never synced
wholesale because it embeds OAuth tokens and per-project trust decisions; only
its `mcpServers` definitions are bundled (see above).

Anything under `~/.claude` that appears in **neither** list is not captured;
`ccsync status` prints a warning naming such entries so new Claude Code state
is surfaced instead of silently dropped — add them to `include` or `exclude`
to classify them.

Config files — including the extracted MCP server definitions — are scanned for
secret-shaped strings (API keys, tokens) before inclusion; a match aborts the
snapshot unless you pass `--allow-secrets`. A server `env` holding a literal API
key will therefore abort by default.

Session transcripts get a separate policy (`transcript_secrets`), because
conversations legitimately discuss secrets: by default each secret-shaped span
is rewritten to `[REDACTED:ccsync]` **in the staged copy only** — the files in
`~/.claude` are never modified. Set `transcript_secrets = "abort"` for the
config-file behavior or `"ignore"` to capture transcripts verbatim. The
scanner is best-effort pattern matching, not a guarantee.

## Install

```sh
cargo install --path .
# or
cargo build --release   # binary at target/release/ccsync
```

## Quickstart

**On the source machine:**

```sh
# Configure a git remote (a private repo you control) for sync + versioned backup.
ccsync init --remote git@github.com:you/claude-backup.git

# Snapshot ~/.claude and push it.
ccsync backup
```

**On the target machine:**

```sh
ccsync init --remote git@github.com:you/claude-backup.git
ccsync pull
ccsync restore         # backs up the existing ~/.claude first, then applies + remaps
```

### Offline / portable backup (encrypted archive)

```sh
export CCSYNC_PASSPHRASE='a-strong-passphrase'

# Source machine: write a single encrypted file you can copy anywhere.
ccsync export claude-backup.tar.gz.age

# Target machine:
ccsync import claude-backup.tar.gz.age
ccsync restore
```

## Commands

| Command | Description |
|---------|-------------|
| `ccsync init [--remote URL]` | Write the default config to `~/.config/ccsync/config.toml`. |
| `ccsync snapshot [--dry-run] [--allow-secrets]` | Capture a sanitized snapshot into the staging dir. |
| `ccsync status` | Show what a snapshot would capture (dry run). |
| `ccsync push [--remote URL] [--archive FILE]` | Publish the staged snapshot (git by default). |
| `ccsync pull [--remote URL] [--archive FILE] [--from MACHINE] [--at COMMIT]` | Fetch a snapshot into staging (another machine's, or a past commit's). |
| `ccsync restore [--dry-run] [--no-remap] [--overwrite] [--only COMPONENTS] [--yes]` | Apply the staged snapshot to `~/.claude` (optionally only named components). |
| `ccsync diff [--remote [--from MACHINE]]` | Show how local `~/.claude` differs from the staged snapshot (or the remote's, manifest-only). |
| `ccsync history [--limit N]` | List snapshot commits on the remote, newest first. |
| `ccsync machines` | List every machine with a snapshot on the remote. |
| `ccsync rollback COMMIT [--only COMPONENTS] [--yes]` | Restore `~/.claude` from a past snapshot commit. |
| `ccsync layer list\|pull\|apply` | Pull and apply read-only shared layers (see [Shared layers](#shared-layers)). |
| `ccsync export FILE` | One-shot snapshot → encrypted archive. |
| `ccsync import FILE` | Encrypted archive → staging. |
| `ccsync backup [--remote URL] [--archive FILE]` | `snapshot` + `push`. |
| `ccsync profile list\|create\|switch\|show\|diff\|delete\|rollback` | Manage named profiles (see [Profiles](#profiles)). |
| `ccsync tui` | Launch an interactive terminal UI: review what would be backed up, browse local backups, and push/export. |
| `ccsync daemon` | Run the background backup loop in the foreground (used by the installed service). |
| `ccsync service install\|uninstall` | Register/remove an OS service (systemd user unit / launchd agent). |
| `ccsync service start\|stop` | Run the daemon detached in the background (nohup-style; no service manager). |
| `ccsync service status` | Report whether the service is installed and/or running. |

## Profiles

Profiles turn ccsync into an environment manager for Claude Code: named
overlays (work, personal, client-X) over the shared base state.

A profile **owns** a component set — by default `settings.json`, `CLAUDE.md`,
`agents/`, `skills/`, `commands/`, `output-styles/`, plus the **user-scope**
`mcpServers` of `~/.claude.json`. Everything else (sessions, agent memory,
keybindings, todos) is shared and untouched by switching. Per-project MCP
servers are tied to directories, not environments, and are never touched.

```sh
ccsync profile create work --from-current   # seed a profile from what you have now
ccsync profile create personal              # start another one empty
ccsync profile switch personal              # swap owned components + user MCP servers
# ...configure Claude Code as "personal"...
ccsync profile switch work                  # personal's edits are captured back, work returns
ccsync profile diff work                    # what changed live vs. work's store?
ccsync profile rollback                     # undo the last switch
```

How a switch works:

1. the currently-active profile's live components are **captured back** into
   its store, so edits made while it was active are never lost;
2. if the target profile's `settings.json` would install hook commands you
   don't already have, they are printed and must be confirmed (hooks are
   arbitrary shell commands — `--yes` to skip);
3. the affected live components are backed up to a timestamped
   `~/.claude.ccsync-profile-backup-<ts>` directory;
4. the target profile's components are swapped in wholesale and its user-scope
   MCP servers replace the current set (other `~/.claude.json` keys are
   preserved). A failure mid-apply automatically restores the backup.

Profile stores live under `<config>/ccsync/profiles/<name>/` and — with the
default `sync = true` — ride along inside snapshots, so `backup` on one
machine and `pull` + `restore` on another moves your profiles too (the
active-profile pointer stays machine-local).

```toml
[profiles]
components = ["settings.json", "CLAUDE.md", "agents", "skills", "commands", "output-styles"]
include_user_mcp = true
sync = true
```

## Multiple machines, history, and rollback

Each machine owns a `machines/<machine-id>/` subtree in the sync repo, so
several machines can share one remote without overwriting each other. The
identity defaults to the hostname and is recorded as `machine_id` in the
config on your first push (set it yourself to survive hostname changes).

```sh
ccsync machines                 # who has pushed snapshots, and when
ccsync pull --from laptop       # deliberately stage another machine's snapshot
ccsync diff --remote            # how does local state differ from my last push?
ccsync diff --remote --from laptop   # ...or from another machine's?

ccsync history                  # snapshot commits, newest first
ccsync pull --at <commit>       # stage the snapshot as of a past commit
ccsync rollback <commit>        # pull --at + restore in one step
```

`pull` with no `--from` uses this machine's own subtree (or the only one
present). Rollback and `pull --at` verify manifest integrity like any other
restore, so an old or tampered snapshot cannot slip past the checks.

Per-machine config tweaks live under `[machines.<id>]` and fold into the base
config only on that machine:

```toml
[machines.laptop]
exclude_extra = ["projects"]        # don't sync sessions from the laptop
[machines.laptop.remap]
"/Volumes/src" = "/home/you/src"
```

## Shared layers

A layer is a read-only git repo — typically a team's shared `skills/` and
`commands/` — declared in the config and applied beneath your own state:

```toml
[[layers]]
name = "team"
remote = "git@github.com:acme/claude-shared.git"
components = ["skills", "commands"]
```

```sh
ccsync layer list     # configured layers + checkout state
ccsync layer pull     # clone/update the checkouts
ccsync layer apply team
```

`apply` copies only the declared components and treats the repo as untrusted
input: credential files are hard-blocked, text files are secret-scanned, and
a layer `settings.json` that would install new hook commands requires the
same confirmation as a restore. ccsync never pushes to a layer.

## Background service

Instead of running `ccsync backup` by hand, you can have ccsync back up
automatically on a fixed interval. Configure the `[service]` table, then install
the OS service:

```toml
[service]
enabled = true
interval_minutes = 60
destination = "git"        # or "archive"
# backup_dir = "/home/you/.config/ccsync/backups"  # archive destination only
allow_secrets = false
```

```sh
ccsync service install     # writes + enables a systemd user unit (Linux) or launchd agent (macOS)
ccsync service status
ccsync service uninstall
```

`install` writes the unit (`~/.config/systemd/user/ccsync.service` on Linux,
`~/Library/LaunchAgents/com.ccsync.daemon.plist` on macOS) and tries to enable
it; if the service manager isn't reachable it prints the manual command. The
unit just runs `ccsync daemon`, so you can also run that directly under your own
supervisor, cron, or Task Scheduler.

### Detached mode (no service manager)

If you don't want to register an OS service, run the daemon detached instead —
the same idea as `nohup ccsync daemon &` or a `screen`/`tmux` session, but
managed for you:

```sh
ccsync service start    # forks the daemon, detaches from the terminal (survives logout)
ccsync service status   # is it running?
ccsync service stop     # SIGTERM the recorded PID
tail -f ~/.config/ccsync/daemon.log   # follow its output
```

`start` writes the PID to `~/.config/ccsync/daemon.pid` and redirects output to
`~/.config/ccsync/daemon.log`. It refuses to start a second copy while one is
running, and clears a stale pidfile if the recorded process is gone. (Unix
only; on Windows use `ccsync daemon` under your own supervisor.)

Each tick builds a sanitized snapshot and publishes it to `destination`:

- **`git`** — pushes to the configured `remote`, exactly like `ccsync push`.
- **`archive`** — writes a timestamped `claude-backup-<ts>.tar.gz.age` into
  `backup_dir` (default `~/.config/ccsync/backups`). These appear in `ccsync tui`.

**Things to know:**

- **Secrets aren't inherited by the service.** A systemd user unit / launchd
  agent does not see your shell environment. The installed unit sources
  `<config>/ccsync/service.env` (optional, create it with mode 600), so the
  `archive` destination works by putting `CCSYNC_PASSPHRASE=...` there; the
  `git` destination needs SSH keys / a credential helper the agent can reach
  (an HTTPS remote with a stored credential is simplest). `install` prints the
  exact command; ccsync never writes your secret itself.
- **`allow_secrets = false` (default) makes a tick fail closed** — if a config
  file looks like it contains a secret the snapshot aborts and the error is
  logged; the daemon keeps running and retries next interval.
- **The `archive` destination accumulates files** — one per tick, with no
  automatic pruning. Point `backup_dir` somewhere you can manage, or prefer the
  `git` destination (which is a cheap no-op when nothing changed).
- On Linux, `systemctl --user` needs a user session bus; on a headless box you
  may need `loginctl enable-linger $USER` first.

## How path remapping works

Claude Code names each project's session directory after the absolute working
directory, replacing `/` with `-` (e.g. `/Users/alice/proj` →
`-Users-alice-proj`), and embeds that path in each transcript's `cwd` field.

On `restore`, ccsync rewrites these using the snapshot manifest's recorded
`source_home`:

1. **Automatic:** the source machine's home directory is mapped to the local
   home (e.g. `/Users/alice` → `/home/bob`).
2. **Explicit:** add pairs to the `[remap]` table in `config.toml` for checkouts
   that live at different paths, e.g.
   ```toml
   [remap]
   "/Users/alice/work" = "/srv/work"
   ```

Longer (more specific) source prefixes win. Pass `--no-remap` to restore
transcripts verbatim on a same-path machine.

## Safety

- **Credentials never leave the machine** — `.credentials.json` is hard-blocked
  in the capture path regardless of configuration (including profile stores).
- **Snapshots are integrity-checked** — every captured file's SHA-256 is
  recorded in the manifest, and `restore` verifies the staged data against it
  (both directions, plus path-safety checks) before touching anything. Archive
  extraction refuses absolute paths, `..`, and link entries.
- **Incoming hooks require confirmation** — a restored or profile-switched
  `settings.json` can carry `hooks`, which are arbitrary shell commands Claude
  Code will execute on this machine. New or changed hook commands are printed
  and must be confirmed; non-interactive runs fail closed (`--yes` to accept,
  `confirm_hooks = false` to disable the check).
- **Archives are always encrypted** with [age](https://age-encryption.org/)
  using `CCSYNC_PASSPHRASE`; there is no plaintext mode.
- **`restore` is reversible** — it backs up the existing `~/.claude` to a
  timestamped `~/.claude.ccsync-backup-<ts>` directory before writing, supports
  `--dry-run`, and deep-merges `settings.json` by default (`--overwrite` to
  replace; scalar arrays like `permissions.allow` are unioned so locally-added
  entries survive). When MCP servers are bundled, `~/.claude.json` is likewise
  copied to a timestamped `~/.claude.json.ccsync-backup-<ts>` before its
  `mcpServers` are merged.
- **Git remotes are restricted to real transports** (ssh/https/http/file) —
  exotic schemes like `ext::` that execute commands are refused.

## Configuration

`<config>/ccsync/config.toml` (created by `ccsync init`) controls the
`include`/`exclude` sets, `include_sessions`, `include_mcp_servers`,
`transcript_secrets` (`"redact"` default / `"abort"` / `"ignore"`),
`confirm_hooks`, the git `remote`, `machine_id`, the `[remap]` table, the
`[service]` table (see [Background service](#background-service)), the
`[profiles]` table (see [Profiles](#profiles)), per-machine `[machines.<id>]`
overrides, and `[[layers]]` entries (see [Shared layers](#shared-layers)).
`CLAUDE_CONFIG_DIR` is honored when locating the source directory.

> **Where is `<config>`?** All of ccsync's own files (config, staging,
> backups, repo cache, daemon pid/log) live under your platform config
> directory: `~/Library/Application Support/ccsync/` on macOS and
> `~/.config/ccsync/` on Linux (or `$XDG_CONFIG_HOME/ccsync/` when set). The
> `~/.config/ccsync/...` paths shown elsewhere in this README are the Linux
> form; substitute the macOS location accordingly.

- **`include_mcp_servers`** (default `true`) — bundle the locally-configured MCP
  servers from `~/.claude.json` and merge them back on restore. Set to `false`
  to leave MCP configuration out of the snapshot entirely.
