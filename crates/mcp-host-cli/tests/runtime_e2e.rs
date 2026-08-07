use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use mcp_host_core::{
    BatchToolCall, BatchToolCallOutcome, CallPolicy, LifecycleState, MAX_BATCH_CALLS,
    ManifestLoader, Policy, ProcessEnvironment, RegistryBuilder, RuntimeErrorCode, SkillCatalog,
    SkillRunStatus,
};
use mcp_host_mcp::{RuntimeManager, RuntimeSettings};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn real_stdio_initialize_discover_call_timeout_and_disconnect() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();

    let connected = manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should initialize");
    assert_eq!(connected.state, LifecycleState::Connected);
    assert_eq!(connected.protocol_version, "2025-11-25");
    assert_eq!(connected.tool_count, 5);

    let tools = manager
        .list_tools("fixture", false)
        .await
        .expect("tools should be cached");
    assert_eq!(tools.tool_count, 5);
    assert_eq!(
        tools
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["add", "crash", "echo", "fail", "sleep"]
    );

    let echo = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "hello"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("echo should succeed");
    assert_eq!(echo.value()["structuredContent"]["message"], "hello");

    let add = manager
        .call_tool(
            "fixture",
            "add",
            json!({"a": 2, "b": 3}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("add should succeed");
    assert_eq!(add.value()["structuredContent"]["sum"], 5);

    let failure = manager
        .call_tool("fixture", "fail", json!({}), None, CallPolicy::default())
        .await
        .expect("tool-level failure remains a valid MCP result");
    assert_eq!(failure.value()["isError"], true);

    let timeout = manager
        .call_tool(
            "fixture",
            "sleep",
            json!({"milliseconds": 1_000}),
            Some(30),
            CallPolicy::default(),
        )
        .await
        .expect_err("sleep should time out");
    assert_eq!(timeout.code.as_str(), "TOOL_CALL_TIMEOUT");

    let after_timeout = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "still-alive"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("session should remain usable after cancellation");
    assert_eq!(
        after_timeout.value()["structuredContent"]["message"],
        "still-alive"
    );

    let pid = fixture.pid();
    let disconnected = manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");
    assert_eq!(disconnected.state, LifecycleState::Disconnected);
    wait_for_process_exit(pid).await;
}

#[tokio::test]
async fn call_auto_connects_a_registered_but_disconnected_server() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");
    manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");

    let result = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "auto-connect"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("auto-connect should connect and call");
    assert_eq!(
        result.value()["structuredContent"]["message"],
        "auto-connect"
    );
    let startup_after_first = fixture.startup_count();
    assert_eq!(
        manager
            .call_tool(
                "fixture",
                "echo",
                json!({"message": "second"}),
                None,
                CallPolicy::default(),
            )
            .await
            .expect("second call should reuse the auto-connected session")
            .value()["structuredContent"]["message"],
        "second"
    );
    assert_eq!(
        fixture.startup_count(),
        startup_after_first,
        "the second call must reuse the auto-connected session"
    );
}

#[tokio::test]
async fn call_with_auto_connect_disabled_requires_an_explicit_connection() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");
    manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");

    let error = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "blocked"}),
            None,
            CallPolicy {
                auto_connect: false,
                ..CallPolicy::default()
            },
        )
        .await
        .expect_err("auto-connect is disabled, the call must fail");
    assert_eq!(error.code, RuntimeErrorCode::ServerNotConnected);
}

#[tokio::test]
async fn misspelled_tool_reports_close_name_suggestions_after_refresh_retry() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let error = manager
        .call_tool(
            "fixture",
            "ech",
            json!({"message": "nope"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect_err("misspelled tool must fail with suggestions");
    assert_eq!(error.code, RuntimeErrorCode::ToolNotFound);
    let suggestions = error
        .suggestions
        .as_ref()
        .expect("suggestions should be attached");
    assert_eq!(
        suggestions
            .iter()
            .map(|suggestion| suggestion.tool_name.as_str())
            .collect::<Vec<_>>(),
        ["echo"]
    );
    assert_eq!(suggestions[0].server_id, "fixture");
}

#[tokio::test]
async fn max_output_tokens_truncates_oversized_results() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let result = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "x".repeat(2_000)}),
            None,
            CallPolicy {
                max_output_tokens: Some(8),
                ..CallPolicy::default()
            },
        )
        .await
        .expect("echo should succeed");
    let value = result.value();
    assert_eq!(value["truncated"], true);
    assert!(
        value["message"]
            .as_str()
            .is_some_and(|message| message.contains("max_output_tokens"))
    );
}

