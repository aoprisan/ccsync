//! Read-only shared layers: git repos (e.g. a team's `skills/` + `commands/`
//! collection) that can be pulled and applied into `~/.claude` beneath your
//! own state.
//!
//! A layer is declared in the config as a `[[layers]]` entry naming the repo
//! and the top-level components it provides. `ccsync layer pull` clones or
//! updates the checkout under `<config>/ccsync/layers/<name>/`;
//! `ccsync layer apply` copies the declared components into `~/.claude`.
//!
//! Layers are *untrusted input* and get snapshot-grade treatment on apply:
//! credential files are hard-blocked, text files are secret-scanned, and a
//! layer settings.json that would install new hook commands requires the same
//! confirmation as a restore. Only the declared components are ever copied —
//! a hostile repo cannot smuggle files outside them (nor its `.git`).
//!
//! ccsync never pushes to a layer; bidirectional team sync is deliberately
//! out of scope. Note that applied layer files land in the live `~/.claude`,
//! so the active profile's capture-back will absorb them like any other live
//! state.

use std::path::Path;

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

use crate::config::{Config, LayerConfig};
use crate::error::CcError;
use crate::git;
use crate::redact;
use crate::restore::{self, ApplyOptions, MergeMode};
use crate::snapshot::SCANNED_EXTS;

/// Look up a configured layer by name.
pub fn find<'a>(config: &'a Config, name: &str) -> Result<&'a LayerConfig> {
    config
        .layers
        .iter()
        .find(|l| l.name == name)
        .ok_or_else(|| {
            let known = config
                .layers
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("no [[layers]] entry named {name:?} in the config (have: {known})")
        })
}

/// Layer names become directory names under the layers root.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("invalid layer name {name:?}: use letters, digits, '-', '_'");
    }
    Ok(())
}

/// Clone or update the checkout for `layer` under `layers_root`.
pub fn pull(layer: &LayerConfig, layers_root: &Path) -> Result<()> {
    validate_name(&layer.name)?;
    if layer.remote.is_empty() {
        bail!("layer {:?} has no remote configured", layer.name);
    }
    let dest = layers_root.join(&layer.name);
    git::clone_or_update(&layer.remote, &dest)
        .with_context(|| format!("pulling layer {:?}", layer.name))
}

