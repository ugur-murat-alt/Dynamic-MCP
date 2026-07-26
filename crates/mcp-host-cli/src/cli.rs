use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

/// Command-line interface for the MCP host.
#[derive(Debug, Parser)]
#[command(
    name = "mcp-host",
    version,
    propagate_version = true,
    arg_required_else_help = true,
    about = "Run and control a shared Dynamic MCP Host",
    long_about = "Run and control a shared Dynamic MCP Host.\n\nThe daemon owns upstream MCP sessions. Ordinary CLI commands use the control endpoint, while `mcp-host mcp` is a transparent stdio bridge for AI clients. Connections to configured upstream servers are always explicit.",
    after_long_help = "Examples:\n  mcp-host daemon run --config-dir ./config\n  mcp-host connect filesystem\n  mcp-host call filesystem read_file --arguments '{\"path\":\"README.md\"}'\n  mcp-host batch --calls '[{\"server_id\":\"filesystem\",\"tool_name\":\"read_file\",\"arguments\":{\"path\":\"README.md\"}}]'\n  mcp-host harness install opencode\n  mcp-host harness install claude-code --scope user\n\nRun `mcp-host <command> --help` for command-specific details."
)]
pub struct Cli {
    /// Directory containing daemon lock, metadata, and IPC endpoints.
    #[arg(long, global = true, value_name = "DIR")]
    pub runtime_dir: Option<PathBuf>,

    /// Emit compact machine-readable JSON instead of pretty JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Maximum control or tool request duration in milliseconds.
    #[arg(
        long,
        global = true,
        value_name = "MS",
        value_parser = clap::value_parser!(u64).range(1..=300_000)
    )]
    pub timeout: Option<u64>,

    #[command(subcommand)]
    pub command: Command,
}

/// Top-level MCP host commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start, inspect, or stop the foreground daemon.
    Daemon(Daemon),
    /// List every server in the immutable registry.
    List,
    /// Show manifest, lifecycle, and cached-tool details for one server.
    Inspect {
        /// Registry ID of the server to inspect.
        server_id: String,
    },
    /// Establish an upstream MCP session and discover its tools.
    Connect {
        /// Registry ID of the server to connect.
        server_id: String,
    },
    /// Close an upstream MCP session and reap its child process.
    Disconnect {
        /// Registry ID of the server to disconnect.
        server_id: String,
    },
    /// List the cached tools exposed by a connected upstream server.
    Tools {
        /// Registry ID of the connected server.
        server_id: String,
        /// Refresh the complete paginated tool snapshot before printing it.
        #[arg(long)]
        refresh: bool,
    },
    /// Atomically refresh one connected server's tool snapshot.
    Refresh {
        /// Registry ID of the connected server.
        server_id: String,
    },
    /// Invoke one tool on an already connected upstream server.
    Call(Call),
    /// Invoke up to 32 connected upstream tools concurrently.
    #[command(
        long_about = "Invoke between 1 and 32 tools concurrently across connected downstream MCP servers.\n\nResults preserve input order. A runtime error for one item is embedded in that item and does not cancel the remaining calls. Upstream MCP results, including isError, structuredContent, and _meta, are preserved."
    )]
    Batch(Batch),
    /// Show daemon health and aggregate runtime state.
    Status,
    /// Register the stdio bridge in a supported AI coding harness.
    Harness(Harness),
    /// Bridge stdin/stdout to the daemon's raw MCP endpoint.
    #[command(
        long_about = "Bridge stdin/stdout to the daemon's raw MCP endpoint.\n\nThis command is intended to be launched by an MCP client. The daemon must already be running. Stdout is reserved exclusively for MCP protocol bytes; diagnostics are written to stderr."
    )]
    Mcp,
}

/// Commands that manage the local daemon.
#[derive(Debug, Args)]
pub struct Daemon {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

/// Local daemon lifecycle commands.
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground and load manifests from a directory.
    Run(DaemonRun),
    /// Read status from the running daemon.
    Status,
    /// Request an orderly daemon shutdown.
    Stop,
}

/// Arguments used to start the daemon.
#[derive(Debug, Args)]
pub struct DaemonRun {
    /// Directory containing server manifest TOML files.
    #[arg(long, value_name = "DIR")]
    pub config_dir: PathBuf,
}

/// Arguments used to invoke a tool.
#[derive(Debug, Args)]
pub struct Call {
    /// Registry ID of the connected server.
    pub server_id: String,
    /// Upstream tool name returned by tools/list.
    pub tool_name: String,