#[tokio::test]
async fn fixture_exposes_resources_through_the_runtime() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let resources = manager
        .list_resources("fixture")
        .await
        .expect("resources should be listed");
    let resources = resources
        .as_array()
        .expect("resource list should be an array");
    assert_eq!(resources[0]["uri"], "fixture://info");
    assert_eq!(resources[0]["name"], "fixture info");

    let read = manager
        .read_resource("fixture", "fixture://info")
        .await
        .expect("resource should be readable");
    assert_eq!(read["contents"][0]["text"], "fixture information resource");
}

#[tokio::test]
async fn resources_require_a_connected_server() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();

    let error = manager
        .list_resources("fixture")
        .await
        .expect_err("disconnected server must fail resource listing");
    assert_eq!(error.code, RuntimeErrorCode::ServerNotConnected);
}

#[tokio::test]
async fn batch_calls_run_concurrently_preserve_order_and_isolate_errors() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let started = Instant::now();
    let response = manager
        .call_tools(vec![
            batch_call("sleep", json!({"milliseconds": 500}), None),
            batch_call("sleep", json!({"milliseconds": 500}), None),
            batch_call("missing", json!({}), None),
            batch_call("fail", json!({}), None),
            batch_call("echo", json!({"message": "ordered"}), None),
            batch_call("sleep", json!({"milliseconds": 10}), Some(0)),
        ])
        .await
        .expect("valid batch should complete");

    assert!(
        started.elapsed() < Duration::from_millis(900),
        "two 500ms calls should run concurrently: {:?}",
        started.elapsed()
    );
    assert_eq!(
        response
            .results
            .iter()
            .map(|result| result.tool_name.as_str())
            .collect::<Vec<_>>(),
        ["sleep", "sleep", "missing", "fail", "echo", "sleep"]
    );
    assert!(matches!(
        response.results[0].outcome,
        BatchToolCallOutcome::Success { .. }
    ));
    assert!(matches!(
        &response.results[2].outcome,
        BatchToolCallOutcome::Error { error } if error.code.as_str() == "TOOL_NOT_FOUND"
    ));
    assert!(matches!(
        &response.results[3].outcome,
        BatchToolCallOutcome::Success { result }
            if result.value()["isError"] == true
    ));
    assert!(matches!(
        &response.results[4].outcome,
        BatchToolCallOutcome::Success { result }
            if result.value()["structuredContent"]["message"] == "ordered"
    ));
    assert!(matches!(
        &response.results[5].outcome,
        BatchToolCallOutcome::Error { error } if error.code.as_str() == "INVALID_ARGUMENTS"
    ));

    let empty = manager
        .call_tools(Vec::new())
        .await
        .expect_err("empty batch should fail");
    assert_eq!(empty.code.as_str(), "INVALID_ARGUMENTS");
    let too_large = manager
        .call_tools(vec![
            batch_call("echo", json!({}), None);
            MAX_BATCH_CALLS + 1
        ])
        .await
        .expect_err("oversized batch should fail");
    assert_eq!(too_large.code.as_str(), "INVALID_ARGUMENTS");

    manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");
}

#[tokio::test]
async fn ten_concurrent_connects_start_one_real_process() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    let results = join_all((0..10).map(|_| {
        let manager = Arc::clone(&manager);
        async move { manager.connect_server("fixture", None).await }
    }))
    .await;

    assert!(results.iter().all(Result::is_ok));
    assert_eq!(fixture.startup_count(), 1);
    manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");
}

