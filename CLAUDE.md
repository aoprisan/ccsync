# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`ccsync` is a single-binary Rust CLI that syncs and backs up Claude Code state
(`~/.claude` + the `mcpServers` slice of `~/.claude.json`) across machines. The
hard problems it solves — and the invariants you must not break — are:

1. **Credentials never leave the machine.** `.credentials.json` is a *hard
   block* in the capture path (`redact::is_credential_file`), independent of
   config. `~/.claude.json` is never synced wholesale (OAuth tokens, trust
   decisions); only its `mcpServers` are extracted.
2. **Best-effort secret scanning.** Text configs are regex-scanned before
   inclusion; a match aborts the snapshot unless `--allow-secrets`.
3. **Path remapping.** Session dirs under `projects/` are named after the
   absolute cwd (`/` → `-`), and transcripts embed that path. Restoring on a
   machine with a different home/checkout requires rewriting those paths or the
   session picker won't find them.

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
  `status` is just `snapshot --dry-run`, and `backup` is `snapshot` + `push`.
- **`config.rs`** — `Config` (TOML at `~/.config/ccsync/config.toml`). The
  `Default` impl *is* the include/exclude policy (the portable-vs-sensitive
  split). `#[serde(default)]` is load-bearing: configs written before a field
  existed must still load, and several tests assert this — preserve it when
  adding fields.
- **`paths.rs`** — single source of truth for *all* filesystem locations and for
  the `encode_path`/`decode_path` dash-encoding. Honors `CLAUDE_CONFIG_DIR`.
  ccsync's own files live under `dirs::config_dir()/ccsync/` — that's
  `~/Library/Application Support/ccsync/` on **macOS** and `~/.config/ccsync/`
  on Linux. The `~/.config/ccsync/...` paths written throughout this file and
  the source doc-comments are the Linux form; they are *not* literal on macOS.
  Never hand-roll the encoding or a path elsewhere; call into here.
- **`snapshot.rs`** — walks `~/.claude`, applies include/exclude + the credential
  hard-block + secret scan, copies survivors into `<staging>/data/`, and writes
  `manifest.json`. Staging is wiped and rebuilt each run.
- **`manifest.rs`** — `manifest.json` carried in every snapshot. Records
  `source_home` and the decoded `project_roots`; this is what makes remap
  possible on the target machine. Versioned (`manifest_version`).
- **`redact.rs`** — the credential blocklist check and the secret-pattern regexes.
- **`remap.rs`** — rewrites absolute-path prefixes inside staged `data/` *in
  place* before restore copies it out: rewrites `*.jsonl` contents, then renames
  encoded `projects/<encoded>` dirs. Mappings are longest-prefix-first. Reused by
  `mcp.rs` to remap per-project MCP keys.
- **`restore.rs`** — backs up existing `~/.claude` to a timestamped sibling
  (always reversible), runs remap, copies files (deep-merging `*.json` unless
  `--overwrite`), then merges bundled MCP servers into `~/.claude.json`.
- **`mcp.rs`** — extracts user-scope + per-project `mcpServers` from
  `~/.claude.json` into `mcp-servers.json` inside the snapshot, and merges them
  back on restore. This file is special-cased in `restore.rs` (NOT copied into
  `~/.claude/`). Project `.mcp.json` files are deliberately untouched.
- **`git.rs`** — shells out to the system `git` binary (no libgit2); caches a
  clone at `~/.config/ccsync/repo`. **`archive.rs`** — `tar.gz` + `age`
  encryption, passphrase from `CCSYNC_PASSPHRASE` (no plaintext mode).
- **`service.rs`** — `daemon` (foreground loop) + `service install/uninstall/
  start/stop/status`. `install` writes a systemd user unit / launchd agent;
  `start` runs detached nohup-style with a pidfile. Pure orchestration over
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
