use std::{
    fs,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use mcp_host::ipc::send_control;
use mcp_host_core::ControlRequest;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(8);

#[tokio::test]
async fn real_daemon_and_cli_flow_persists_state_between_commands() {
    let mut daemon = TestDaemon::start().await;

    let listed = daemon.cli_json(&["list"]);
    assert_success(&listed);
    assert_eq!(parse_stdout(&listed)[0]["id"], "fixture");

    let connected = daemon.cli_json(&["connect", "fixture"]);
    assert_success(&connected);
    assert_eq!(parse_stdout(&connected)["state"], "connected");

    let tools = daemon.cli_json(&["tools", "fixture"]);
    assert_success(&tools);
    assert_eq!(parse_stdout(&tools)["tool_count"], 5);

    let called = daemon.cli_json(&[
        "call",
        "fixture",
        "echo",
        "--arguments",
        r#"{"message":"CLI works"}"#,
    ]);
    assert_success(&called);
    assert_eq!(
        parse_stdout(&called)["structuredContent"]["message"],
        "CLI works"
    );

    let batch_started = Instant::now();
    let batched = daemon.cli_json(&[
        "batch",
        "--calls",
        r#"[{"server_id":"fixture","tool_name":"sleep","arguments":{"milliseconds":500}},{"server_id":"fixture","tool_name":"sleep","arguments":{"milliseconds":500}},{"server_id":"fixture","tool_name":"echo","arguments":{"message":"batch CLI"}}]"#,
    ]);
    assert_success(&batched);
    assert!(batch_started.elapsed() < Duration::from_millis(900));
    let batched = parse_stdout(&batched);
    assert_eq!(batched["results"][0]["status"], "success");
    assert_eq!(batched["results"][1]["status"], "success");
    assert_eq!(
        batched["results"][2]["result"]["structuredContent"]["message"],
        "batch CLI"
    );

    let partial_error = daemon.cli_json(&[
        "batch",
        "--calls",
        r#"[{"server_id":"fixture","tool_name":"missing"},{"server_id":"fixture","tool_name":"echo","arguments":{"message":"survived"}}]"#,
    ]);
    assert_eq!(partial_error.status.code(), Some(4));
    let partial_error = parse_stdout(&partial_error);
    assert_eq!(
        partial_error["results"][0]["error"]["code"],
        "TOOL_NOT_FOUND"
    );
    assert_eq!(
        partial_error["results"][1]["result"]["structuredContent"]["message"],
        "survived"
    );

    let upstream_error = daemon.cli_json(&[
        "batch",
        "--calls",
        r#"[{"server_id":"fixture","tool_name":"fail"}]"#,
    ]);
    assert_eq!(upstream_error.status.code(), Some(5));
    assert_eq!(
        parse_stdout(&upstream_error)["results"][0]["result"]["isError"],
        true
    );

    let refreshed = daemon.cli_json(&["refresh", "fixture"]);
    assert_success(&refreshed);
    assert_eq!(parse_stdout(&refreshed)["tool_count"], 5);

    let inspected = daemon.cli_json(&["inspect", "fixture"]);
    assert_success(&inspected);
    assert_eq!(parse_stdout(&inspected)["observed_state"], "connected");

    let disconnected = daemon.cli_json(&["disconnect", "fixture"]);
    assert_success(&disconnected);
    assert_eq!(parse_stdout(&disconnected)["state"], "disconnected");
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn real_rmcp_client_reaches_upstream_through_bridge_and_daemon() {
    let mut daemon = TestDaemon::start().await;
    let client = host_client(&daemon).await;

    let mut tools = client
        .list_all_tools()
        .await
        .expect("host tools should list")
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect::<Vec<_>>();
    tools.sort_unstable();
    assert_eq!(
        tools,
        [
            "call_tool",
            "call_tools",
            "connect_server",
            "disconnect_server",
            "inspect_server",
            "list_servers",
            "list_tools",
            "refresh_server",
            "status",
        ]
    );

    let listed = call_host_tool(&client, "list_servers", json!({})).await;
    assert_eq!(
        host_envelope(&listed, "list_servers")["data"]["servers"][0]["id"],
        "fixture"
    );

    let connected =
        call_host_tool(&client, "connect_server", json!({"server_id": "fixture"})).await;
    assert_eq!(
        host_envelope(&connected, "connect_server")["data"]["state"],
        "connected"
    );

    let upstream_tools = call_host_tool(
        &client,
        "list_tools",
        json!({"server_id": "fixture", "refresh": false}),
    )
    .await;
    assert_eq!(
        host_envelope(&upstream_tools, "list_tools")["data"]["tool_count"],
        5
    );

    let echo = call_host_tool(
        &client,
        "call_tool",
        json!({
            "server_id": "fixture",
            "tool_name": "echo",
            "arguments": {"message": "full chain"}
        }),
    )
    .await;
    assert_eq!(
        host_envelope(&echo, "call_tool")["data"]["result"]["structuredContent"]["message"],
        "full chain"
    );

    let batch = call_host_tool(
        &client,
        "call_tools",
        json!({
            "calls": [
                {
                    "server_id": "fixture",
                    "tool_name": "echo",
                    "arguments": {"message": "host batch"}
                },
                {"server_id": "fixture", "tool_name": "missing"},
                {"server_id": "fixture", "tool_name": "fail"}
            ]
        }),
    )
    .await;
    let batch = host_envelope(&batch, "call_tools");
    assert_eq!(batch["data"]["results"][0]["tool_name"], "echo");
    assert_eq!(
        batch["data"]["results"][0]["result"]["structuredContent"]["message"],
        "host batch"
    );
    assert_eq!(batch["data"]["results"][1]["status"], "error");
    assert_eq!(
        batch["data"]["results"][1]["error"]["code"],
        "TOOL_NOT_FOUND"
    );
    assert_eq!(batch["data"]["results"][2]["status"], "success");
    assert_eq!(batch["data"]["results"][2]["result"]["isError"], true);

    call_host_tool(
        &client,
        "disconnect_server",
        json!({"server_id": "fixture"}),
    )
    .await;
    client.cancel().await.expect("host client should close");
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn cli_and_two_mcp_clients_share_one_upstream_runtime() {
    let mut daemon = TestDaemon::start().await;
    assert_success(&daemon.cli_json(&["connect", "fixture"]));
    let client_a = host_client(&daemon).await;
    let client_b = host_client(&daemon).await;

    let inspected =
        call_host_tool(&client_a, "inspect_server", json!({"server_id": "fixture"})).await;
    assert_eq!(
        host_envelope(&inspected, "inspect_server")["data"]["observed_state"],
        "connected"
    );
    let echo = call_host_tool(
        &client_a,
        "call_tool",
        json!({
            "server_id": "fixture",
            "tool_name": "echo",
            "arguments": {"message": "shared"}
        }),
    )
    .await;
    assert_eq!(
        host_envelope(&echo, "call_tool")["data"]["result"]["structuredContent"]["message"],
        "shared"
    );
    let tools = call_host_tool(
        &client_b,
        "list_tools",
        json!({"server_id": "fixture", "refresh": false}),
    )
    .await;
    assert_eq!(host_envelope(&tools, "list_tools")["data"]["tool_count"], 5);
    assert_eq!(daemon.startup_count(), 1);

    call_host_tool(
        &client_b,
        "disconnect_server",
        json!({"server_id": "fixture"}),
    )
    .await;
    let cli_inspection = daemon.cli_json(&["inspect", "fixture"]);
    assert_success(&cli_inspection);
    assert_eq!(
        parse_stdout(&cli_inspection)["observed_state"],
        "disconnected"
    );

    client_a.cancel().await.expect("client A should close");
    client_b.cancel().await.expect("client B should close");
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn second_daemon_is_rejected_without_harming_the_active_daemon() {
    let mut daemon = TestDaemon::start().await;
    let second = Command::new(host_binary())
        .args(["daemon", "run", "--config-dir"])
        .arg(&daemon.config_dir)
        .arg("--runtime-dir")
        .arg(&daemon.runtime_dir)
        .arg("--json")
        .output()
        .expect("second daemon should run and exit");
    assert_eq!(second.status.code(), Some(4));
    assert_eq!(
        parse_stdout(&second)["error"]["code"],
        "DAEMON_ALREADY_RUNNING"
    );

    let status = daemon.cli_json(&["daemon", "status"]);
    assert_success(&status);
    assert_eq!(parse_stdout(&status)["registry_server_count"], 1);
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn daemon_hot_reload_adds_removes_and_rejects_invalid_snapshots() {
    let mut daemon = TestDaemon::start().await;
    let second = daemon.config_dir.join("second.toml");
    fs::write(
        &second,
        "id = \"second\"\nname = \"Second\"\ndescription = \"Hot reload\"\nenabled = false\n[transport]\ntype = \"stdio\"\ncommand = \"unused\"\n",
    )
    .expect("second manifest should be written");
    daemon.wait_server_count(2).await;

    let invalid = daemon.config_dir.join("invalid.toml");
    fs::write(&invalid, "not = \"a server manifest\"\n")
        .expect("invalid manifest should be written");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        parse_stdout(&daemon.cli_json(&["list"]))
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    fs::remove_file(invalid).expect("invalid manifest should be removed");
    fs::remove_file(second).expect("second manifest should be removed");
    daemon.wait_server_count(1).await;
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn daemon_hot_reload_publishes_policy_changes() {
    let mut daemon = TestDaemon::start().await;
    let policy = daemon.config_dir.join("policy.toml");
    fs::write(
        &policy,
        "[[rules]]\nid = \"hide-fixture\"\naction = \"list\"\neffect = \"deny\"\nserver = \"fixture\"\n",
    )
    .expect("policy should be written");
    daemon.wait_server_count(0).await;

    fs::remove_file(policy).expect("policy should be removed");
    daemon.wait_server_count(1).await;
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn daemon_hot_reload_runs_skills_and_preserves_the_last_valid_snapshot() {
    let mut daemon = TestDaemon::start().await;
    let skill_path = daemon.config_dir.join("echo.skill.toml");
    let valid_skill = r#"
        id = "echo-skill"
        name = "Echo skill"
        description = "Daemon skill fixture"

        [[inputs]]
        name = "message"
        type = "string"

        [[steps]]
        id = "echo"
        server = "fixture"
        tool = "echo"
        arguments = { message = "${input.message}" }
    "#;
    fs::write(&skill_path, valid_skill).expect("skill should be written");
    daemon.wait_skill_count(1).await;
    assert_eq!(
        parse_stdout(&daemon.cli_json(&["list"]))
            .as_array()
            .map(Vec::len),
        Some(1),
        "skill files must not be parsed as server manifests"
    );
    assert_success(&daemon.cli_json(&["connect", "fixture"]));
    let output = daemon.cli_json(&[
        "skill",
        "run",
        "echo-skill",
        "--input",
        "{\"message\":\"hello\"}",
    ]);
    assert_success(&output);
    assert_eq!(parse_stdout(&output)["status"], "ok");
    assert_eq!(
        parse_stdout(&output)["results"][0]["result"]["structuredContent"]["message"],
        "hello"
    );

    fs::write(
        &skill_path,
        "id='echo-skill'\nname='Invalid'\ndescription='Invalid'\n[[steps]]\nid='first'\nserver='fixture'\ntool='echo'\narguments={message='${steps.later.output}'}\n",
    )
    .expect("invalid skill should be written");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let retained = daemon.cli_json(&[
        "skill",
        "run",
        "echo-skill",
        "--input",
        "{\"message\":\"retained\"}",
    ]);
    assert_success(&retained);
    assert_eq!(parse_stdout(&retained)["status"], "ok");

    fs::write(&skill_path, valid_skill).expect("valid skill should be restored");
    fs::write(
        daemon.config_dir.join("policy.toml"),
        "[[rules]]\nid='deny-skill'\naction='skill_run'\neffect='deny'\nskill='echo-*'\n",
    )
    .expect("skill policy should be written");
    daemon.wait_skill_denied("echo-skill").await;
    fs::remove_file(daemon.config_dir.join("policy.toml")).expect("policy should be removed");
    daemon.wait_skill_allowed("echo-skill").await;

    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn package_install_requires_a_manifest_provision_section() {
    let mut daemon = TestDaemon::start().await;
    let output = daemon.cli_json(&["package", "install", "fixture"]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        parse_stdout(&output)["error"]["code"],
        "PACKAGE_NOT_CONFIGURED"
    );
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn oauth_status_requires_an_auth_manifest_section() {
    let mut daemon = TestDaemon::start().await;
    let output = daemon.cli_json(&["auth", "status", "fixture"]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        parse_stdout(&output)["error"]["code"],
        "AUTH_NOT_CONFIGURED"
    );
    daemon.stop();
    daemon.assert_clean_shutdown();
}

#[tokio::test]
async fn shutdown_closes_active_downstream_and_upstream_sessions() {
    let mut daemon = TestDaemon::start().await;
    assert_success(&daemon.cli_json(&["connect", "fixture"]));
    let pid = daemon.fixture_pid();
    let client = host_client(&daemon).await;

    daemon.stop();
    daemon.assert_clean_shutdown();
    wait_for_process_exit(pid);
    let reason = tokio::time::timeout(Duration::from_secs(3), client.waiting())
        .await
        .expect("downstream MCP session should close before timeout")
        .expect("downstream MCP task should join");
    assert!(matches!(
        reason,
        rmcp::service::QuitReason::Cancelled | rmcp::service::QuitReason::Closed
    ));
}

async fn host_client(
    daemon: &TestDaemon,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
    let transport = TokioChildProcess::new(tokio::process::Command::new(host_binary()).configure(
        |command| {
            command
                .arg("mcp")
                .arg("--runtime-dir")
                .arg(&daemon.runtime_dir)
                .stderr(Stdio::null());
        },
    ))
    .expect("bridge should start");
    ().serve(transport)
        .await
        .expect("host MCP initialize should complete")
}

async fn call_host_tool(
    client: &rmcp::Peer<rmcp::service::RoleClient>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    let arguments = arguments.as_object().cloned().unwrap_or_else(Map::new);
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .expect("host tool should succeed")
}

fn host_envelope<'a>(result: &'a CallToolResult, operation: &str) -> &'a Value {
    let envelope = result
        .structured_content
        .as_ref()
        .expect("structured result");
    assert_eq!(envelope["schema_version"], "dynamic-mcp/v1");
    assert_eq!(envelope["operation"], operation);
    assert_eq!(envelope["ok"], true);
    envelope
}

struct TestDaemon {
    _root: TempDir,
    config_dir: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
    startup_counter: std::path::PathBuf,
    pid_file: std::path::PathBuf,
    child: Option<Child>,
}

impl TestDaemon {
    async fn start() -> Self {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let config_dir = root.path().join("config");
        let runtime_dir = root.path().join("runtime");
        let startup_counter = root.path().join("starts.txt");
        let pid_file = root.path().join("fixture.pid");
        fs::create_dir(&config_dir).expect("config directory should be created");
        let fixture = fixture_binary();
        let manifest = format!(
            "id = \"fixture\"\nname = \"Fixture\"\ndescription = \"E2E fixture\"\n[transport]\ntype = \"stdio\"\ncommand = {fixture:?}\nargs = [\"--startup-counter-file\", {startup_counter:?}, \"--pid-file\", {pid_file:?}]\n"
        );
        fs::write(config_dir.join("fixture.toml"), manifest)
            .expect("fixture manifest should be written");
        let child = Command::new(host_binary())
            .args(["daemon", "run", "--config-dir"])
            .arg(&config_dir)
            .arg("--runtime-dir")
            .arg(&runtime_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon should start");
        let daemon = Self {
            _root: root,
            config_dir,
            runtime_dir,
            startup_counter,
            pid_file,
            child: Some(child),
        };
        daemon.wait_ready().await;
        daemon
    }

    async fn wait_ready(&self) {
        tokio::time::timeout(PROCESS_TIMEOUT, async {
            loop {
                if send_control(
                    &self.runtime_dir,
                    &ControlRequest::Ping,
                    Duration::from_millis(100),
                )
                .await
                .is_ok()
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("daemon should become ready");
    }

    async fn wait_server_count(&self, expected: usize) {
        tokio::time::timeout(PROCESS_TIMEOUT, async {
            loop {
                let output = self.cli_json(&["list"]);
                if output.status.success()
                    && parse_stdout(&output).as_array().map(Vec::len) == Some(expected)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("daemon should publish the reloaded registry");
    }

    async fn wait_skill_count(&self, expected: usize) {
        tokio::time::timeout(PROCESS_TIMEOUT, async {
            loop {
                let output = self.cli_json(&["skill", "list"]);
                if output.status.success()
                    && parse_stdout(&output).as_array().map(Vec::len) == Some(expected)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("daemon should publish the reloaded skill catalog");
    }

    async fn wait_skill_denied(&self, skill_id: &str) {
        tokio::time::timeout(PROCESS_TIMEOUT, async {
            loop {
                let output = self.cli_json(&["skill", "run", skill_id]);
                if output.status.code() == Some(4)
                    && parse_stdout(&output)["error"]["code"] == "POLICY_DENIED"
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("daemon should publish the skill deny policy");
    }

    async fn wait_skill_allowed(&self, skill_id: &str) {
        tokio::time::timeout(PROCESS_TIMEOUT, async {
            loop {
                let output = self.cli_json(&[
                    "skill",
                    "run",
                    skill_id,
                    "--input",
                    "{\"message\":\"allowed\"}",
                ]);
                if output.status.success() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("daemon should remove the skill deny policy");
    }

    fn cli_json(&self, arguments: &[&str]) -> Output {
        Command::new(host_binary())
            .args(arguments)
            .arg("--runtime-dir")
            .arg(&self.runtime_dir)
            .arg("--json")
            .output()
            .expect("CLI command should run")
    }

    fn startup_count(&self) -> u64 {
        fs::read_to_string(&self.startup_counter)
            .expect("startup counter should exist")
            .trim()
            .parse()
            .expect("startup counter should be numeric")
    }

    fn fixture_pid(&self) -> u32 {
        fs::read_to_string(&self.pid_file)
            .expect("fixture PID should exist")
            .trim()
            .parse()
            .expect("fixture PID should be numeric")
    }

    fn stop(&self) {
        let output = self.cli_json(&["daemon", "stop"]);
        assert_success(&output);
    }

    fn assert_clean_shutdown(&mut self) {
        let child = self.child.as_mut().expect("daemon child should exist");
        let status = wait_for_child(child);
        assert!(status.success(), "daemon should exit successfully");
        self.child = None;
        assert!(!self.runtime_dir.join("control.sock").exists());
        assert!(!self.runtime_dir.join("mcp.sock").exists());
        assert!(!self.runtime_dir.join("daemon.json").exists());
        assert!(!self.runtime_dir.join("daemon.lock").exists());
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = Command::new(host_binary())
                .args(["daemon", "stop", "--runtime-dir"])
                .arg(&self.runtime_dir)
                .output();
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn parse_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON")
}

fn wait_for_child(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("daemon status should be readable") {
            return status;
        }
        assert!(Instant::now() < deadline, "daemon shutdown timed out");
        std::thread::yield_now();
    }
}

fn wait_for_process_exit(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        let process = std::path::PathBuf::from(format!("/proc/{pid}"));
        while process.exists() {
            assert!(Instant::now() < deadline, "fixture process did not exit");
            std::thread::yield_now();
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

fn host_binary() -> &'static str {
    env!("CARGO_BIN_EXE_mcp-host")
}

fn fixture_binary() -> &'static str {
    env!("CARGO_BIN_EXE_mcp-host-fixture-server")
}
