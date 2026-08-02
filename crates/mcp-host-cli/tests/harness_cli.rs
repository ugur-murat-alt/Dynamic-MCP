#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

#[test]
fn installs_both_harnesses_with_separate_arguments() {
    let environment = FakeHarnessEnvironment::new();
    let runtime_dir = environment.root.path().join("runtime path");
    let output = environment.run([
        "--json",
        "--runtime-dir",
        runtime_dir.to_str().expect("UTF-8 runtime path"),
        "harness",
        "install",
        "all",
        "--scope",
        "project",
    ]);

    assert_success(&output);
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON harness result");
    assert_eq!(
        result["installed"].as_array().map(Vec::len),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(result["daemonRequired"], true);
    assert_eq!(result["installed"][0]["verified"], true);
    assert_eq!(result["installed"][0]["configUpdated"], true);
    assert_eq!(result["installed"][1]["verified"], true);
    assert_eq!(result["installed"][1]["configUpdated"], true);

    let opencode = environment.capture("opencode");
    assert!(opencode.starts_with("CALL\n<mcp>\n<add>\n<dynamic-mcp>\n<-->\n"));
    assert!(opencode.contains("<--runtime-dir>\n"));
    assert!(opencode.contains(&format!("<{}>\n", runtime_dir.display())));
    assert!(opencode.ends_with("<mcp>\n"));

    let claude = environment.capture("claude");
    assert!(
        claude.starts_with("CALL\n<mcp>\n<remove>\n<dynamic-mcp>\n<--scope>\n<project>\nCALL\n")
    );
    assert!(claude.contains(
        "<mcp>\n<add>\n<--scope>\n<project>\n<--transport>\n<stdio>\n<dynamic-mcp>\n<-->\n"
    ));
    assert!(claude.ends_with("<mcp>\n"));

    let agents = fs::read_to_string(environment.config_dir().join("opencode/AGENTS.md"))
        .expect("OpenCode managed instructions");
    let claude_instructions = fs::read_to_string(environment.home_dir().join(".claude/CLAUDE.md"))
        .expect("Claude managed instructions");
    assert!(agents.contains("<!-- dynamic-mcp:start -->"));
    assert!(claude_instructions.contains("<!-- dynamic-mcp:start -->"));
    assert!(
        environment
            .config_dir()
            .join("opencode/skills/dynamic-mcp/SKILL.md")
            .is_file()
    );
    assert!(
        environment
            .home_dir()
            .join(".claude/skills/dynamic-mcp/SKILL.md")
            .is_file()
    );

    let second = environment.run([
        "--json",
        "--runtime-dir",
        runtime_dir.to_str().expect("UTF-8 runtime path"),
        "harness",
        "install",
        "all",
        "--scope",
        "project",
    ]);
    assert_success(&second);
    let second_result: Value = serde_json::from_slice(&second.stdout).expect("second JSON result");
    assert_eq!(second_result["installed"][0]["configUpdated"], false);
    assert_eq!(second_result["installed"][0]["skillUpdated"], false);
    assert_eq!(second_result["installed"][0]["instructionUpdated"], false);
    assert_eq!(second_result["installed"][1]["configUpdated"], false);
    assert_eq!(environment.capture("opencode"), opencode);
    assert_eq!(environment.capture("claude"), claude);
}

#[test]
fn reports_a_missing_harness_cli_without_polluting_success_output() {
    let environment = FakeHarnessEnvironment::without_programs();
    let output = environment.run(["--json", "harness", "install", "opencode"]);

    assert_eq!(output.status.code(), Some(4));
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON error result");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("not found on PATH"))
    );
}

#[test]
fn repairs_mismatched_config_and_preserves_user_instructions() {
    let environment = FakeHarnessEnvironment::new();
    let opencode_dir = environment.config_dir().join("opencode");
    fs::create_dir(&opencode_dir).expect("OpenCode directory");
    fs::write(
        opencode_dir.join("opencode.jsonc"),
        r#"{
          // Deliberately stale registration.
          "mcp": {
            "dynamic-mcp": {
              "type": "local",
              "command": ["/old/mcp-host", "mcp"],
            },
          },
        }"#,
    )
    .expect("stale OpenCode config");
    fs::write(
        opencode_dir.join("AGENTS.md"),
        "# User policy\n\nKeep this text.\n",
    )
    .expect("existing user instructions");

    let output = environment.run(["--json", "harness", "install", "opencode"]);
    assert_success(&output);
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON harness result");
    assert_eq!(result["installed"][0]["configUpdated"], true);
    assert_eq!(result["installed"][0]["verified"], true);
    let instructions =
        fs::read_to_string(opencode_dir.join("AGENTS.md")).expect("updated user instructions");
    assert!(instructions.starts_with("# User policy\n\nKeep this text."));
    assert_eq!(
        instructions.matches("<!-- dynamic-mcp:start -->").count(),
        1
    );
}

