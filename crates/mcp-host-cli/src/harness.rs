use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use mcp_host_core::{RuntimeError, RuntimeErrorCode};
use serde_json::{Value, json};
use tokio::{process::Command, time::timeout};

use crate::cli::{ClaudeScope, HarnessInstall, HarnessTarget};

const HARNESS_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const ERROR_DETAIL_LIMIT: usize = 2_048;

pub async fn install_harnesses(
    install: HarnessInstall,
    runtime_dir: Option<&Path>,
) -> Result<Value, RuntimeError> {
    let executable = canonical_executable()?;
    let runtime_dir = absolute_runtime_dir(runtime_dir)?;
    let runtime_dir = runtime_dir.as_deref();
    let mut installed = Vec::new();

    if matches!(install.target, HarnessTarget::OpenCode | HarnessTarget::All) {
        let arguments = opencode_arguments(&install.name, &executable, runtime_dir);
        run_harness_command("opencode", &arguments, true).await?;
        installed.push(json!({
            "harness": "opencode",
            "name": install.name,
            "scope": "global",
            "command": bridge_command_json(&executable, runtime_dir),
        }));
    }

    if matches!(
        install.target,
        HarnessTarget::ClaudeCode | HarnessTarget::All
    ) {
        let remove_arguments = claude_remove_arguments(&install.name, install.scope);
        // Claude Code rejects duplicate names. Removing only the requested scope makes
        // repeated installs deterministic; a missing entry is expected on first install.
        run_harness_command("claude", &remove_arguments, false).await?;

        let arguments =
            claude_add_arguments(&install.name, install.scope, &executable, runtime_dir);
        run_harness_command("claude", &arguments, true).await?;
        installed.push(json!({
            "harness": "claude-code",
            "name": install.name,
            "scope": install.scope.as_str(),
            "command": bridge_command_json(&executable, runtime_dir),
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

fn canonical_executable() -> Result<PathBuf, RuntimeError> {
    let executable = std::env::current_exe().map_err(|error| {
        harness_error(format!(
            "could not locate the current mcp-host executable: {error}"
        ))
    })?;

    std::fs::canonicalize(&executable).map_err(|error| {
        harness_error(format!(
            "could not resolve executable path {}: {error}",
            executable.display()
        ))
    })
}

fn bridge_arguments(executable: &Path, runtime_dir: Option<&Path>) -> Vec<OsString> {
    let mut arguments = vec![executable.as_os_str().to_owned()];
    if let Some(runtime_dir) = runtime_dir {
        arguments.extend([
            OsString::from("--runtime-dir"),
            runtime_dir.as_os_str().to_owned(),
        ]);
    }
    arguments.push(OsString::from("mcp"));
    arguments
}

fn opencode_arguments(name: &str, executable: &Path, runtime_dir: Option<&Path>) -> Vec<OsString> {
    let mut arguments = os_strings(["mcp", "add", name, "--"]);
    arguments.extend(bridge_arguments(executable, runtime_dir));
    arguments
}

fn claude_add_arguments(
    name: &str,
    scope: ClaudeScope,
    executable: &Path,
    runtime_dir: Option<&Path>,
) -> Vec<OsString> {
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
    arguments.extend(bridge_arguments(executable, runtime_dir));
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

fn bridge_command_json(executable: &Path, runtime_dir: Option<&Path>) -> Vec<String> {
    bridge_arguments(executable, runtime_dir)
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
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
        let arguments = opencode_arguments(
            "dynamic-mcp",
            Path::new("/path with spaces/mcp-host"),
            Some(Path::new("/runtime path")),
        );

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
        let arguments = claude_add_arguments(
            "dynamic-mcp",
            ClaudeScope::Project,
            Path::new("/bin/mcp-host"),
            None,
        );

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
}
