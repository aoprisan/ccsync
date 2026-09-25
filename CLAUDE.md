# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`ccsync` is a single-binary Rust CLI that syncs and backs up Claude Code state
(`~/.claude` + the `mcpServers` slice of `~/.claude.json`) across machines. The
hard problems it solves — and the invariants you must not break — are:

1. **Credentials never leave the machine.** `.credentials.json` is a *hard
   block* in the capture path (`redact::is_credential_file`), independent of
   config, and also enforced when copying into profile stores. `~/.claude.json`
   is never synced wholesale (OAuth tokens, trust decisions); only its
   `mcpServers` are extracted.
2. **Best-effort secret scanning.** Text configs are regex-scanned before
   inclusion (via `from_utf8_lossy`, so invalid bytes can't dodge the scan); a
   match aborts the snapshot unless `--allow-secrets`. Transcripts (`*.jsonl`)
   get the `transcript_secrets` policy instead: default `redact` rewrites
   matches to `[REDACTED:ccsync]` *in the staged copy only* — never mutate the
   user's source files.
3. **Path remapping.** Session dirs under `projects/` are named after the
   absolute cwd (`/` → `-`), and transcripts embed that path. Restoring on a
   machine with a different home/checkout requires rewriting those paths or the
   session picker won't find them. The dash-encoding is lossy (dashes in real
   dir names), so `manifest.project_roots` — resolved at snapshot time from
   `~/.claude.json`'s `projects` keys and the live filesystem — is the
   authoritative decode table; never re-derive paths with `decode_path` when a
   manifest is available. Content rewrites must stay boundary-aware
   (`remap::replace_bounded`) so `/Users/alice2` survives an
   `/Users/alice` remap.
4. **Staging is immutable input; restores verify integrity.** Restore remaps a
   temp apply-set copy (never staging itself — snapshots are reusable) and
   first checks every staged file against the manifest's sha256 (both
   directions) plus path safety. Keep any new restore/extract path behind
   these checks.
5. **Hooks are code.** An incoming `settings.json` can install hook commands
   — and so can `statusLine.command`, the `*Helper`/`aws*` keys and `env`
   (`restore::hook_commands_in` collects all of them), plus new stdio MCP
   servers (`mcp::server_commands`); restore, profile switch/rollback and
   layer apply must surface new ones and fail closed when non-interactive
   (`confirm_hooks`).
6. **Never write through a symlink, never lose one.** Restore skips
   destinations under a symlink (`symlink_on_path`); backups and profile
   copies recreate links rather than following or dropping them.

## Commands

```sh
cargo build                     # debug build
cargo build --release           # release binary at target/release/ccsync
cargo test                      # run all unit tests (tests live inline per module)
cargo test snapshot::tests      # run one module's tests
cargo test hard_blocks_credentials   # run a single test by name
cargo clippy --all-targets      # lint
cargo fmt                       # format
```

**Test gotcha:** several tests mutate the process environment — `restore.rs` and
anything calling `paths::home_dir` set `HOME`, and the `with_config_dir` helpers
in `backups.rs`/`service.rs` set both `HOME` and `XDG_CONFIG_HOME` (both are
needed because `dirs::config_dir()` reads `XDG_CONFIG_HOME` on Linux but
`$HOME/Library/Application Support` on macOS). Because env vars are process-global
these race under the default parallel runner. Treat
`cargo test -- --test-threads=1` as the canonical way to run the suite; if a
test flakes under plain `cargo test`, re-run single-threaded before assuming a
real failure.

## Architecture

The flow is a pipeline; each stage is one module, and `main.rs` is thin glue
that maps CLI subcommands to stage calls.

```
snapshot ──> (git push | archive create) ──> [transport] ──> (git pull | archive extract) ──> restore
```

- **`cli.rs`** — clap subcommand definitions. `main.rs` dispatches them; note
  `status` is just `snapshot --dry-run`, `backup` is `snapshot` + `push`, and
  `rollback` is `pull --at` + `restore`. `profile`, `diff`, and `layer` have
  their own modules below.
- **`config.rs`** — `Config` (TOML at `~/.config/ccsync/config.toml`). The
  `Default` impl *is* the include/exclude policy (the portable-vs-sensitive
  split). `#[serde(default)]` is load-bearing: configs written before a field
  existed must still load, and several tests assert this — preserve it when
  adding fields. `[machines.<id>]` overrides fold in via
  `with_machine_overrides()` at load; anything that *saves* config must reload
  from disk first or the folded overrides leak into the base sets (see
  `cmd_push`). `effective_machine_id()` names this machine's repo subtree.
- **`paths.rs`** — single source of truth for *all* filesystem locations and for
  the `encode_path`/`decode_path` dash-encoding. Honors `CLAUDE_CONFIG_DIR`.
  ccsync's own files live under `dirs::config_dir()/ccsync/` — that's
  `~/Library/Application Support/ccsync/` on **macOS** and `~/.config/ccsync/`
  on Linux. The `~/.config/ccsync/...` paths written throughout this file and
  the source doc-comments are the Linux form; they are *not* literal on macOS.
  Never hand-roll the encoding or a path elsewhere; call into here.
