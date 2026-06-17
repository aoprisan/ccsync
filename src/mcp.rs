//! Bundling locally-configured MCP servers.
//!
//! Claude Code stores MCP server definitions in `~/.claude.json`, not under
//! `~/.claude/`:
//!   * **user scope** — the top-level `mcpServers` object (available in every
//!     project on this machine), and
//!   * **local scope** — per-project `projects.<cwd>.mcpServers` objects (private
//!     to a single working directory).
//!
//! `~/.claude.json` as a whole is *not* synced: it embeds OAuth tokens, command
//! history, and per-project trust decisions that are sensitive or
//! machine-specific. This module extracts just the portable `mcpServers`
//! definitions into a standalone `mcp-servers.json` that rides along inside the
//! snapshot's `data/` directory, and merges them back into the local
//! `~/.claude.json` on restore — remapping project paths the same way session
//! transcripts are remapped, and leaving every other key of `~/.claude.json`
//! untouched.
//!
//! Project `.mcp.json` files (the *project* scope, checked into a repo) are not
//! handled here: they already travel with their repository.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::remap::{self, Mapping};

/// File name, relative to the snapshot's `data/` root, holding the extracted
/// MCP server definitions. Restore special-cases this name instead of copying
/// it into `~/.claude/`.
pub const MCP_FILE: &str = "mcp-servers.json";

/// Extract portable MCP server definitions from a `~/.claude.json` file.
///
/// Returns `Ok(None)` when the file is absent or defines no MCP servers at all.
/// Otherwise returns a document of the shape:
/// ```json
/// {
///   "mcpServers": { "<name>": { ... } },
///   "projects": { "/abs/path": { "mcpServers": { "<name>": { ... } } } }
/// }
/// ```
/// Only the `mcpServers` maps are carried over; OAuth tokens, history, and trust
/// decisions in `~/.claude.json` are dropped.
pub fn extract(claude_json: &Path) -> Result<Option<Value>> {
    if !claude_json.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(claude_json)
        .with_context(|| format!("reading {}", claude_json.display()))?;
    let root: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", claude_json.display()))?;

    let mut out = Map::new();

    // User scope: top-level `mcpServers`.
    if let Some(servers) = non_empty_object(root.get("mcpServers")) {
        out.insert("mcpServers".into(), Value::Object(servers.clone()));
    }

    // Local scope: per-project `mcpServers`, keyed by the project's absolute
    // working directory so restore can remap it.
    let mut projects = Map::new();
    if let Some(Value::Object(by_path)) = root.get("projects") {
        for (path, entry) in by_path {
            if let Some(servers) = non_empty_object(entry.get("mcpServers")) {
                let mut wrapper = Map::new();
                wrapper.insert("mcpServers".into(), Value::Object(servers.clone()));
                projects.insert(path.clone(), Value::Object(wrapper));
            }
        }
    }
    if !projects.is_empty() {
        out.insert("projects".into(), Value::Object(projects));
    }

    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(Value::Object(out)))
}

/// Total number of server definitions in an extracted document (user scope plus
/// every project scope), for human-readable reporting.
pub fn server_count(doc: &Value) -> usize {
    let mut n = object_len(doc.get("mcpServers"));
    if let Some(Value::Object(projects)) = doc.get("projects") {
        for entry in projects.values() {
            n += object_len(entry.get("mcpServers"));
        }
    }
    n
}

