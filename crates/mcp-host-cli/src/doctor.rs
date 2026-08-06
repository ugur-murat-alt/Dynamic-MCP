use std::path::{Path, PathBuf};

use mcp_host_core::{ControlRequest, HostStatus};
use serde_json::{Value, json};

use crate::commands::{DEFAULT_CONTROL_TIMEOUT, resolve_runtime_dir};
use crate::ipc::send_control;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckLevel {
    Ok,
    Warn,
    Error,
}

#[derive(Debug)]
pub struct Check {
    pub label: String,
    pub level: CheckLevel,
    pub detail: String,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    pub daemon: Option<HostStatus>,
    pub servers: Vec<Value>,
    pub skills: Vec<Value>,
    pub opencode: Option<Value>,
    pub claude: Option<Value>,
}

impl DoctorReport {
    pub fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.level == CheckLevel::Error)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "healthy": !self.has_errors(),
            "checks": self.checks.iter().map(|check| json!({
                "label": check.label,
                "level": match check.level {
                    CheckLevel::Ok => "ok",
                    CheckLevel::Warn => "warn",
                    CheckLevel::Error => "error",
                },
                "detail": check.detail,
            })).collect::<Vec<_>>(),
            "daemon": self.daemon,
            "servers": self.servers,
            "skills": self.skills,
            "harness": {
                "opencode": self.opencode,
                "claude": self.claude,
            },
        })
    }

    pub fn print(&self) {
        for check in &self.checks {
            let marker = match check.level {
                CheckLevel::Ok => "[ok]   ",
                CheckLevel::Warn => "[warn] ",
                CheckLevel::Error => "[error]",
            };
            println!("{marker} {}: {}", check.label, check.detail);
        }
        println!(
            "\n{}",
            if self.has_errors() {
                "issues found: fix them before relying on the host"
            } else {
                "all checks passed"
            }
        );
    }
}

pub async fn run_doctor(
    runtime_dir: &Path,
    timeout: Option<u64>,
    json_output: bool,
) -> DoctorReport {
    let mut report = DoctorReport {
        checks: Vec::new(),
        daemon: None,
        servers: Vec::new(),
        skills: Vec::new(),
        opencode: None,
        claude: None,
    };
    let control_timeout = timeout.map_or(DEFAULT_CONTROL_TIMEOUT, std::time::Duration::from_millis);

    check_binary(&mut report);
    check_path(&mut report);
    check_runtime_directory(runtime_dir, &mut report);

    match send_control(runtime_dir, &ControlRequest::Status, control_timeout).await {
        Ok(value) => {
            if let Ok(status) = serde_json::from_value::<HostStatus>(value.clone()) {
                report.daemon = Some(status.clone());
                report.checks.push(Check {
                    label: "daemon".to_owned(),
                    level: CheckLevel::Ok,
                    detail: format!(
                        "v{} running, protocol v{}, {} servers",
                        status.daemon_version,
                        status.protocol_version,
                        status.registry_server_count
                    ),
                });
            } else {
                report.checks.push(Check {
                    label: "daemon".to_owned(),
                    level: CheckLevel::Error,
                    detail: "the daemon returned an invalid status response".to_owned(),
                });
            }
            collect_daemon_views(&mut report, runtime_dir, control_timeout).await;
        }
        Err(error) => report.checks.push(Check {
            label: "daemon".to_owned(),
            level: CheckLevel::Error,
            detail: format!("not reachable: {error}"),
        }),
    }

    if let Some(status) = &report.daemon {
        let binary_version = env!("CARGO_PKG_VERSION");
        if status.daemon_version == binary_version {
            report.checks.push(Check {
                label: "version".to_owned(),
                level: CheckLevel::Ok,
                detail: format!("binary and daemon both run v{binary_version}"),
            });
        } else {
            report.checks.push(Check {
                label: "version".to_owned(),
                level: CheckLevel::Error,
                detail: format!(
                    "binary is v{binary_version} but the daemon runs v{}; restart the daemon",
                    status.daemon_version
                ),
            });
        }
    }

    check_harness(&mut report);

    if json_output {
        println!("{}", report.to_json());
    }
    report
}