- **`snapshot.rs`** — walks `~/.claude`, applies include/exclude + the credential
  hard-block + secret scan/redaction, copies survivors into `<staging>/data/`,
  and writes `manifest.json`. Also bundles the profile store under the reserved
  `ccsync-profiles/` component (`profiles.sync`) and reports unclassified
  top-level entries (`unclassified_top_level`). Staging is wiped and rebuilt
  each run.
- **`manifest.rs`** — `manifest.json` carried in every snapshot. Records
  `source_home`, per-file sha256 (verified on restore), and the decoded
  `project_roots` (authoritative dash-decoding); this is what makes remap
  possible on the target machine. Versioned (`manifest_version`).
- **`redact.rs`** — the credential blocklist check, the secret-pattern regexes,
  and span-level redaction (`redact_secrets`) used for transcripts.
- **`remap.rs`** — rewrites absolute-path prefixes inside a `data/` tree:
  boundary-aware rewrites of `*.jsonl` contents (raw + dash-encoded forms),
  then renames encoded `projects/<encoded>` dirs using `manifest.project_roots`.
  Mappings are longest-prefix-first. Restore feeds it a temp apply-set copy,
  never staging itself. Reused by `mcp.rs` to remap per-project MCP keys.
- **`restore.rs`** — verifies manifest integrity, backs up existing `~/.claude`
  to a timestamped sibling (always reversible), remaps an apply-set copy,
  applies via the reusable `apply_tree` core (component filter powers
  `--only` and profile switching; deep-merges `*.json` unless `--overwrite`,
  scalar arrays union), gates incoming hooks, then merges bundled MCP servers
  into `~/.claude.json`. Routes the `ccsync-profiles/` component into the
  local profile store.
- **`profile.rs`** — named profiles over the shared base state: store layout
  under `<config>/ccsync/profiles/`, the capture-back → confirm-hooks → backup
  → journal → swap switch protocol with automatic rollback, `active.json`
  journal, per-component diff. Owned components are swapped wholesale, never
  merged (ghost-state bleed).
- **`diff.rs`** — `ccsync diff` (dry-run snapshot manifest vs staged manifest)
  and the `diff_trees` hash-walk used by `profile diff`.
- **`mcp.rs`** — extracts user-scope + per-project `mcpServers` from
  `~/.claude.json` into `mcp-servers.json` inside the snapshot, and merges them
  back on restore. This file is special-cased in `restore.rs` (NOT copied into
  `~/.claude/`). Project `.mcp.json` files are deliberately untouched.
- **`git.rs`** — shells out to the system `git` binary (no libgit2, transports
  restricted to ssh/https/http/file); caches a clone at
  `~/.config/ccsync/repo`. Repo layout: one `machines/<machine-id>/` subtree
  per machine (legacy root snapshots migrate to `machines/default` on first
  push); pushes align the cache to the remote tip with `reset --hard` (history
  is disposable, last writer wins per subtree). Also serves `history`
  (`log`), `machines`, `pull --at` (reset → copy → re-align), and
  `remote_manifest` for `diff --remote`. Read paths fetch strictly (a failed
  fetch is an error, never a stale cache); a remote URL change re-clones.
  `pull`/`import` drop a `.ccsync-pulled` marker in staging that `push`
  refuses; the next real snapshot clears it. **`archive.rs`** — `tar.gz` + `age`
  encryption, passphrase from `CCSYNC_PASSPHRASE` (no plaintext mode);
  extraction is per-entry and refuses unsafe paths/links.
- **`layer.rs`** — read-only shared layers (`[[layers]]` config): `pull`
  clones/updates under `<config>/ccsync/layers/`, `apply` copies only the
  declared components into `~/.claude` after credential/secret/hook vetting —
  a layer repo is untrusted input.
- **`lock.rs`** — `flock` helper. Every public `git.rs` entry point holds
  the repo-cache lock once (they never call each other — keep it that way or
  it self-deadlocks); the pidfile claim uses it too.
- **`service.rs`** — `daemon` (foreground loop; stages into its own
  `daemon-staging` dir so a tick never clobbers a pulled snapshot) + `service install/uninstall/
  start/stop/status`. `install` writes a systemd user unit / launchd agent
  (both source `<config>/ccsync/service.env` for secrets); `start` runs
  detached with an atomically-claimed pidfile, and stop/status verify the PID
  is really a ccsync process before trusting it. Pure orchestration over
  `snapshot`/`git`/`archive` — keep transport logic out of here.
- **`tui.rs`** / **`theme.rs`** / **`backups.rs`** — ratatui interactive UI for
  reviewing/browsing/pushing backups.

## Conventions

- Errors: library code returns `anyhow::Result`; typed variants live in
  `error.rs` (`CcError`). `main.rs` prints `{e:#}` and exits 1.
- Every module keeps its tests inline (`#[cfg(test)]`) using `tempfile` and
  building real snapshot/restore round-trips against temp dirs — mirror that when
  adding behavior rather than mocking the filesystem.
- When adding a new include/exclude default or a security guard, add the
  corresponding assertion test; the existing suite treats the credential block
  and secret scan as contracts.
