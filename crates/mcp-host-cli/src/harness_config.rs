use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use jsonc_parser::parse_to_serde_value;
use serde_json::Value;

use crate::{cli::ClaudeScope, harness_paths::opencode_config_directory};

#[derive(Debug)]
pub struct ConfigVerification {
    pub path: PathBuf,
    pub exact: bool,
    pub reason: Option<String>,
}

pub fn verify_opencode(name: &str, command: &[String]) -> Result<ConfigVerification, String> {
    let directory = opencode_config_directory()?;
    verify_opencode_at(&directory, name, command)
}

pub fn verify_claude(
    name: &str,
    scope: ClaudeScope,
    command: &[String],
) -> Result<ConfigVerification, String> {
    let base =
        BaseDirs::new().ok_or_else(|| "could not determine the user home directory".to_owned())?;
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("could not determine the current directory: {error}"))?;
    let global = base.home_dir().join(".claude.json");
    match scope {
        ClaudeScope::User => verify_claude_at(&global, &[], name, command),
        ClaudeScope::Project => {
            verify_claude_at(&current_dir.join(".mcp.json"), &[], name, command)
        }
        ClaudeScope::Local => {
            let project = current_dir.to_string_lossy().into_owned();
            verify_claude_at(&global, &["projects", &project], name, command)
        }
    }
}

fn verify_opencode_at(
    directory: &Path,
    name: &str,
    command: &[String],
) -> Result<ConfigVerification, String> {
    let config_path = directory.join("config.json");
    let json_path = directory.join("opencode.json");
    let jsonc_path = directory.join("opencode.jsonc");
    if !config_path.exists() && !json_path.exists() && !jsonc_path.exists() {
        return Ok(ConfigVerification {
            path: json_path,
            exact: false,
            reason: Some("MCP entry is missing".to_owned()),
        });
    }

    let config = config_path
        .exists()
        .then(|| parse_config(&config_path))
        .transpose()?;
    let json = json_path
        .exists()
        .then(|| parse_config(&json_path))
        .transpose()?;
    let jsonc = jsonc_path
        .exists()
        .then(|| parse_config(&jsonc_path))
        .transpose()?;
    let layers = [
        (&config_path, config.as_ref()),
        (&json_path, json.as_ref()),
        (&jsonc_path, jsonc.as_ref()),
    ];
    let mut entry = None;
    let mut entry_path = None;
    for (path, root) in layers {
        let Some(layer) = root
            .and_then(|root| root.get("mcp"))
            .and_then(|mcp| mcp.get(name))
        else {
            continue;
        };
        entry = Some(
            entry
                .as_ref()
                .map(|entry| merge_json(entry, layer))
                .unwrap_or_else(|| layer.clone()),
        );
        entry_path = Some(path.to_owned());
    }
    let path = entry_path.unwrap_or_else(|| {
        if json_path.exists() {
            json_path
        } else if jsonc_path.exists() {
            jsonc_path
        } else {
            json_path
        }
    });
    let Some(entry) = entry else {
        return Ok(ConfigVerification {
            path,
            exact: false,
            reason: Some("MCP entry is missing".to_owned()),
        });
    };
    let actual_command = entry.get("command").and_then(string_array);
    let exact = entry.get("type").and_then(Value::as_str) == Some("local")
        && actual_command.as_deref() == Some(command)
        && entry.get("enabled").and_then(Value::as_bool) != Some(false);
    Ok(ConfigVerification {
        path,
        exact,
        reason: (!exact).then(|| "type, command, or enabled state differs".to_owned()),
    })
}

fn verify_claude_at(
    path: &Path,
    prefix: &[&str],
    name: &str,
    command: &[String],
) -> Result<ConfigVerification, String> {
    if !path.exists() {
        return Ok(ConfigVerification {
            path: path.to_owned(),
            exact: false,
            reason: Some("MCP entry is missing".to_owned()),
        });
    }
    let root = parse_config(path)?;
    let mut parent = &root;
    for segment in prefix {
        let Some(next) = parent.get(*segment) else {
            return Ok(ConfigVerification {
                path: path.to_owned(),
                exact: false,
                reason: Some("MCP scope is missing".to_owned()),
            });
        };
        parent = next;
    }
    let entry = parent
        .get("mcpServers")
        .and_then(|servers| servers.get(name));
    let Some(entry) = entry else {
        return Ok(ConfigVerification {
            path: path.to_owned(),
            exact: false,
            reason: Some("MCP entry is missing".to_owned()),
        });
    };
    let executable = command.first().map(String::as_str);
    let arguments = command.get(1..).unwrap_or_default();
    let actual_arguments = entry.get("args").and_then(string_array);
    let exact = entry
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|value| value == "stdio")
        && entry.get("command").and_then(Value::as_str) == executable
        && actual_arguments.as_deref() == Some(arguments);
    Ok(ConfigVerification {
        path: path.to_owned(),
        exact,
        reason: (!exact).then(|| "transport, command, or arguments differ".to_owned()),
    })
}

