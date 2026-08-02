use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use mcp_host_core::{RuntimeError, RuntimeErrorCode};
use serde_json::{Value, json};
use tokio::{process::Command, time::timeout};

use crate::{
    cli::{ClaudeScope, HarnessInstall, HarnessTarget},
    harness_config::{ConfigVerification, verify_claude, verify_opencode},
    harness_files::install_harness_files,
};

const HARNESS_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const ERROR_DETAIL_LIMIT: usize = 2_048;

pub async fn install_harnesses(
    install: HarnessInstall,
    runtime_dir: Option<&Path>,
) -> Result<Value, RuntimeError> {
    let runtime_dir = absolute_runtime_dir(runtime_dir)?;
    let bridge = bridge_arguments(&install, runtime_dir.as_deref())?;
    let command = bridge_command_json(&bridge);
    let mut installed = Vec::new();

    if matches!(install.target, HarnessTarget::OpenCode | HarnessTarget::All) {
        let before = verify_opencode(&install.name, &command).map_err(harness_error)?;
        let updated = !before.exact;
        if updated {
            let arguments = opencode_arguments(&install.name, &bridge);
            run_harness_command("opencode", &arguments, true).await?;
        }
        let verification = require_exact(
            verify_opencode(&install.name, &command).map_err(harness_error)?,
            "opencode",
        )?;
        let files = install_harness_files(HarnessTarget::OpenCode).map_err(harness_error)?;
        installed.push(json!({
            "harness": "opencode",
            "name": install.name,
            "scope": "global",
            "command": command,
            "configUpdated": updated,
            "verified": true,
            "configPath": verification.path,
            "skillPath": files.skill_path,
            "skillUpdated": files.skill_updated,
            "instructionPath": files.instruction_path,
            "instructionUpdated": files.instruction_updated,
        }));
    }

    if matches!(
        install.target,
        HarnessTarget::ClaudeCode | HarnessTarget::All
    ) {
        let before =
            verify_claude(&install.name, install.scope, &command).map_err(harness_error)?;
        let updated = !before.exact;
        if updated {
            let remove_arguments = claude_remove_arguments(&install.name, install.scope);
            // Claude Code rejects duplicate names. Removing only the requested scope makes
            // repair deterministic; a missing entry is expected on first install.
            run_harness_command("claude", &remove_arguments, false).await?;

            let arguments = claude_add_arguments(&install.name, install.scope, &bridge);
            run_harness_command("claude", &arguments, true).await?;
        }
        let verification = require_exact(
            verify_claude(&install.name, install.scope, &command).map_err(harness_error)?,
            "claude",
        )?;
        let files = install_harness_files(HarnessTarget::ClaudeCode).map_err(harness_error)?;
        installed.push(json!({
            "harness": "claude-code",
            "name": install.name,
            "scope": install.scope.as_str(),
            "command": command,
            "configUpdated": updated,
            "verified": true,
            "configPath": verification.path,
            "skillPath": files.skill_path,
            "skillUpdated": files.skill_updated,
            "instructionPath": files.instruction_path,
            "instructionUpdated": files.instruction_updated,
        }));
    }

    Ok(json!({
        "installed": installed,
        "daemonRequired": true,
    }))
}

fn absolute_runtime_dir(runtime_dir: Option<&Path>) -> Result<Option<PathBuf>, RuntimeError> {
    let Some(runtime_dir) = runtime_dir else {
        return Ok(None);
    };
    if runtime_dir.is_absolute() {
        return Ok(Some(runtime_dir.to_owned()));
    }

    std::env::current_dir()
        .map(|current_dir| Some(current_dir.join(runtime_dir)))
        .map_err(|error| {
            harness_error(format!(
                "could not resolve relative runtime directory {}: {error}",
                runtime_dir.display()
            ))
        })
}

fn canonical_executable(executable: &Path) -> Result<PathBuf, RuntimeError> {
    std::fs::canonicalize(executable).map_err(|error| {
        harness_error(format!(
            "could not resolve bridge executable path {}: {error}",
            executable.display()
        ))
    })
}

fn bridge_arguments(
    install: &HarnessInstall,
    runtime_dir: Option<&Path>,
) -> Result<Vec<OsString>, RuntimeError> {
    if let Some(executable) = &install.bridge_command {
        if runtime_dir.is_some() {
            return Err(harness_error(
                "--runtime-dir cannot be combined with --bridge-command; pass wrapper arguments with --bridge-arg",
            ));
        }
        let executable = canonical_executable(executable)?;
        let mut arguments = vec![executable.into_os_string()];
        arguments.extend(install.bridge_arg.iter().map(OsString::from));
        return Ok(arguments);
    }

    let current = std::env::current_exe().map_err(|error| {
        harness_error(format!(
            "could not locate the current mcp-host executable: {error}"
        ))
    })?;
    let executable = canonical_executable(&current)?;
    let mut arguments = vec![executable.into_os_string()];
    if let Some(runtime_dir) = runtime_dir {
        arguments.extend([
            OsString::from("--runtime-dir"),
            runtime_dir.as_os_str().to_owned(),
        ]);
    }
    arguments.push(OsString::from("mcp"));
    Ok(arguments)
}

fn opencode_arguments(name: &str, bridge: &[OsString]) -> Vec<OsString> {
    let mut arguments = os_strings(["mcp", "add", name, "--"]);
    arguments.extend_from_slice(bridge);
    arguments
}