#[tokio::test]
async fn ten_concurrent_disconnects_join_one_real_shutdown() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");
    let results = join_all((0..10).map(|_| {
        let manager = Arc::clone(&manager);
        async move { manager.disconnect_server("fixture").await }
    }))
    .await;

    assert!(results.iter().all(Result::is_ok));
    assert_eq!(fixture.startup_count(), 1);
    assert_eq!(
        manager
            .inspect_server("fixture")
            .await
            .expect("fixture should be inspectable")
            .observed_state,
        LifecycleState::Disconnected
    );
}

#[tokio::test]
async fn different_servers_connect_independently_and_shutdown_together() {
    let fixture = FixtureRuntime::new();
    let second_counter = fixture._root.path().join("second-starts.txt");
    let second_pid = fixture._root.path().join("second-pid.txt");
    fs::write(
        fixture.config_dir.join("second.toml"),
        format!(
            "id = \"second\"\nname = \"Second\"\ndescription = \"Second fixture\"\n[transport]\ntype = \"stdio\"\ncommand = {:?}\nargs = [\"--startup-counter-file\", {second_counter:?}, \"--pid-file\", {second_pid:?}]\n",
            env!("CARGO_BIN_EXE_mcp-host-fixture-server")
        ),
    )
    .expect("second manifest should be written");
    let manager = fixture.manager();
    let (first, second) = tokio::join!(
        manager.connect_server("fixture", None),
        manager.connect_server("second", None)
    );

    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(fixture.startup_count(), 1);
    assert_eq!(
        fs::read_to_string(&second_counter)
            .expect("second counter should exist")
            .trim(),
        "1"
    );

    let started = Instant::now();
    let batch = manager
        .call_tools(vec![
            batch_call_for("fixture", "sleep", json!({"milliseconds": 500}), None),
            batch_call_for("second", "sleep", json!({"milliseconds": 500}), None),
        ])
        .await
        .expect("calls across servers should complete");
    assert!(started.elapsed() < Duration::from_millis(900));
    assert_eq!(batch.results[0].server_id, "fixture");
    assert_eq!(batch.results[1].server_id, "second");

    manager.shutdown().await.expect("both servers should stop");
    assert_eq!(manager.connected_count().await, 0);
}

#[tokio::test]
async fn disconnect_during_initialize_cancels_and_reaps_startup() {
    let fixture = FixtureRuntime::new_with_initialize_delay(2_000);
    let manager = fixture.manager();
    let connection = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.connect_server("fixture", None).await })
    };
    wait_for_file(&fixture.pid_file).await;
    let pid = fixture.pid();

    let disconnected = manager
        .disconnect_server("fixture")
        .await
        .expect("disconnect should cancel startup");
    assert_eq!(disconnected.state, LifecycleState::Stopped);
    let connect_error = connection
        .await
        .expect("connect task should join")
        .expect_err("cancelled connect should fail safely");
    assert_eq!(connect_error.code.as_str(), "SERVER_NOT_CONNECTED");
    wait_for_process_exit(pid).await;
}

#[tokio::test]
async fn joined_connect_waiters_do_not_restart_after_a_later_disconnect() {
    let fixture = FixtureRuntime::new_with_initialize_delay(2_000);
    let manager = fixture.manager();
    let first = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.connect_server("fixture", None).await })
    };
    wait_for_file(&fixture.pid_file).await;
    let joiners = (0..10)
        .map(|_| {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.connect_server("fixture", None).await })
        })
        .collect::<Vec<_>>();
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }

    manager
        .disconnect_server("fixture")
        .await
        .expect("later disconnect should win");
    assert!(first.await.expect("first connect should join").is_err());
    for joiner in joiners {
        let error = joiner
            .await
            .expect("joined connect should join")
            .expect_err("joined connect should observe cancellation");
        assert_eq!(error.code.as_str(), "SERVER_NOT_CONNECTED");
    }
    assert_eq!(fixture.startup_count(), 1);
    assert_eq!(
        manager
            .inspect_server("fixture")
            .await
            .expect("fixture should be inspectable")
            .observed_state,
        LifecycleState::Stopped
    );
}