/// Merge an extracted MCP document into the local `~/.claude.json`, remapping
/// project paths via `mappings`. Every other key of `~/.claude.json` is
/// preserved. Returns the number of server definitions merged in.
///
/// With `overwrite` true, an incoming server definition replaces any existing
/// one of the same name; otherwise the two are deep-merged (incoming wins on
/// conflicting leaves), so servers already present are kept.
pub fn merge_into(
    claude_json: &Path,
    doc: &Value,
    mappings: &[Mapping],
    overwrite: bool,
) -> Result<usize> {
    let mut root: Value = if claude_json.exists() {
        let text = std::fs::read_to_string(claude_json)
            .with_context(|| format!("reading {}", claude_json.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", claude_json.display()))?
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("root is an object");
    let mut merged = 0usize;

    // User scope.
    if let Some(Value::Object(servers)) = doc.get("mcpServers") {
        let target = ensure_object(root_obj, "mcpServers");
        for (name, def) in servers {
            insert_server(target, name, def, overwrite);
            merged += 1;
        }
    }

    // Local (per-project) scope, with path remapping.
    if let Some(Value::Object(projects)) = doc.get("projects") {
        let by_path = ensure_object(root_obj, "projects");
        for (path, entry) in projects {
            let Some(Value::Object(servers)) = entry.get("mcpServers") else {
                continue;
            };
            let mapped = remap::remap_path(path, mappings).unwrap_or_else(|| path.clone());
            let project = ensure_object(by_path, &mapped);
            let target = ensure_object(project, "mcpServers");
            for (name, def) in servers {
                insert_server(target, name, def, overwrite);
                merged += 1;
            }
        }
    }

    let serialized =
        serde_json::to_string_pretty(&root).context("serializing merged ~/.claude.json")?;
    std::fs::write(claude_json, serialized)
        .with_context(|| format!("writing {}", claude_json.display()))?;
    Ok(merged)
}

/// Insert one server definition, either replacing or deep-merging.
fn insert_server(target: &mut Map<String, Value>, name: &str, def: &Value, overwrite: bool) {
    if overwrite {
        target.insert(name.to_string(), def.clone());
    } else {
        let slot = target.entry(name.to_string()).or_insert(Value::Null);
        merge_value(slot, def);
    }
}

/// Deep-merge `incoming` into `base`; objects merge key-by-key, everything else
/// is replaced by `incoming`. Mirrors `restore::merge_value`.
fn merge_value(base: &mut Value, incoming: &Value) {
    match (base, incoming) {
        (Value::Object(b), Value::Object(i)) => {
            for (k, v) in i {
                merge_value(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, i) => *b = i.clone(),
    }
}

/// Borrow (creating if needed) a nested object under `key`.
fn ensure_object<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    // Replace a non-object value (corrupt config) with a fresh object.
    if !parent.get(key).map(Value::is_object).unwrap_or(false) {
        parent.insert(key.to_string(), Value::Object(Map::new()));
    }
    parent
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("ensured object")
}

/// Return the object behind `value` only if it is a non-empty object.
fn non_empty_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    match value {
        Some(Value::Object(m)) if !m.is_empty() => Some(m),
        _ => None,
    }
}

fn object_len(value: Option<&Value>) -> usize {
    match value {
        Some(Value::Object(m)) => m.len(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn extract_pulls_user_and_project_scopes_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cj = tmp.path().join(".claude.json");
        write(
            &cj,
            &json!({
                "oauthAccount": { "accessToken": "secret-should-be-dropped" },
                "mcpServers": {
                    "fetch": { "command": "uvx", "args": ["mcp-server-fetch"] }
                },
                "projects": {
                    "/home/alice/proj": {
                        "allowedTools": ["Bash"],
                        "mcpServers": {
                            "db": { "command": "pg-mcp", "args": ["--ro"] }
                        }
                    },
                    "/home/alice/other": { "allowedTools": [] }
                }
            }),
        );

        let doc = extract(&cj).unwrap().expect("some servers");
        assert_eq!(server_count(&doc), 2);
        // User scope captured, OAuth token not.
        assert_eq!(doc["mcpServers"]["fetch"]["command"], "uvx");
        assert!(doc.get("oauthAccount").is_none());
        // Project with servers captured; project without is omitted.
        assert_eq!(
            doc["projects"]["/home/alice/proj"]["mcpServers"]["db"]["command"],
            "pg-mcp"
        );
        assert!(doc["projects"].get("/home/alice/other").is_none());
        // Non-mcp project keys are not carried over.
        assert!(doc["projects"]["/home/alice/proj"]
            .get("allowedTools")
            .is_none());
    }

    #[test]
    fn extract_returns_none_when_no_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let cj = tmp.path().join(".claude.json");
        write(
            &cj,
            &json!({ "mcpServers": {}, "projects": { "/x": { "allowedTools": [] } } }),
        );
        assert!(extract(&cj).unwrap().is_none());
        // Missing file is also None, not an error.
        assert!(extract(&tmp.path().join("absent.json")).unwrap().is_none());
    }

    #[test]
    fn merge_preserves_other_keys_and_remaps_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let cj = tmp.path().join(".claude.json");
        // Pre-existing local config with an OAuth token and one user server.
        write(
            &cj,
            &json!({
                "oauthAccount": { "accessToken": "keep-me" },
                "mcpServers": { "existing": { "command": "keep" } }
            }),
        );

        let doc = json!({
            "mcpServers": { "fetch": { "command": "uvx" } },
            "projects": {
                "/home/alice/proj": { "mcpServers": { "db": { "command": "pg-mcp" } } }
            }
        });
        let mappings = vec![Mapping {
            from: "/home/alice".into(),
            to: "/home/bob".into(),
        }];
        let merged = merge_into(&cj, &doc, &mappings, false).unwrap();
        assert_eq!(merged, 2);

        let root: Value = serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        // Untouched keys preserved.
        assert_eq!(root["oauthAccount"]["accessToken"], "keep-me");
        assert_eq!(root["mcpServers"]["existing"]["command"], "keep");
        // Incoming user server added.
        assert_eq!(root["mcpServers"]["fetch"]["command"], "uvx");
        // Project path remapped /home/alice -> /home/bob.
        assert_eq!(
            root["projects"]["/home/bob/proj"]["mcpServers"]["db"]["command"],
            "pg-mcp"
        );
        assert!(root["projects"].get("/home/alice/proj").is_none());
    }

    #[test]
    fn merge_into_absent_file_creates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cj = tmp.path().join(".claude.json");
        let doc = json!({ "mcpServers": { "fetch": { "command": "uvx" } } });
        merge_into(&cj, &doc, &[], false).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert_eq!(root["mcpServers"]["fetch"]["command"], "uvx");
    }

    #[test]
    fn merge_mode_keeps_existing_overwrite_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let cj = tmp.path().join(".claude.json");
        write(
            &cj,
            &json!({ "mcpServers": { "fetch": { "command": "old", "extra": 1 } } }),
        );
        let doc = json!({ "mcpServers": { "fetch": { "command": "new" } } });

        // Merge: deep-merge, incoming leaf wins but unrelated keys survive.
        merge_into(&cj, &doc, &[], false).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert_eq!(root["mcpServers"]["fetch"]["command"], "new");
        assert_eq!(root["mcpServers"]["fetch"]["extra"], 1);

        // Overwrite: the whole server definition is replaced.
        write(
            &cj,
            &json!({ "mcpServers": { "fetch": { "command": "old", "extra": 1 } } }),
        );
        merge_into(&cj, &doc, &[], true).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert_eq!(root["mcpServers"]["fetch"]["command"], "new");
        assert!(root["mcpServers"]["fetch"].get("extra").is_none());
    }
}
