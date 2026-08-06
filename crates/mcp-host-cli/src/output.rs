use mcp_host_core::{HostStatus, LifecycleState, ServerSummary, SkillSummary, ToolSnapshot};
use serde_json::Value;

pub fn render_human(command: &str, value: &Value) -> Option<String> {
    match command {
        "list" => render_server_table(value),
        "tools" => render_tool_table(value),
        "status" => Some(render_status(value)),
        "skill-list" => render_skill_table(value),
        "connect" => Some(render_connect(value)),
        "disconnect" => Some(render_disconnect(value)),
        _ => None,
    }
}

fn render_server_table(value: &Value) -> Option<String> {
    let Ok(servers) = serde_json::from_value::<Vec<ServerSummary>>(value.clone()) else {
        return None;
    };
    let rows: Vec<[String; 5]> = servers
        .iter()
        .map(|server| {
            [
                server.id.clone(),
                server.name.clone(),
                state_label(server.observed_state, server.desired_state),
                server.tool_count.to_string(),
                transport_label(&server.transport),
            ]
        })
        .collect();
    Some(table(&["ID", "NAME", "STATE", "TOOLS", "TRANSPORT"], &rows))
}

fn render_tool_table(value: &Value) -> Option<String> {
    let Ok(snapshot) = serde_json::from_value::<ToolSnapshot>(value.clone()) else {
        return None;
    };
    let mut rows: Vec<[String; 2]> = snapshot
        .tools
        .iter()
        .map(|tool| {
            [
                tool.name.clone(),
                tool.description
                    .clone()
                    .unwrap_or_default()
                    .replace('\n', " "),
            ]
        })
        .collect();
    rows.sort_by(|left, right| left[0].cmp(&right[0]));
    let mut body = table(&["TOOL", "DESCRIPTION"], &rows);
    if snapshot.stale {
        body.push_str(&format!(
            "\nNote: this snapshot is stale (fetched at {})",
            snapshot.fetched_at_unix_ms
        ));
    }
    Some(body)
}