/// Apply the layer's declared components from its checkout into `claude_dir`.
/// Returns the relative paths applied.
pub fn apply(
    layer: &LayerConfig,
    layers_root: &Path,
    claude_dir: &Path,
    confirm_hooks: bool,
    allow_secrets: bool,
) -> Result<Vec<String>> {
    validate_name(&layer.name)?;
    let checkout = layers_root.join(&layer.name);
    if !checkout.is_dir() {
        bail!(
            "layer {:?} has no checkout yet; run `ccsync layer pull` first",
            layer.name
        );
    }
    if layer.components.is_empty() {
        bail!(
            "layer {:?} declares no components; add e.g. components = [\"skills\"]",
            layer.name
        );
    }

    // Snapshot-grade vetting of everything the apply would copy, before any
    // write: credential hard-block plus the secret scan for text files.
    for comp in &layer.components {
        let root = checkout.join(comp);
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if redact::is_credential_file(&name) {
                return Err(CcError::CredentialBlocked(format!(
                    "{} (in layer {:?})",
                    entry.path().display(),
                    layer.name
                ))
                .into());
            }
            let scanned = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| SCANNED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if scanned && !allow_secrets {
                let bytes = std::fs::read(entry.path())?;
                if let Some(hint) = redact::scan_for_secrets(&String::from_utf8_lossy(&bytes)) {
                    return Err(CcError::SecretDetected {
                        file: format!("{} (in layer {:?})", entry.path().display(), layer.name),
                        hint,
                    }
                    .into());
                }
            }
        }
    }

    // Hooks are code: a shared repo must not silently install them.
    if confirm_hooks
        && layer
            .components
            .iter()
            .any(|c| c.trim_end_matches('/') == "settings.json")
    {
        let incoming = restore::hook_commands_in(&checkout.join("settings.json"))?;
        let existing = restore::hook_commands_in(&claude_dir.join("settings.json"))?;
        let new_hooks: std::collections::BTreeSet<String> =
            incoming.difference(&existing).cloned().collect();
        if !new_hooks.is_empty() {
            restore::confirm_hook_install(&new_hooks)?;
        }
    }

    // Additive copy of the declared components only — the filter also keeps
    // the checkout's `.git` (and anything else undeclared) out of ~/.claude.
    restore::apply_tree(
        &checkout,
        claude_dir,
        &ApplyOptions {
            dry_run: false,
            merge: MergeMode::Merge,
            components: Some(&layer.components),
            profiles_root: None,
            // Layers are `~/.claude` component sources; a layer repo never
            // carries a Copilot tree.
            copilot_root: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn layer_cfg(name: &str, remote: &str, components: &[&str]) -> LayerConfig {
        LayerConfig {
            name: name.into(),
            remote: remote.into(),
            components: components.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Seed a bare repo with team content via a temporary working clone.
    fn seed_team_repo(dir: &Path, files: &[(&str, &str)]) -> String {
        let bare = dir.join("team.git");
        std::fs::create_dir_all(&bare).unwrap();
        let bare_url = bare.to_string_lossy().to_string();
        let work = dir.join("team-work");
        std::process::Command::new("git")
            .args(["init", "--bare", &bare_url])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["clone", &bare_url, &work.to_string_lossy()])
            .output()
            .unwrap();
        for (rel, content) in files {
            write(&work.join(rel), content);
        }
        for args in [
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "seed",
            ],
            vec!["push", "origin", "HEAD"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&work)
                .output()
                .unwrap();
        }
        bare_url
    }

    #[test]
    fn pull_and_apply_copies_declared_components_only() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = seed_team_repo(
            tmp.path(),
            &[
                ("skills/review/SKILL.md", "# team review"),
                ("commands/team.md", "# team cmd"),
                ("secrets-elsewhere/creds.txt", "not-declared"),
            ],
        );
        let layers_root = tmp.path().join("layers");
        let claude = tmp.path().join("claude");
        std::fs::create_dir_all(&claude).unwrap();

        let layer = layer_cfg("team", &remote, &["skills", "commands"]);
        pull(&layer, &layers_root).unwrap();
        assert!(layers_root.join("team/skills/review/SKILL.md").exists());

        let applied = apply(&layer, &layers_root, &claude, true, false).unwrap();
        assert_eq!(applied.len(), 2);
        assert!(claude.join("skills/review/SKILL.md").exists());
        assert!(claude.join("commands/team.md").exists());
        // Undeclared content and the checkout's .git never land in ~/.claude.
        assert!(!claude.join("secrets-elsewhere").exists());
        assert!(!claude.join(".git").exists());

        // Re-pull is an update, not an error.
        pull(&layer, &layers_root).unwrap();
    }

    #[test]
    fn apply_vets_layer_content() {
        let tmp = tempfile::tempdir().unwrap();
        let layers_root = tmp.path().join("layers");
        let claude = tmp.path().join("claude");
        std::fs::create_dir_all(&claude).unwrap();

        // Un-pulled layer is a clear error.
        let layer = layer_cfg("team", "unused", &["skills"]);
        let err = apply(&layer, &layers_root, &claude, true, false).unwrap_err();
        assert!(err.to_string().contains("layer pull"), "got: {err:#}");

        // A credential file in a declared component is hard-blocked.
        write(
            &layers_root.join("team/skills/.credentials.json"),
            r#"{"t":"x"}"#,
        );
        let err = apply(&layer, &layers_root, &claude, true, false).unwrap_err();
        assert!(err.to_string().contains("credential"), "got: {err:#}");
        std::fs::remove_file(layers_root.join("team/skills/.credentials.json")).unwrap();

        // A secret-shaped string aborts unless allowed.
        write(
            &layers_root.join("team/skills/setup.md"),
            "use sk-abcdefghijklmnopqrstuvwx",
        );
        let err = apply(&layer, &layers_root, &claude, true, false).unwrap_err();
        assert!(err.to_string().contains("secret"), "got: {err:#}");
        apply(&layer, &layers_root, &claude, true, true).unwrap();

        // A layer settings.json with new hooks fails closed without a tty.
        let layer = layer_cfg("team", "unused", &["settings.json"]);
        write(
            &layers_root.join("team/settings.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"evil"}]}]}}"#,
        );
        let err = apply(&layer, &layers_root, &claude, true, false).unwrap_err();
        assert!(err.to_string().contains("hooks"), "got: {err:#}");
        assert!(!claude.join("settings.json").exists());
    }
}
