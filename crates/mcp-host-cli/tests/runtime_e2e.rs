use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use mcp_host_core::{
    BatchToolCall, BatchToolCallOutcome, LifecycleState, MAX_BATCH_CALLS, ManifestLoader,
    ProcessEnvironment, RegistryBuilder,
};
use mcp_host_mcp::{RuntimeManager, RuntimeSettings};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn real_stdio_initialize_discover_call_timeout_and_disconnect() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();

    let connected = manager
        .connect_server("fixture")
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
        .call_tool("fixture", "echo", json!({"message": "hello"}), None)
        .await
        .expect("echo should succeed");
    assert_eq!(echo.value()["structuredContent"]["message"], "hello");

    let add = manager
        .call_tool("fixture", "add", json!({"a": 2, "b": 3}), None)
        .await
        .expect("add should succeed");
    assert_eq!(add.value()["structuredContent"]["sum"], 5);

    let failure = manager
        .call_tool("fixture", "fail", json!({}), None)
        .await
        .expect("tool-level failure remains a valid MCP result");
    assert_eq!(failure.value()["isError"], true);

    let timeout = manager
        .call_tool("fixture", "sleep", json!({"milliseconds": 1_000}), Some(30))
        .await
        .expect_err("sleep should time out");
    assert_eq!(timeout.code.as_str(), "TOOL_CALL_TIMEOUT");

    let after_timeout = manager
        .call_tool("fixture", "echo", json!({"message": "still-alive"}), None)
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
async fn batch_calls_run_concurrently_preserve_order_and_isolate_errors() {
    let fixture = FixtureRuntime::new();
    let manager = fixture.manager();
    manager
        .connect_server("fixture")
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
        async move { manager.connect_server("fixture").await }
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
        .connect_server("fixture")
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
        manager.connect_server("fixture"),
        manager.connect_server("second")
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
        tokio::spawn(async move { manager.connect_server("fixture").await })
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
        tokio::spawn(async move { manager.connect_server("fixture").await })
    };
    wait_for_file(&fixture.pid_file).await;
    let joiners = (0..10)
        .map(|_| {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.connect_server("fixture").await })
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
        .connect_server("fixture")
        .await
        .expect("fixture should connect");

    let crash = manager
        .call_tool("fixture", "crash", json!({}), None)
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
        .connect_server("fixture")
        .await
        .expect("failed fixture should reconnect");
    assert_eq!(fixture.startup_count(), 2);
    let echo = manager
        .call_tool("fixture", "echo", json!({"message": "reconnected"}), None)
        .await
        .expect("reconnected fixture should answer");
    assert_eq!(echo.value()["structuredContent"]["message"], "reconnected");
    manager
        .disconnect_server("fixture")
        .await
        .expect("fixture should disconnect");
}

struct FixtureRuntime {
    _root: TempDir,
    config_dir: std::path::PathBuf,
    startup_counter: std::path::PathBuf,
    pid_file: std::path::PathBuf,
}

impl FixtureRuntime {
    fn new() -> Self {
        Self::new_with_initialize_delay(0)
    }

    fn new_with_initialize_delay(initialize_delay_ms: u64) -> Self {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let config_dir = root.path().join("config");
        fs::create_dir(&config_dir).expect("config directory should be created");
        let startup_counter = root.path().join("starts.txt");
        let pid_file = root.path().join("pid.txt");
        let fixture_binary = env!("CARGO_BIN_EXE_mcp-host-fixture-server");
        let manifest = format!(
            "id = \"fixture\"\nname = \"Fixture\"\ndescription = \"Real RMCP fixture\"\n[transport]\ntype = \"stdio\"\ncommand = {fixture_binary:?}\nargs = [\"--startup-counter-file\", {startup_counter:?}, \"--pid-file\", {pid_file:?}, \"--initialize-delay-ms\", \"{initialize_delay_ms}\"]\n"
        );
        fs::write(config_dir.join("fixture.toml"), manifest)
            .expect("fixture manifest should be written");
        Self {
            _root: root,
            config_dir,
            startup_counter,
            pid_file,
        }
    }

    fn manager(&self) -> Arc<RuntimeManager> {
        let loaded = ManifestLoader::new(ProcessEnvironment)
            .load_directory(&self.config_dir)
            .expect("fixture manifest should load");
        let registry = RegistryBuilder::build(loaded).expect("registry should build");
        RuntimeManager::new(Arc::new(registry), RuntimeSettings::default())
    }

    fn startup_count(&self) -> u64 {
        fs::read_to_string(&self.startup_counter)
            .expect("startup counter should exist")
            .trim()
            .parse()
            .expect("startup counter should be numeric")
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