#[test]
fn rejects_cli_success_when_readback_does_not_match() {
    let environment = FakeHarnessEnvironment::without_programs();
    environment.write_noop_program("opencode");

    let output = environment.run(["--json", "harness", "install", "opencode"]);
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON error result");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("configuration verification failed"))
    );
}

#[test]
fn registers_an_explicit_bridge_wrapper_without_implicit_arguments() {
    let environment = FakeHarnessEnvironment::new();
    let wrapper = environment.root.path().join("bridge wrapper");
    fs::write(&wrapper, "#!/bin/sh\nexit 0\n").expect("bridge wrapper");
    let mut permissions = fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).expect("wrapper permissions");

    let output = environment.run([
        "--json",
        "harness",
        "install",
        "opencode",
        "--bridge-command",
        wrapper.to_str().expect("UTF-8 wrapper path"),
    ]);
    assert_success(&output);
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON harness result");
    assert_eq!(
        result["installed"][0]["command"][0],
        wrapper.to_string_lossy().as_ref()
    );
    assert_eq!(
        result["installed"][0]["command"].as_array().map(Vec::len),
        Some(1)
    );
}

struct FakeHarnessEnvironment {
    root: TempDir,
    bin_dir: PathBuf,
}

impl FakeHarnessEnvironment {
    fn new() -> Self {
        let environment = Self::without_programs();
        environment.write_program("opencode");
        environment.write_program("claude");
        environment
    }

    fn without_programs() -> Self {
        let root = tempfile::tempdir().expect("temporary harness directory");
        let bin_dir = root.path().join("bin");
        fs::create_dir(&bin_dir).expect("fake bin directory");
        fs::create_dir(root.path().join("home")).expect("fake home directory");
        fs::create_dir(root.path().join("config")).expect("fake config directory");
        Self { root, bin_dir }
    }

    fn write_program(&self, name: &str) {
        let path = self.bin_dir.join(name);
        let setup = match name {
            "opencode" => {
                r#"
config_dir="$XDG_CONFIG_HOME/opencode"
/bin/mkdir -p "$config_dir"
config="$config_dir/opencode.json"
if [ -f "$config_dir/opencode.jsonc" ]; then config="$config_dir/opencode.jsonc"; fi
if [ "$1" = "mcp" ] && [ "$2" = "add" ]; then
  if [ "$#" = "5" ]; then
    printf '{"mcp":{"%s":{"type":"local","command":["%s"],"enabled":true}}}\n' "$3" "$5" > "$config"
  elif [ "$6" = "--runtime-dir" ]; then
    printf '{"mcp":{"%s":{"type":"local","command":["%s","--runtime-dir","%s","mcp"],"enabled":true}}}\n' "$3" "$5" "$7" > "$config"
  else
    printf '{"mcp":{"%s":{"type":"local","command":["%s","mcp"],"enabled":true}}}\n' "$3" "$5" > "$config"
  fi
fi
"#
            }
            "claude" => {
                r#"
if [ "$1" = "mcp" ] && [ "$2" = "add" ]; then
  if [ "$4" = "project" ]; then config="$PWD/.mcp.json"; else config="$HOME/.claude.json"; fi
  if [ "${10}" = "--runtime-dir" ]; then
    printf '{"mcpServers":{"%s":{"type":"stdio","command":"%s","args":["--runtime-dir","%s","mcp"]}}}\n' "$7" "$9" "${11}" > "$config"
  else
    printf '{"mcpServers":{"%s":{"type":"stdio","command":"%s","args":["mcp"]}}}\n' "$7" "$9" > "$config"
  fi
fi
"#
            }
            _ => unreachable!("unsupported fake harness"),
        };
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n{{ printf 'CALL\\n'; for argument in \"$@\"; do printf '<%s>\\n' \"$argument\"; done; }} >> \"$CAPTURE_DIR/{name}\"\n{setup}"
            ),
        )
        .expect("fake harness executable");
        let mut permissions = fs::metadata(&path)
            .expect("fake harness metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("fake harness permissions");
    }

    fn write_noop_program(&self, name: &str) {
        let path = self.bin_dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("fake no-op harness executable");
        let mut permissions = fs::metadata(&path)
            .expect("fake harness metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("fake harness permissions");
    }

    fn run<const N: usize>(&self, arguments: [&str; N]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mcp-host"))
            .args(arguments)
            .env("PATH", &self.bin_dir)
            .env("CAPTURE_DIR", self.root.path())
            .env("HOME", self.home_dir())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .env_remove("XDG_RUNTIME_DIR")
            .current_dir(self.root.path())
            .output()
            .expect("run mcp-host")
    }

    fn home_dir(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }

    fn capture(&self, name: &str) -> String {
        fs::read_to_string(self.root.path().join(name)).expect("captured harness arguments")
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