fn parse_config(path: &Path) -> Result<Value, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    parse_to_serde_value(&content, &Default::default())
        .map_err(|error| format!("could not parse {}: {error}", path.display()))
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn merge_json(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            let mut merged = base.clone();
            for (key, value) in overlay {
                let value = merged
                    .get(key)
                    .map(|base| merge_json(base, value))
                    .unwrap_or_else(|| value.clone());
                merged.insert(key.clone(), value);
            }
            Value::Object(merged)
        }
        (_, overlay) => overlay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{verify_claude_at, verify_opencode_at};

    #[test]
    fn verifies_commented_opencode_jsonc_semantically() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("opencode.jsonc"),
            r#"{
              // Existing user comment stays valid.
              "mcp": {
                "dynamic-mcp": {
                  "type": "local",
                  "command": ["/bin/mcp-host", "mcp"],
                  "enabled": true,
                },
              },
            }"#,
        )
        .expect("OpenCode config");

        let exact = verify_opencode_at(
            directory.path(),
            "dynamic-mcp",
            &["/bin/mcp-host".to_owned(), "mcp".to_owned()],
        )
        .expect("verification");
        assert!(exact.exact);

        let mismatch = verify_opencode_at(
            directory.path(),
            "dynamic-mcp",
            &["/other/mcp-host".to_owned(), "mcp".to_owned()],
        )
        .expect("verification");
        assert!(!mismatch.exact);
        assert_eq!(
            mismatch.reason.as_deref(),
            Some("type, command, or enabled state differs")
        );
    }

    #[test]
    fn opencode_jsonc_entry_overrides_the_json_entry() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("opencode.json"),
            r#"{"mcp":{"dynamic-mcp":{"type":"local","command":["/bin/mcp-host","mcp"]}}}"#,
        )
        .expect("OpenCode JSON config");
        fs::write(
            directory.path().join("opencode.jsonc"),
            r#"{"mcp":{"dynamic-mcp":{"type":"local","command":["/stale/mcp-host","mcp"]}}}"#,
        )
        .expect("OpenCode JSONC config");

        let verification = verify_opencode_at(
            directory.path(),
            "dynamic-mcp",
            &["/bin/mcp-host".to_owned(), "mcp".to_owned()],
        )
        .expect("verification");
        assert!(!verification.exact);
        assert_eq!(verification.path, directory.path().join("opencode.jsonc"));
    }

    #[test]
    fn opencode_json_and_jsonc_entries_are_deep_merged() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("opencode.json"),
            r#"{"mcp":{"dynamic-mcp":{"type":"local","command":["/bin/mcp-host","mcp"]}}}"#,
        )
        .expect("OpenCode JSON config");
        fs::write(
            directory.path().join("opencode.jsonc"),
            r#"{"mcp":{"dynamic-mcp":{"enabled":true}}}"#,
        )
        .expect("OpenCode JSONC config");

        let verification = verify_opencode_at(
            directory.path(),
            "dynamic-mcp",
            &["/bin/mcp-host".to_owned(), "mcp".to_owned()],
        )
        .expect("verification");
        assert!(verification.exact);
        assert_eq!(verification.path, directory.path().join("opencode.jsonc"));
    }

    #[test]
    fn opencode_config_json_is_the_lowest_priority_merge_layer() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("config.json"),
            r#"{"mcp":{"dynamic-mcp":{"enabled":false}}}"#,
        )
        .expect("OpenCode base config");
        fs::write(
            directory.path().join("opencode.json"),
            r#"{"mcp":{"dynamic-mcp":{"type":"local","command":["/bin/mcp-host","mcp"]}}}"#,
        )
        .expect("OpenCode JSON config");

        let verification = verify_opencode_at(
            directory.path(),
            "dynamic-mcp",
            &["/bin/mcp-host".to_owned(), "mcp".to_owned()],
        )
        .expect("verification");
        assert!(!verification.exact);
        assert_eq!(verification.path, directory.path().join("opencode.json"));
    }

    #[test]
    fn verifies_claude_user_config_semantically() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join(".claude.json");
        fs::write(
            &path,
            r#"{
              "mcpServers": {
                "dynamic-mcp": {
                  "type": "stdio",
                  "command": "/bin/mcp-host",
                  "args": ["--runtime-dir", "/runtime path", "mcp"],
                  "env": {}
                }
              }
            }"#,
        )
        .expect("Claude config");

        let verification = verify_claude_at(
            &path,
            &[],
            "dynamic-mcp",
            &[
                "/bin/mcp-host".to_owned(),
                "--runtime-dir".to_owned(),
                "/runtime path".to_owned(),
                "mcp".to_owned(),
            ],
        )
        .expect("verification");
        assert!(verification.exact);
    }
}