#[tokio::test]
async fn unexpected_crash_becomes_failed_and_reconnects() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let crash = manager
        .call_tool("fixture", "crash", json!({}), None, CallPolicy::default())
        .await
        .expect_err("crash should close the transport");
    assert!(matches!(
        crash.code.as_str(),
        "TRANSPORT_CLOSED" | "TOOL_CALL_FAILED"
    ));

    wait_for_state(&manager, LifecycleState::Failed).await;
    let first_error = manager
        .inspect_server("fixture")
        .await
        .expect("fixture should be inspectable")
        .last_safe_error
        .expect("crash should record a safe error");
    assert!(!first_error.message.is_empty());

    manager
        .connect_server("fixture", None)
        .await
        .expect("failed fixture should reconnect");
    assert_eq!(fixture.startup_count(), 2);
    let echo = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "reconnected"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("reconnected fixture should answer");
    assert_eq!(echo.value()["structuredContent"]["message"], "reconnected");
    manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");
}

#[tokio::test]
async fn enabled_reconnect_policy_recovers_without_an_explicit_connect() {
    let fixture = FixtureRuntime::new_with_reconnect();
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let _ = manager
        .call_tool("fixture", "crash", json!({}), None, CallPolicy::default())
        .await
        .expect_err("crash should close the transport");
    fixture.wait_startup_count(2).await;
    wait_for_state(&manager, LifecycleState::Connected).await;

    let echo = manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "automatic"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("automatically reconnected fixture should answer");
    assert_eq!(echo.value()["structuredContent"]["message"], "automatic");
    manager.shutdown().await.expect("fixture should shut down");
}

#[tokio::test]
async fn runtime_skill_chains_typed_outputs_and_stops_on_tool_errors() {
    let fixture = FixtureRuntime::new();
    fs::write(
        fixture.config_dir.join("chain.skill.toml"),
        r#"
            id = "echo-chain"
            name = "Echo chain"
            description = "Pass one result into the next step"

            [[inputs]]
            name = "message"
            type = "string"

            [[steps]]
            id = "first"
            server = "fixture"
            tool = "echo"
            arguments = { message = "${input.message}" }

            [[steps]]
            id = "second"
            server = "fixture"
            tool = "echo"
            arguments = { message = "Again: ${steps.first.output.structuredContent.message}" }
        "#,
    )
    .expect("chain skill should be written");
    fs::write(
        fixture.config_dir.join("fail.skill.toml"),
        r#"
            id = "fail-fast"
            name = "Fail fast"
            description = "Stop after a tool-level error"

            [[steps]]
            id = "fail"
            server = "fixture"
            tool = "fail"

            [[steps]]
            id = "never"
            server = "fixture"
            tool = "echo"
            arguments = { message = "not-called" }
        "#,
    )
    .expect("fail-fast skill should be written");
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    assert_eq!(manager.list_skills().await.len(), 2);
    let chained = manager
        .run_skill("echo-chain", json!({"message": "hello"}))
        .await
        .expect("chain should run");
    assert_eq!(chained.status, SkillRunStatus::Ok);
    assert_eq!(chained.steps_completed, 2);
    assert_eq!(
        chained.results[1].result.value()["structuredContent"]["message"],
        "Again: hello"
    );

    let failed = manager
        .run_skill("fail-fast", json!({}))
        .await
        .expect("tool-level errors should produce a structured skill result");
    assert_eq!(failed.status, SkillRunStatus::Error);
    assert_eq!(failed.results.len(), 1);
    assert_eq!(failed.steps_completed, 0);
    let failure = failed.failure.expect("failure metadata should exist");
    assert_eq!(failure.step_index, 0);
    assert_eq!(failure.error.code.as_str(), "SKILL_UPSTREAM_ERROR");
    manager.shutdown().await.expect("fixture should stop");
}