fn check_binary(report: &mut DoctorReport) {
    let binary = match std::env::current_exe() {
        Ok(binary) => binary,
        Err(error) => {
            report.checks.push(Check {
                label: "binary".to_owned(),
                level: CheckLevel::Error,
                detail: format!("cannot resolve the current executable: {error}"),
            });
            return;
        }
    };
    report.checks.push(Check {
        label: "binary".to_owned(),
        level: CheckLevel::Ok,
        detail: format!(
            "mcp-host {} at {}",
            env!("CARGO_PKG_VERSION"),
            binary.display()
        ),
    });
}

fn check_path(report: &mut DoctorReport) {
    let current = std::env::current_exe().ok();
    let in_path = find_on_path("mcp-host");
    match (current, in_path) {
        (Some(current), Some(found)) if same_file(&current, &found) => {
            report.checks.push(Check {
                label: "PATH".to_owned(),
                level: CheckLevel::Ok,
                detail: format!("`mcp-host` resolves to {}", found.display()),
            });
        }
        (_, Some(found)) => report.checks.push(Check {
            label: "PATH".to_owned(),
            level: CheckLevel::Warn,
            detail: format!(
                "`mcp-host` on PATH ({}) differs from the running binary; harness installs may use a stale executable",
                found.display()
            ),
        }),
        (_, None) => report.checks.push(Check {
            label: "PATH".to_owned(),
            level: CheckLevel::Warn,
            detail: "`mcp-host` was not found on PATH; add its install directory".to_owned(),
        }),
    }
}

fn check_runtime_directory(runtime_dir: &Path, report: &mut DoctorReport) {
    let resolved = resolve_runtime_dir(Some(runtime_dir.to_path_buf()));
    let dir = resolved.as_deref().unwrap_or(runtime_dir);
    if dir.exists() {
        report.checks.push(Check {
            label: "runtime directory".to_owned(),
            level: CheckLevel::Ok,
            detail: format!("{} exists", dir.display()),
        });
    } else {
        report.checks.push(Check {
            label: "runtime directory".to_owned(),
            level: CheckLevel::Warn,
            detail: format!(
                "{} does not exist yet; the daemon creates it on start",
                dir.display()
            ),
        });
    }
}

async fn collect_daemon_views(
    report: &mut DoctorReport,
    runtime_dir: &Path,
    timeout: std::time::Duration,
) {
    if let Ok(value) = send_control(runtime_dir, &ControlRequest::ListServers, timeout).await {
        report.servers = value.as_array().cloned().unwrap_or_default();
    }
    if let Ok(value) = send_control(runtime_dir, &ControlRequest::SkillList, timeout).await {
        report.skills = value.as_array().cloned().unwrap_or_default();
    }
}

fn check_harness(report: &mut DoctorReport) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let opencode_dir: Option<PathBuf> = crate::harness_paths::opencode_config_directory()
        .ok()
        .or_else(|| xdg.clone().map(|dir| dir.join("opencode")))
        .or_else(|| home.as_ref().map(|home| home.join(".config/opencode")));

    let skill_path = opencode_dir
        .as_ref()
        .map(|dir| dir.join("skills/dynamic-mcp/SKILL.md"));
    let skill_ok = skill_path.as_deref().is_some_and(Path::exists);
    report.opencode = Some(json!({
        "config_directory": opencode_dir.as_ref().map(|dir| dir.display().to_string()),
        "skill_installed": skill_ok,
    }));

    let claude_skill = home.map(|home| home.join(".claude/skills/dynamic-mcp/SKILL.md"));
    let claude_skill_ok = claude_skill.as_deref().is_some_and(Path::exists);
    report.claude = Some(json!({
        "skill_installed": claude_skill_ok,
    }));

    let checked = [
        ("opencode skill", skill_ok),
        ("claude skill", claude_skill_ok),
    ];
    let missing: Vec<&str> = checked
        .iter()
        .filter_map(|(name, installed)| (!installed).then_some(*name))
        .collect();
    if missing.is_empty() {
        report.checks.push(Check {
            label: "harness skills".to_owned(),
            level: CheckLevel::Ok,
            detail: "dynamic-mcp skill installed for opencode and claude".to_owned(),
        });
    } else {
        report.checks.push(Check {
            label: "harness skills".to_owned(),
            level: CheckLevel::Warn,
            detail: format!(
                "missing: {} (run `mcp-host harness install all`)",
                missing.join(", ")
            ),
        });
    }
}

fn find_on_path(executable: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(executable);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_extension = directory.join(format!("{executable}.exe"));
            if with_extension.is_file() {
                return Some(with_extension);
            }
        }
    }
    None
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