    /// Tool arguments as an inline JSON object. Defaults to `{}`.
    #[arg(long, value_name = "JSON", conflicts_with = "arguments_file")]
    pub arguments: Option<String>,

    /// Read the tool argument JSON object from a file, or `-` for stdin.
    #[arg(long, value_name = "PATH", conflicts_with = "arguments")]
    pub arguments_file: Option<PathBuf>,
}

/// Arguments used to invoke a bounded batch of tools concurrently.
#[derive(Debug, Args)]
#[command(
    group(ArgGroup::new("batch_input").required(true).args(["calls", "calls_file"])),
    after_long_help = "Examples:\n  mcp-host batch --calls '[{\"server_id\":\"fixture\",\"tool_name\":\"echo\",\"arguments\":{\"text\":\"hello\"}}]'\n  mcp-host batch --calls-file calls.json\n  cat calls.json | mcp-host batch --calls-file -"
)]
pub struct Batch {
    /// Calls as an inline JSON array.
    #[arg(long, value_name = "JSON")]
    pub calls: Option<String>,

    /// Read the calls JSON array from a file, or `-` for stdin.
    #[arg(long, value_name = "PATH")]
    pub calls_file: Option<PathBuf>,
}

/// Commands that integrate the stdio bridge with AI coding harnesses.
#[derive(Debug, Args)]
pub struct Harness {
    #[command(subcommand)]
    pub command: HarnessCommand,
}

/// AI coding harness integration commands.
#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// Register this binary's MCP bridge in one or more harnesses.
    #[command(
        long_about = "Register this binary's MCP bridge in one or more harnesses.\n\nThe command invokes the selected harness's official CLI and stores the canonical absolute path of the current mcp-host executable. OpenCode uses its global configuration. Claude Code defaults to user scope so the host is available in every project. The daemon is not started by this command.",
        after_long_help = "Examples:\n  mcp-host harness install opencode\n  mcp-host harness install claude-code\n  mcp-host harness install claude-code --scope project\n  mcp-host harness install all --name dynamic-mcp\n  mcp-host --runtime-dir /tmp/mcp-host harness install all"
    )]
    Install(HarnessInstall),
}

/// Arguments used to register the bridge with an AI coding harness.
#[derive(Debug, Args)]
pub struct HarnessInstall {
    /// Harness to configure.
    #[arg(value_enum)]
    pub target: HarnessTarget,

    /// MCP server name stored by the harness.
    #[arg(long, default_value = "dynamic-mcp", value_parser = parse_harness_name)]
    pub name: String,

    /// Claude Code configuration scope. Ignored for OpenCode.
    #[arg(long, value_enum, default_value_t = ClaudeScope::User)]
    pub scope: ClaudeScope,
}

/// Supported AI coding harnesses.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum HarnessTarget {
    /// Configure OpenCode's global MCP registry.
    #[value(name = "opencode")]
    OpenCode,
    /// Configure Claude Code at the selected scope.
    ClaudeCode,
    /// Configure OpenCode and Claude Code in sequence.
    All,
}

/// Claude Code MCP configuration scopes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ClaudeScope {
    /// Current project only, private to the current user.
    Local,
    /// Current project via a shareable `.mcp.json` file.
    Project,
    /// Every project for the current user.
    User,
}

impl ClaudeScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

fn parse_harness_name(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "name must be 1-64 ASCII letters, digits, hyphens, underscores, or dots".to_owned(),
        );
    }

    Ok(value.to_owned())
}