#[tokio::test]
async fn runtime_skill_rechecks_call_policy_for_every_step() {
    let fixture = FixtureRuntime::new();
    fs::write(
        fixture.config_dir.join("policy.skill.toml"),
        r#"
            id = "policy-stop"
            name = "Policy stop"
            description = "Stop when a step is denied"

            [[steps]]
            id = "allowed"
            server = "fixture"
            tool = "add"
            arguments = { a = 2, b = 3 }

            [[steps]]
            id = "denied"
            server = "fixture"
            tool = "echo"
            arguments = { message = "blocked" }

            [[steps]]
            id = "never"
            server = "fixture"
            tool = "fail"
        "#,
    )
    .expect("policy skill should be written");
    fs::write(
        fixture.config_dir.join("policy.toml"),
        r#"
            [[rules]]
            id = "deny-echo"
            action = "call"
            effect = "deny"
            server = "fixture"
            tool = "echo"
        "#,
    )
    .expect("policy should be written");
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");

    let result = manager
        .run_skill("policy-stop", json!({}))
        .await
        .expect("step denial should be embedded with partial results");
    assert_eq!(result.status, SkillRunStatus::Error);
    assert_eq!(result.steps_completed, 1);
    assert_eq!(result.results.len(), 1);
    let failure = result.failure.expect("failure metadata should exist");
    assert_eq!(failure.step_index, 1);
    assert_eq!(failure.error.code.as_str(), "POLICY_DENIED");
    manager.shutdown().await.expect("fixture should stop");
}

#[tokio::test]
async fn running_skill_finishes_its_snapshot_during_catalog_reload() {
    let fixture = FixtureRuntime::new();
    fs::write(
        fixture.config_dir.join("snapshot.skill.toml"),
        r#"
            id = "snapshot"
            name = "Snapshot"
            description = "Complete an immutable running definition"

            [[steps]]
            id = "wait"
            server = "fixture"
            tool = "sleep"
            arguments = { milliseconds = 200 }

            [[steps]]
            id = "echo"
            server = "fixture"
            tool = "echo"
            arguments = { message = "completed" }
        "#,
    )
    .expect("snapshot skill should be written");
    let manager = fixture.manager();
    manager
        .connect_server("fixture", None)
        .await
        .expect("fixture should connect");
    let running = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.run_skill("snapshot", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let reload = manager
        .reload_configuration(
            manager.registry().await,
            Policy::default(),
            SkillCatalog::default(),
        )
        .await;
    assert!(reload.skills_changed);
    assert!(manager.list_skills().await.is_empty());
    let result = running
        .await
        .expect("skill task should join")
        .expect("running skill should finish");
    assert_eq!(result.status, SkillRunStatus::Ok);
    assert_eq!(result.steps_completed, 2);
    manager.shutdown().await.expect("fixture should stop");
}

struct FixtureRuntime {
    _root: TempDir,
    root: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    startup_counter: std::path::PathBuf,
    pid_file: std::path::PathBuf,
}

impl FixtureRuntime {
    fn new() -> Self {
        Self::new_with_initialize_delay(0)
    }

    fn new_with_initialize_delay(initialize_delay_ms: u64) -> Self {
        Self::new_with_options(initialize_delay_ms, false)
    }

    fn new_with_reconnect() -> Self {
        Self::new_with_options(0, true)
    }

    fn new_with_options(initialize_delay_ms: u64, reconnect: bool) -> Self {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let config_dir = root.path().join("config");
        fs::create_dir(&config_dir).expect("config directory should be created");
        let startup_counter = root.path().join("starts.txt");
        let pid_file = root.path().join("pid.txt");
        let fixture_binary = env!("CARGO_BIN_EXE_mcp-host-fixture-server");
        let reconnect = if reconnect {
            "[reconnect]\nenabled = true\nmax_retries = 5\ninitial_backoff_ms = 20\nmax_backoff_ms = 20\njitter = false\n"
        } else {
            ""
        };
        let manifest = format!(
            "id = \"fixture\"\nname = \"Fixture\"\ndescription = \"Real RMCP fixture\"\n{reconnect}[transport]\ntype = \"stdio\"\ncommand = {fixture_binary:?}\nargs = [\"--startup-counter-file\", {startup_counter:?}, \"--pid-file\", {pid_file:?}, \"--initialize-delay-ms\", \"{initialize_delay_ms}\"]\n"
        );
        fs::write(config_dir.join("fixture.toml"), manifest)
            .expect("fixture manifest should be written");
        let root_path = root.path().to_path_buf();
        Self {
            _root: root,
            root: root_path,
            config_dir,
            startup_counter,
            pid_file,
        }
    }

    fn manager(&self) -> Arc<RuntimeManager> {
        self.manager_with_usage(None)
    }

    fn manager_with_usage(&self, usage_root: Option<&std::path::Path>) -> Arc<RuntimeManager> {
        let loaded = ManifestLoader::new(ProcessEnvironment)
            .load_directory(&self.config_dir)
            .expect("fixture manifest should load");
        let registry = RegistryBuilder::build(loaded).expect("registry should build");
        let policy = Policy::load_optional(&self.config_dir).expect("policy should load");
        let skills = SkillCatalog::load_directory(&self.config_dir).expect("skills should load");
        RuntimeManager::new_with_configuration(
            Arc::new(registry),
            RuntimeSettings {
                usage_root: usage_root.map(std::path::Path::to_path_buf),
                ..RuntimeSettings::default()
            },
            policy,
            skills,
        )
    }

    fn startup_count(&self) -> u64 {
        fs::read_to_string(&self.startup_counter)
            .expect("startup counter should exist")
            .trim()
            .parse()
            .expect("startup counter should be numeric")
    }

    async fn wait_startup_count(&self, expected: u64) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fs::read_to_string(&self.startup_counter)
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .is_some_and(|count| count >= expected)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fixture should restart before timeout");
    }

    fn pid(&self) -> u32 {
        fs::read_to_string(&self.pid_file)
            .expect("PID file should exist")
            .trim()
            .parse()
            .expect("PID should be numeric")
    }
}

