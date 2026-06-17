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
`themes/` directories.

**Included (sessions, with path remapping):**
`projects/<encoded-path>/*.jsonl` transcripts and per-repo `memory/`.

**Never synced:**
`.credentials.json` (hard-blocked), plus machine-local/cache state
(`shell-snapshots/`, `session-env/`, `backups/`, `statsig/`, `launcher-settings.json`,
`policy-limits.json`, `remote-settings.json`). `~/.claude.json` is excluded by
default because it embeds OAuth tokens and per-project trust decisions.

Config files are scanned for secret-shaped strings (API keys, tokens) before
inclusion; a match aborts the snapshot unless you pass `--allow-secrets`.

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
| `ccsync pull [--remote URL] [--archive FILE]` | Fetch a snapshot into staging. |
| `ccsync restore [--dry-run] [--no-remap] [--overwrite]` | Apply the staged snapshot to `~/.claude`. |
| `ccsync export FILE` | One-shot snapshot → encrypted archive. |
| `ccsync import FILE` | Encrypted archive → staging. |
| `ccsync backup [--remote URL] [--archive FILE]` | `snapshot` + `push`. |
| `ccsync tui` | Launch an interactive terminal UI: review what would be backed up, browse local backups, and push/export. |
| `ccsync daemon` | Run the background backup loop in the foreground (used by the installed service). |
| `ccsync service install\|uninstall\|status` | Register/remove the background service (systemd user unit / launchd agent). |

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

Each tick builds a sanitized snapshot and publishes it to `destination`:

- **`git`** — pushes to the configured `remote`, exactly like `ccsync push`.
- **`archive`** — writes a timestamped `claude-backup-<ts>.tar.gz.age` into
  `backup_dir` (default `~/.config/ccsync/backups`). These appear in `ccsync tui`.

**Things to know:**

- **Secrets aren't inherited by the service.** A systemd user unit / launchd
  agent does not see your shell environment, so the `archive` destination needs
  `CCSYNC_PASSPHRASE` added to the unit, and the `git` destination needs SSH
  keys / a credential helper the agent can reach (an HTTPS remote with a stored
  credential is simplest). `install` prints a reminder; it never writes your
  secret into the generated unit.
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
  in the capture path regardless of configuration.
- **Archives are always encrypted** with [age](https://age-encryption.org/)
  using `CCSYNC_PASSPHRASE`; there is no plaintext mode.
- **`restore` is reversible** — it backs up the existing `~/.claude` to a
  timestamped `~/.claude.ccsync-backup-<ts>` directory before writing, supports
  `--dry-run`, and deep-merges `settings.json` by default (`--overwrite` to
  replace).

## Configuration

`~/.config/ccsync/config.toml` (created by `ccsync init`) controls the
`include`/`exclude` sets, `include_sessions`, the git `remote`, the `[remap]`
table, and the `[service]` table (see [Background service](#background-service)).
`CLAUDE_CONFIG_DIR` is honored when locating the source directory.