fn render_status(value: &Value) -> String {
    let (daemon_version, protocol_version, registry_count, connected_count, failed_count) =
        if let Ok(status) = serde_json::from_value::<HostStatus>(value.clone()) {
            (
                status.daemon_version.clone(),
                status.protocol_version.to_string(),
                status.registry_server_count.to_string(),
                status.connected_count.to_string(),
                status.failed_count.to_string(),
            )
        } else {
            (
                value
                    .get("daemon_version")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                value
                    .get("protocol_version")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string(),
                value
                    .get("registry_server_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string(),
                value
                    .get("connected_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string(),
                value
                    .get("failed_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string(),
            )
        };
    let uptime_ms = value.get("uptime_ms").and_then(Value::as_u64).unwrap_or(0);
    let active_sessions = value
        .get("active_downstream_mcp_sessions")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .to_string();
    let control_ready = value
        .get("control_endpoint_ready")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mcp_ready = value
        .get("mcp_endpoint_ready")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut lines = Vec::new();
    lines.push(("Daemon version".to_owned(), daemon_version));
    lines.push((
        "Control protocol".to_owned(),
        format!("v{protocol_version}"),
    ));
    lines.push(("Uptime".to_owned(), format_duration(uptime_ms)));
    lines.push((
        "Servers".to_owned(),
        format!("{registry_count} registered, {connected_count} connected, {failed_count} failed"),
    ));
    lines.push(("Downstream MCP sessions".to_owned(), active_sessions));
    lines.push((
        "Control endpoint".to_owned(),
        if control_ready {
            "ready".to_owned()
        } else {
            "not ready".to_owned()
        },
    ));
    lines.push((
        "MCP endpoint".to_owned(),
        if mcp_ready {
            "ready".to_owned()
        } else {
            "not ready".to_owned()
        },
    ));
    key_values(&lines)
}

/// Renders a status block followed by the durable per-server usage table.
pub fn render_status_stats(value: &Value) -> Option<String> {
    let servers =
        serde_json::from_value::<Vec<ServerSummary>>(value.get("servers")?.clone()).ok()?;
    let rows: Vec<[String; 5]> = servers
        .iter()
        .map(|server| {
            [
                server.id.clone(),
                server
                    .use_count
                    .map_or_else(|| "-".to_owned(), |count| count.to_string()),
                server
                    .error_count
                    .map_or_else(|| "-".to_owned(), |count| count.to_string()),
                server
                    .last_used_at_unix_ms
                    .map_or_else(|| "never".to_owned(), format_unix_ms),
                if server.projects.is_empty() {
                    "-".to_owned()
                } else {
                    server.projects.join(", ")
                },
            ]
        })
        .collect();
    let mut body = render_status(value.get("status")?);
    body.push('\n');
    body.push_str("Usage:\n");
    body.push_str(&table(
        &["SERVER", "CALLS", "ERRORS", "LAST USED", "PROJECTS"],
        &rows,
    ));
    Some(body)
}

fn format_unix_ms(timestamp: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        });
    let elapsed = now.saturating_sub(timestamp);
    let seconds = elapsed / 1_000;
    let days = seconds / 86_400;
    if days > 0 {
        return format!("{days}d ago");
    }
    let hours = seconds / 3_600;
    if hours > 0 {
        return format!("{hours}h ago");
    }
    let minutes = seconds / 60;
    if minutes > 0 {
        return format!("{minutes}m ago");
    }
    format!("{seconds}s ago")
}

fn render_skill_table(value: &Value) -> Option<String> {
    let Ok(skills) = serde_json::from_value::<Vec<SkillSummary>>(value.clone()) else {
        return None;
    };
    let rows: Vec<[String; 3]> = skills
        .iter()
        .map(|skill| {
            [
                skill.id.clone(),
                skill.name.clone(),
                skill.step_count.to_string(),
            ]
        })
        .collect();
    Some(table(&["ID", "NAME", "STEPS"], &rows))
}

fn render_connect(value: &Value) -> String {
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let server_id = value
        .get("server_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    match state {
        "connected" => {
            let tools = value.get("tool_count").and_then(Value::as_u64).unwrap_or(0);
            let protocol = value
                .get("protocol_version")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("{server_id}: connected ({tools} tools, MCP {protocol})")
        }
        other => format!("{server_id}: {other}"),
    }
}

fn render_disconnect(value: &Value) -> String {
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let server_id = value
        .get("server_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    format!("{server_id}: {state}")
}

fn state_label(observed: LifecycleState, desired: mcp_host_core::DesiredConnection) -> String {
    let label = match observed {
        LifecycleState::Registered => "registered",
        LifecycleState::Starting => "starting",
        LifecycleState::Initializing => "initializing",
        LifecycleState::Connected => "connected",
        LifecycleState::Disconnected => "disconnected",
        LifecycleState::Stopped => "stopped",
        LifecycleState::Failed => "failed",
    };
    let desired = match desired {
        mcp_host_core::DesiredConnection::Connected => "desired",
        mcp_host_core::DesiredConnection::Disconnected => "idle",
    };
    if label == "disconnected" && desired == "desired" {
        "disconnected (retry desired)".to_owned()
    } else if label == "connected" {
        "connected".to_owned()
    } else if desired == "desired" {
        format!("{label} (desired)")
    } else {
        label.to_owned()
    }
}

fn transport_label(transport: &mcp_host_core::TransportKind) -> String {
    match transport {
        mcp_host_core::TransportKind::Stdio => "stdio",
        mcp_host_core::TransportKind::Http => "http",
    }
    .to_owned()
}

fn format_duration(millis: u64) -> String {
    let seconds = millis / 1000;
    let (days, seconds) = (seconds / 86_400, seconds % 86_400);
    let (hours, seconds) = (seconds / 3_600, seconds % 3_600);
    let (minutes, seconds) = (seconds / 60, seconds % 60);
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn key_values(lines: &[(String, String)]) -> String {
    let width = lines
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    lines
        .iter()
        .map(|(key, value)| format!("{key:<width$}  {value}", width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) -> String {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (column, value) in row.iter().enumerate() {
            widths[column] = widths[column].max(value.chars().count());
        }
    }
    let mut lines = vec![format_row(headers, &widths)];
    lines.push("-".repeat(format_row(headers, &widths).chars().count()));
    for row in rows {
        lines.push(format_row(
            &row.iter().map(String::as_str).collect::<Vec<_>>(),
            &widths,
        ));
    }
    lines.join("\n")
}

fn format_row(cells: &[&str], widths: &[usize]) -> String {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!("{cell:<width$}  ", width = width))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{render_human, render_status_stats};

    #[test]
    fn status_stats_renders_usage_columns_and_relative_times() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let value = json!({
            "status": {
                "daemon_version": "0.2.0",
                "protocol_version": 1,
                "registry_server_count": 1,
                "connected_count": 1,
                "failed_count": 0,
                "uptime_ms": 1000,
                "active_downstream_mcp_sessions": 0,
                "control_endpoint_ready": true,
                "mcp_endpoint_ready": true,
                "shutting_down": false,
                "tool_calls_total": 3
            },
            "servers": [
                {
                    "id": "fixture",
                    "name": "Fixture",
                    "description": "d",
                    "enabled": true,
                    "transport": "stdio",
                    "desired_state": "connected",
                    "observed_state": "connected",
                    "tool_count": 5,
                    "tools_stale": false,
                    "use_count": 3,
                    "error_count": 1,
                    "last_used_at_unix_ms": now - 30_000,
                    "projects": ["/home/me/project"]
                }
            ]
        });

        let rendered = render_status_stats(&value).expect("stats should render");
        assert!(rendered.contains("fixture"));
        assert!(rendered.contains("3"));
        assert!(rendered.contains("30s ago"));
        assert!(rendered.contains("/home/me/project"));
    }

    #[test]
    fn list_renders_a_server_table() {
        let value = json!([
            {
                "id": "fixture",
                "name": "Demo Fixture",
                "description": "offline demo",
                "enabled": true,
                "transport": "stdio",
                "desired_state": "connected",
                "observed_state": "connected",
                "tool_count": 5,
                "tools_stale": false,
            }
        ]);
        let rendered = render_human("list", &value).expect("human output");
        assert!(rendered.contains("ID"));
        assert!(rendered.contains("fixture"));
        assert!(rendered.contains("Demo Fixture"));
        assert!(rendered.contains("connected"));
        assert!(rendered.contains("5"));
    }

    #[test]
    fn tools_renders_sorted_name_and_description() {
        let value = json!({
            "server_id": "fixture",
            "fetched_at_unix_ms": 1,
            "tool_count": 2,
            "stale": false,
            "tools": [
                {"name": "sleep", "description": "Sleep for a while\nwith a newline", "input_schema": {"type": "object"}},
                {"name": "echo", "description": "Return the message", "title": null, "input_schema": {"type": "object"}},
            ],
        });
        let rendered = render_human("tools", &value).expect("human output");
        let echo = rendered.find("echo").expect("echo present");
        let sleep = rendered.find("sleep").expect("sleep present");
        assert!(echo < sleep, "tools are sorted");
        assert!(rendered.contains("Return the message"));
        assert!(rendered.contains("Sleep for a while with a newline"));
    }

    #[test]
    fn status_renders_key_values() {
        let value = json!({
            "daemon_version": "0.2.0",
            "protocol_version": 1,
            "started_at_unix_ms": 0,
            "uptime_ms": 65_000,
            "registry_server_count": 1,
            "connected_count": 1,
            "failed_count": 0,
            "active_downstream_mcp_sessions": 1,
            "control_endpoint_ready": true,
            "mcp_endpoint_ready": true,
            "shutting_down": false,
        });
        let rendered = render_human("status", &value).expect("human output");
        assert!(rendered.contains("Daemon version"));
        assert!(rendered.contains("0.2.0"));
        assert!(rendered.contains("1m 5s"));
        assert!(rendered.contains("1 registered, 1 connected, 0 failed"));
    }

    #[test]
    fn connect_renders_a_friendly_line() {
        let value = json!({
            "server_id": "fixture",
            "state": "connected",
            "tool_count": 5,
            "protocol_version": "2025-11-25",
            "connected_at_unix_ms": 0,
            "tool_snapshot": null,
        });
        assert_eq!(
            render_human("connect", &value).expect("human output"),
            "fixture: connected (5 tools, MCP 2025-11-25)"
        );
    }

    #[test]
    fn unknown_commands_fall_back_to_json() {
        assert!(render_human("inspect", &json!({"server_id": "x"})).is_none());
    }

    #[test]
    fn malformed_responses_fall_back_to_json_instead_of_blank_output() {
        assert!(render_human("tools", &json!({"unexpected": true})).is_none());
        assert!(render_human("list", &json!({"unexpected": true})).is_none());
        assert!(render_human("skill-list", &json!({"unexpected": true})).is_none());
    }

    #[test]
    fn table_and_key_values_do_not_emit_a_trailing_newline() {
        let value = json!([
            {
                "id": "fixture",
                "name": "Demo",
                "description": "",
                "enabled": true,
                "transport": "stdio",
                "desired_state": "disconnected",
                "observed_state": "registered",
                "tool_count": 0,
                "tools_stale": false,
            }
        ]);
        let rendered = render_human("list", &value).expect("human output");
        assert!(!rendered.ends_with('\n'));
    }

    #[test]
    fn wide_utf8_names_keep_columns_aligned() {
        let value = json!([
            {
                "id": "üni-cödé",
                "name": "Ünïcodé",
                "description": "",
                "enabled": true,
                "transport": "stdio",
                "desired_state": "disconnected",
                "observed_state": "registered",
                "tool_count": 0,
                "tools_stale": false,
            }
        ]);
        let rendered = render_human("list", &value).expect("human output");
        assert!(rendered.contains("üni-cödé"));
    }
}