/// Process exit statuses exposed by the CLI.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitCode {
    Success = 0,
    Usage = 2,
    DaemonUnavailable = 3,
    RuntimeFailure = 4,
    UpstreamToolError = 5,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory as _, error::ErrorKind};

    fn parse(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(arguments).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn parses_every_command() {
        let commands: &[&[&str]] = &[
            &["mcp-host", "daemon", "run", "--config-dir", "config"],
            &["mcp-host", "daemon", "status"],
            &["mcp-host", "daemon", "stop"],
            &["mcp-host", "list"],
            &["mcp-host", "inspect", "server-a"],
            &["mcp-host", "connect", "server-a"],
            &["mcp-host", "disconnect", "server-a"],
            &["mcp-host", "tools", "server-a", "--refresh"],
            &["mcp-host", "refresh", "server-a"],
            &[
                "mcp-host",
                "call",
                "server-a",
                "tool-a",
                "--arguments",
                "{\"key\":true}",
            ],
            &[
                "mcp-host",
                "batch",
                "--calls",
                "[{\"server_id\":\"server-a\",\"tool_name\":\"tool-a\"}]",
            ],
            &["mcp-host", "status"],
            &["mcp-host", "harness", "install", "opencode"],
            &[
                "mcp-host",
                "harness",
                "install",
                "claude-code",
                "--scope",
                "project",
            ],
            &["mcp-host", "harness", "install", "all"],
            &["mcp-host", "mcp"],
        ];

        for arguments in commands {
            assert!(Cli::try_parse_from(*arguments).is_ok(), "{arguments:?}");
        }
    }

    #[test]
    fn accepts_stdin_as_an_arguments_file() {
        let cli = parse(&[
            "mcp-host",
            "call",
            "server-a",
            "tool-a",
            "--arguments-file",
            "-",
        ]);

        let Command::Call(call) = cli.command else {
            panic!("expected call command");
        };
        assert_eq!(call.arguments_file, Some(PathBuf::from("-")));
    }

    #[test]
    fn rejects_inline_and_file_arguments_together() {
        let result = Cli::try_parse_from([
            "mcp-host",
            "call",
            "server-a",
            "tool-a",
            "--arguments",
            "{}",
            "--arguments-file",
            "arguments.json",
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn requires_exactly_one_batch_input() {
        assert!(Cli::try_parse_from(["mcp-host", "batch"]).is_err());
        assert!(
            Cli::try_parse_from([
                "mcp-host",
                "batch",
                "--calls",
                "[]",
                "--calls-file",
                "calls.json",
            ])
            .is_err()
        );

        let cli = parse(&["mcp-host", "batch", "--calls-file", "-"]);
        let Command::Batch(batch) = cli.command else {
            panic!("expected batch command");
        };
        assert_eq!(batch.calls_file, Some(PathBuf::from("-")));
    }

    #[test]
    fn validates_timeout_bounds() {
        assert!(Cli::try_parse_from(["mcp-host", "--timeout", "1", "list"]).is_ok());
        assert!(Cli::try_parse_from(["mcp-host", "--timeout", "300000", "list"]).is_ok());
        assert!(Cli::try_parse_from(["mcp-host", "--timeout", "0", "list"]).is_err());
        assert!(Cli::try_parse_from(["mcp-host", "--timeout", "300001", "list"]).is_err());
    }

    #[test]
    fn parses_global_options_after_commands() {
        let cli = parse(&[
            "mcp-host",
            "daemon",
            "run",
            "--config-dir",
            "config",
            "--runtime-dir",
            "runtime",
            "--json",
            "--timeout",
            "25",
        ]);

        assert_eq!(cli.runtime_dir, Some(PathBuf::from("runtime")));
        assert!(cli.json);
        assert_eq!(cli.timeout, Some(25));
    }

    #[test]
    fn runtime_errors_have_stable_exit_codes() {
        assert_eq!(ExitCode::Success as u8, 0);
        assert_eq!(ExitCode::Usage as u8, 2);
        assert_eq!(ExitCode::DaemonUnavailable as u8, 3);
        assert_eq!(ExitCode::RuntimeFailure as u8, 4);
        assert_eq!(ExitCode::UpstreamToolError as u8, 5);
    }

    #[test]
    fn exposes_version_and_detailed_help() {
        let version = Cli::try_parse_from(["mcp-host", "--version"])
            .expect_err("version should exit before parsing a command");
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert!(version.to_string().contains(env!("CARGO_PKG_VERSION")));

        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("transparent stdio bridge"));
        assert!(help.contains("harness install claude-code --scope user"));

        let mut command = Cli::command();
        let batch_help = command
            .find_subcommand_mut("batch")
            .expect("batch command")
            .render_long_help()
            .to_string();
        assert!(batch_help.contains("Results preserve input order"));
        assert!(batch_help.contains("mcp-host batch --calls-file calls.json"));
    }

    #[test]
    fn validates_harness_name_and_defaults() {
        let cli = parse(&["mcp-host", "harness", "install", "claude-code"]);
        let Command::Harness(Harness {
            command: HarnessCommand::Install(install),
        }) = cli.command
        else {
            panic!("expected harness install command");
        };

        assert_eq!(install.target, HarnessTarget::ClaudeCode);
        assert_eq!(install.name, "dynamic-mcp");
        assert_eq!(install.scope, ClaudeScope::User);
        assert!(
            Cli::try_parse_from([
                "mcp-host", "harness", "install", "opencode", "--name", "bad name"
            ])
            .is_err()
        );
    }
}
