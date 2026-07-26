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
        Self { root, bin_dir }
    }

    fn write_program(&self, name: &str) {
        let path = self.bin_dir.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n{{ printf 'CALL\\n'; for argument in \"$@\"; do printf '<%s>\\n' \"$argument\"; done; }} >> \"$CAPTURE_DIR/{name}\"\n"
            ),
        )
        .expect("fake harness executable");
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
            .env_remove("HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .expect("run mcp-host")
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