fn claude_add_arguments(name: &str, scope: ClaudeScope, bridge: &[OsString]) -> Vec<OsString> {
    let mut arguments = os_strings([
        "mcp",
        "add",
        "--scope",
        scope.as_str(),
        "--transport",
        "stdio",
        name,
        "--",
    ]);
    arguments.extend_from_slice(bridge);
    arguments
}

fn claude_remove_arguments(name: &str, scope: ClaudeScope) -> Vec<OsString> {
    os_strings(["mcp", "remove", name, "--scope", scope.as_str()])
}

fn os_strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

async fn run_harness_command(
    program: &str,
    arguments: &[OsString],
    require_success: bool,
) -> Result<(), RuntimeError> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = timeout(HARNESS_COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            harness_error(format!(
                "`{program}` did not finish within {} seconds",
                HARNESS_COMMAND_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                harness_error(format!(
                    "`{program}` was not found on PATH; install it before configuring this harness"
                ))
            } else {
                harness_error(format!("could not run `{program}`: {error}"))
            }
        })?;

    if require_success && !output.status.success() {
        let detail = command_error_detail(&output.stderr, &output.stdout);
        return Err(harness_error(format!(
            "`{program}` exited with {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )));
    }

    Ok(())
}

fn command_error_detail(stderr: &[u8], stdout: &[u8]) -> String {
    let detail = if stderr.is_empty() { stdout } else { stderr };
    let detail = String::from_utf8_lossy(detail);
    let detail = detail.trim();
    let end = detail
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= ERROR_DETAIL_LIMIT)
        .last()
        .unwrap_or(0);

    if detail.len() <= ERROR_DETAIL_LIMIT {
        detail.to_owned()
    } else {
        format!("{}...", &detail[..end])
    }
}

fn bridge_command_json(bridge: &[OsString]) -> Vec<String> {
    bridge
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn require_exact(
    verification: ConfigVerification,
    harness: &str,
) -> Result<ConfigVerification, RuntimeError> {
    if verification.exact {
        return Ok(verification);
    }
    Err(harness_error(format!(
        "{harness} configuration verification failed at {}: {}",
        verification.path.display(),
        verification
            .reason
            .as_deref()
            .unwrap_or("configuration differs")
    )))
}

fn harness_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::ProtocolError, "harness_install", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(arguments: Vec<OsString>) -> Vec<String> {
        arguments
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn builds_opencode_arguments_without_shell_joining() {
        let bridge = os_strings([
            "/path with spaces/mcp-host",
            "--runtime-dir",
            "/runtime path",
            "mcp",
        ]);
        let arguments = opencode_arguments("dynamic-mcp", &bridge);

        assert_eq!(
            strings(arguments),
            [
                "mcp",
                "add",
                "dynamic-mcp",
                "--",
                "/path with spaces/mcp-host",
                "--runtime-dir",
                "/runtime path",
                "mcp",
            ]
        );
    }

    #[test]
    fn builds_claude_arguments_with_scope_and_transport() {
        let bridge = os_strings(["/bin/mcp-host", "mcp"]);
        let arguments = claude_add_arguments("dynamic-mcp", ClaudeScope::Project, &bridge);

        assert_eq!(
            strings(arguments),
            [
                "mcp",
                "add",
                "--scope",
                "project",
                "--transport",
                "stdio",
                "dynamic-mcp",
                "--",
                "/bin/mcp-host",
                "mcp",
            ]
        );
        assert_eq!(
            strings(claude_remove_arguments("dynamic-mcp", ClaudeScope::Project)),
            ["mcp", "remove", "dynamic-mcp", "--scope", "project"]
        );
    }

    #[test]
    fn truncates_child_error_details_on_character_boundaries() {
        let detail = "ş".repeat(ERROR_DETAIL_LIMIT);
        let rendered = command_error_detail(detail.as_bytes(), b"");
        assert!(rendered.ends_with("..."));
        assert!(rendered.len() <= ERROR_DETAIL_LIMIT + 4);
    }

    #[test]
    fn prefers_stderr_for_child_error_details() {
        assert_eq!(command_error_detail(b"stderr\n", b"stdout\n"), "stderr");
        assert_eq!(command_error_detail(b"", b"stdout\n"), "stdout");
    }

    #[test]
    fn makes_registered_runtime_directories_absolute() {
        let runtime_dir = absolute_runtime_dir(Some(Path::new("relative runtime")))
            .expect("runtime path")
            .expect("runtime override");

        assert!(runtime_dir.is_absolute());
        assert!(runtime_dir.ends_with("relative runtime"));
    }

    #[test]
    fn custom_bridge_command_uses_only_explicit_arguments() {
        let bridge_file = tempfile::NamedTempFile::new().expect("temporary bridge");
        let install = HarnessInstall {
            target: HarnessTarget::OpenCode,
            name: "dynamic-mcp".to_owned(),
            scope: ClaudeScope::User,
            bridge_command: Some(bridge_file.path().to_owned()),
            bridge_arg: vec!["--custom".to_owned(), "argument with spaces".to_owned()],
        };

        let bridge = bridge_arguments(&install, None).expect("custom bridge");
        assert_eq!(
            strings(bridge),
            [
                bridge_file.path().to_string_lossy().into_owned(),
                "--custom".to_owned(),
                "argument with spaces".to_owned(),
            ]
        );
        assert!(bridge_arguments(&install, Some(Path::new("runtime"))).is_err());
    }
}