fn batch_call(
    tool_name: &str,
    arguments: serde_json::Value,
    timeout_ms: Option<u64>,
) -> BatchToolCall {
    batch_call_for("fixture", tool_name, arguments, timeout_ms)
}

fn batch_call_for(
    server_id: &str,
    tool_name: &str,
    arguments: serde_json::Value,
    timeout_ms: Option<u64>,
) -> BatchToolCall {
    BatchToolCall {
        server_id: server_id.to_owned(),
        tool_name: tool_name.to_owned(),
        arguments,
        timeout_ms,
        call_policy: CallPolicy::default(),
    }
}

async fn wait_for_state(manager: &RuntimeManager, expected: LifecycleState) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let inspection = manager
                .inspect_server("fixture")
                .await
                .expect("fixture should be inspectable");
            if inspection.observed_state == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime state should converge before timeout");
}

async fn wait_for_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while fs::read_to_string(path).map_or(true, |contents| contents.trim().is_empty()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture file should appear before timeout");
}

async fn wait_for_process_exit(pid: u32) {
    #[cfg(target_os = "linux")]
    tokio::time::timeout(Duration::from_secs(3), async move {
        let process = std::path::PathBuf::from(format!("/proc/{pid}"));
        while process.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture process should exit and be reaped");

    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[tokio::test]
async fn usage_memory_persists_across_manager_restarts() {
    let fixture = FixtureRuntime::new();
    let usage_root = fixture.root.join("usage");
    let manager = fixture.manager_with_usage(Some(&usage_root));
    manager
        .connect_server("fixture", Some("/tmp/project-a"))
        .await
        .expect("fixture should connect");
    manager
        .call_tool(
            "fixture",
            "echo",
            json!({"message": "one"}),
            None,
            CallPolicy::default(),
        )
        .await
        .expect("echo should succeed");
    manager
        .call_tool("fixture", "missing", json!({}), None, CallPolicy::default())
        .await
        .expect_err("missing tool should fail");
    std::mem::forget(manager);

    let restarted = fixture.manager_with_usage(Some(&usage_root));
    let servers = restarted.list_servers().await;
    let fixture_summary = servers
        .iter()
        .find(|summary| summary.id == "fixture")
        .expect("fixture should be listed");
    assert_eq!(fixture_summary.use_count, Some(2));
    assert_eq!(fixture_summary.error_count, Some(1));
    assert!(
        fixture_summary
            .projects
            .contains(&"/tmp/project-a".to_owned())
    );
    assert!(fixture_summary.last_used_at_unix_ms.is_some());
}
