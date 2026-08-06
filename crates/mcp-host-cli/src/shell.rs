use std::{
    collections::BTreeMap,
    io::IsTerminal as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use clap::Parser as _;
use mcp_host_core::{ControlRequest, RuntimeError, ServerSummary};
use rustyline::{
    Context, Helper, Result as ReadlineResult, completion::Completer, error::ReadlineError,
    highlight::Highlighter, hint::Hinter, validate::Validator,
};

use crate::cli::{Cli, Command, ExitCode as CliExitCode};
use crate::commands;
use crate::ipc::send_control;

const COMMANDS: &[&str] = &[
    "daemon",
    "list",
    "inspect",
    "connect",
    "disconnect",
    "tools",
    "refresh",
    "call",
    "batch",
    "status",
    "auth",
    "skill",
    "package",
    "harness",
    "doctor",
    "help",
    "exit",
];

const ALIASES: &[(&str, &str)] = &[
    ("d", "daemon"),
    ("ls", "list"),
    ("i", "inspect"),
    ("c", "connect"),
    ("dc", "disconnect"),
    ("t", "tools"),
    ("rf", "refresh"),
    ("ca", "call"),
    ("b", "batch"),
    ("st", "status"),
    ("a", "auth"),
    ("sk", "skill"),
    ("pkg", "package"),
    ("h", "harness"),
];

const FLAGS: &[&str] = &[
    "--refresh",
    "--arguments",
    "--arguments-file",
    "--input",
    "--input-file",
    "--calls",
    "--calls-file",
    "--runtime-dir",
    "--timeout",
    "--json",
];

#[derive(Debug, Clone, Default)]
struct Cache {
    servers: Vec<String>,
    tools: BTreeMap<String, Vec<String>>,
}

struct HostHelper {
    cache: Arc<Mutex<Cache>>,
}

impl Completer for HostHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> ReadlineResult<(usize, Vec<String>)> {
        Ok(complete_line(
            &self
                .cache
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
            line,
            pos,
        ))
    }
}

impl Hinter for HostHelper {
    type Hint = String;
}

impl Highlighter for HostHelper {}

impl Validator for HostHelper {}

impl Helper for HostHelper {}

fn complete_line(cache: &Cache, line: &str, pos: usize) -> (usize, Vec<String>) {
    let prefix = &line[..pos.min(line.len())];
    let (words, current, start) = split_for_completion(prefix);
    let command = words.first().map(String::as_str).unwrap_or("");

    if !prefix.contains(' ') {
        let mut names: Vec<String> = COMMANDS.iter().map(|name| (*name).to_owned()).collect();
        names.extend(ALIASES.iter().map(|(alias, _)| (*alias).to_owned()));
        names.sort();
        return (start, filter_prefix(&names, &current));
    }

    if current.starts_with('-') {
        let flags: Vec<String> = FLAGS.iter().map(|flag| (*flag).to_owned()).collect();
        return (start, filter_prefix(&flags, &current));
    }

    let command = ALIASES
        .iter()
        .find_map(|(alias, target)| (alias == &command).then_some(*target))
        .unwrap_or(command);

    let candidates = match command {
        "inspect" | "connect" | "disconnect" | "tools" | "refresh" => {
            if words.len() == 1 {
                cache.servers.clone()
            } else {
                Vec::new()
            }
        }
        "call" => match words.len() {
            1 => cache.servers.clone(),
            2 => cache
                .tools
                .get(words[1].as_str())
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        },
        "skill" if words.len() == 1 => vec!["list".to_owned(), "run".to_owned()],
        "daemon" if words.len() == 1 => vec![
            "run".to_owned(),
            "status".to_owned(),
            "stop".to_owned(),
            "service".to_owned(),
        ],
        _ => Vec::new(),
    };
    (start, filter_prefix(&candidates, &current))
}

fn split_for_completion(line: &str) -> (Vec<String>, String, usize) {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = line.len();
    let mut token_start = indent;
    let mut in_quote = None;
    for (index, character) in trimmed.char_indices() {
        let absolute = indent + index;
        match (in_quote, character) {
            (Some(quote), '"' | '\'') if quote == character => {
                in_quote = None;
                current.push(character);
            }
            (Some(_), _) => current.push(character),
            (None, '"' | '\'') => {
                in_quote = Some(character);
                if current.is_empty() {
                    token_start = absolute + 1;
                }
            }
            (None, ' ' | '\t') => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                token_start = absolute + 1;
            }
            (None, _) => current.push(character),
        }
    }
    if !line.ends_with(' ') && !current.is_empty() {
        start = token_start;
    }
    if words.is_empty() && current.is_empty() {
        start = line.len();
    }
    (words, current, start)
}

fn filter_prefix(candidates: &[String], prefix: &str) -> Vec<String> {
    let mut matches: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.starts_with(prefix))
        .cloned()
        .collect();
    matches.sort();
    matches
}

/// Split a shell line into words, honoring single and double quotes.
pub fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quote = None;
    for character in line.chars() {
        match (in_quote, character) {
            (Some(quote), '"' | '\'') if quote == character => in_quote = None,
            (Some(_), _) => current.push(character),
            (None, '"' | '\'') => in_quote = Some(character),
            (None, ' ' | '\t') => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, _) => current.push(character),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

pub async fn run_shell(
    runtime_dir: PathBuf,
    timeout: Duration,
) -> Result<CliExitCode, RuntimeError> {
    let cache = Arc::new(Mutex::new(Cache::default()));
    refresh_cache(&cache, &runtime_dir, timeout).await;

    if std::io::stdin().is_terminal() {
        run_interactive(runtime_dir, timeout, cache).await
    } else {
        run_scripted(runtime_dir, timeout, cache).await
    }
}

async fn run_interactive(
    runtime_dir: PathBuf,
    timeout: Duration,
    cache: Arc<Mutex<Cache>>,
) -> Result<CliExitCode, RuntimeError> {
    let history_path = shell_history_path();
    let mut editor =
        rustyline::Editor::<HostHelper, rustyline::history::FileHistory>::with_history(
            rustyline::config::Config::builder().build(),
            rustyline::history::FileHistory::new(),
        )
        .map_err(|error| shell_error(format!("could not initialize the terminal: {error}")))?;
    if let Some(path) = &history_path {
        let _ = editor.load_history(path);
    }
    editor.set_helper(Some(HostHelper {
        cache: Arc::clone(&cache),
    }));
    println!("Dynamic MCP Host shell — type `help` for commands, Tab to complete, Ctrl-D to exit.");

    loop {
        match editor.readline("mcp-host> ") {
            Ok(line) => {
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line.as_str());
                if !handle_line(&line, &runtime_dir, timeout, &cache).await {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(error) => {
                eprintln!("error: {error}");
                break;
            }
        }
    }
    if let Some(path) = &history_path {
        let _ = editor.save_history(path);
    }
    Ok(CliExitCode::Success)
}

async fn run_scripted(
    runtime_dir: PathBuf,
    timeout: Duration,
    cache: Arc<Mutex<Cache>>,
) -> Result<CliExitCode, RuntimeError> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|error| shell_error(format!("could not read stdin: {error}")))?
    {
        let trimmed = line.trim().to_owned();
        if trimmed.is_empty() {
            continue;
        }
        if !handle_line(&trimmed, &runtime_dir, timeout, &cache).await {
            break;
        }
    }
    Ok(CliExitCode::Success)
}

async fn handle_line(
    line: &str,
    runtime_dir: &Path,
    timeout: Duration,
    cache: &Arc<Mutex<Cache>>,
) -> bool {
    match line {
        "exit" | "quit" | "q" | "exit()" | "quit()" => return false,
        "help" | "?" => {
            print_help();
            return true;
        }
        _ => {}
    }
    let words = split_words(line);
    let parsed = match Cli::try_parse_from(std::iter::once("mcp-host".to_owned()).chain(words)) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = error.print();
            return true;
        }
    };
    let mut parsed = parsed;
    if parsed.runtime_dir.is_none() {
        parsed.runtime_dir = Some(runtime_dir.to_path_buf());
    }
    if parsed.timeout.is_none() {
        parsed.timeout = Some(timeout.as_millis() as u64);
    }
    match &parsed.command {
        Command::Shell => {
            eprintln!("error: nested shell sessions are not supported");
            return true;
        }
        Command::Mcp(_) => {
            eprintln!("error: the `mcp` bridge cannot run inside the shell");
            return true;
        }
        Command::Harness(_) | Command::Doctor | Command::Completions(_) | Command::Init(_) => {
            let result = commands::dispatch_local(parsed).await;
            if let Err(error) = result {
                eprintln!("{error}");
            }
            refresh_cache(cache, runtime_dir, timeout).await;
            return true;
        }
        _ => {}
    }
    if uses_stdin_input(&parsed.command) {
        eprintln!("error: stdin-based inputs are not supported inside the shell");
        return true;
    }
    let result = commands::dispatch_daemon(parsed).await;
    match result {
        Ok(_) => {}
        Err(error) => eprintln!("{error}"),
    }
    refresh_cache(cache, runtime_dir, timeout).await;
    true
}

fn uses_stdin_input(command: &Command) -> bool {
    matches!(
        command,
        Command::Call(call)
            if call.arguments_file.as_deref() == Some(Path::new("-"))
    ) || matches!(
        command,
        Command::Batch(batch) if batch.calls_file.as_deref() == Some(Path::new("-"))
    ) || matches!(
        command,
        Command::Skill(crate::cli::Skill {
            command: crate::cli::SkillCommand::Run(run)
        }) if run.input_file.as_deref() == Some(Path::new("-"))
    )
}

fn print_help() {
    println!("Commands (aliases in parentheses):");
    for (name, alias, description) in [
        (
            "daemon (d)",
            "run | status | stop | service",
            "manage the daemon",
        ),
        ("list (ls)", "", "list configured servers"),
        ("inspect (i)", "SERVER", "inspect one server"),
        ("connect (c)", "SERVER", "connect one server"),
        ("disconnect (dc)", "SERVER", "disconnect one server"),
        ("tools (t)", "SERVER [--refresh]", "list a server's tools"),
        ("refresh (rf)", "SERVER", "refresh a server's tools"),
        (
            "call (ca)",
            "SERVER TOOL [--arguments JSON]",
            "invoke a tool",
        ),
        (
            "batch (b)",
            "--calls JSON",
            "invoke up to 32 tools in parallel",
        ),
        ("status (st)", "", "daemon health and runtime state"),
        (
            "auth (a)",
            "login | status | logout SERVER",
            "OAuth credentials",
        ),
        ("skill (sk)", "list | run", "runtime skills"),
        (
            "package (pkg)",
            "install SERVER",
            "install a downstream package",
        ),
        (
            "harness (h)",
            "install TARGET",
            "configure an AI coding harness",
        ),
        ("doctor", "", "run health checks"),
        ("help", "", "show this help"),
        ("exit", "", "leave the shell (Ctrl-D also works)"),
    ] {
        if alias.is_empty() {
            println!("  {name:<18} {description}");
        } else {
            println!("  {name:<18} {alias:<26} {description}");
        }
    }
    println!("\nTab completes commands, server IDs, tool names, and flags.");
}

fn shell_history_path() -> Option<PathBuf> {
    let directories = directories::ProjectDirs::from("org", "mcp-host", "mcp-host")?;
    let directory = directories
        .state_dir()
        .or_else(|| Some(directories.data_local_dir()))
        .map(PathBuf::from);
    let directory = directory?;
    std::fs::create_dir_all(&directory).ok()?;
    Some(directory.join("history.txt"))
}

async fn refresh_cache(cache: &Arc<Mutex<Cache>>, runtime_dir: &Path, timeout: Duration) {
    let Ok(value) = send_control(runtime_dir, &ControlRequest::ListServers, timeout).await else {
        return;
    };
    let Some(servers) = value.as_array() else {
        return;
    };
    let mut next = Cache {
        servers: Vec::new(),
        tools: BTreeMap::new(),
    };
    for server in servers {
        let Ok(summary) = serde_json::from_value::<ServerSummary>(server.clone()) else {
            continue;
        };
        next.servers.push(summary.id.clone());
        if summary.observed_state != mcp_host_core::LifecycleState::Connected {
            continue;
        }
        let Ok(tools) = send_control(
            runtime_dir,
            &ControlRequest::ListTools {
                server_id: summary.id.clone(),
                refresh: false,
            },
            timeout,
        )
        .await
        else {
            continue;
        };
        let Some(tools) = tools
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
        else {
            continue;
        };
        next.tools.insert(
            summary.id.clone(),
            tools
                .iter()
                .filter_map(|tool| tool.get("name"))
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect(),
        );
    }
    if let Ok(mut cache) = cache.lock() {
        *cache = next;
    }
}

fn shell_error(message: String) -> RuntimeError {
    RuntimeError::new(
        mcp_host_core::RuntimeErrorCode::ProtocolError,
        "shell",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::{Cache, complete_line, split_words};

    fn cache(servers: &[&str], tools: &[(&str, &[&str])]) -> Cache {
        let mut cache = Cache::default();
        cache
            .servers
            .extend(servers.iter().map(|server| (*server).to_owned()));
        for (server, names) in tools {
            cache.tools.insert(
                (*server).to_owned(),
                names.iter().map(|name| (*name).to_owned()).collect(),
            );
        }
        cache
    }

    #[test]
    fn split_words_honors_quotes() {
        assert_eq!(
            split_words("call fixture echo --arguments '{\"a\": 1}'"),
            ["call", "fixture", "echo", "--arguments", "{\"a\": 1}"]
        );
        assert_eq!(split_words("exit"), ["exit"]);
        assert_eq!(split_words("  "), Vec::<String>::new());
    }

    #[test]
    fn completion_lists_commands_on_empty_line() {
        let (start, candidates) = complete_line(&cache(&[], &[]), "", 0);
        assert_eq!(start, 0);
        assert!(candidates.contains(&"list".to_owned()));
        assert!(candidates.contains(&"ls".to_owned()));
        assert!(candidates.contains(&"exit".to_owned()));
    }

    #[test]
    fn completion_filters_commands_by_prefix() {
        let (_, candidates) = complete_line(&cache(&[], &[]), "c", 1);
        assert!(candidates.contains(&"call".to_owned()));
        assert!(candidates.contains(&"connect".to_owned()));
        assert!(candidates.contains(&"c".to_owned()));
        assert!(!candidates.contains(&"list".to_owned()));
    }

    #[test]
    fn completion_suggests_servers_for_server_commands() {
        let (start, candidates) =
            complete_line(&cache(&["fixture", "filesystem"], &[]), "connect fix", 11);
        assert_eq!(start, 8);
        assert_eq!(candidates, ["fixture"]);
    }

    #[test]
    fn completion_suggests_tools_for_call() {
        let (_, candidates) = complete_line(
            &cache(&["fixture"], &[("fixture", &["echo", "add", "sleep"])]),
            "call fixture e",
            14,
        );
        assert_eq!(candidates, ["echo"]);
    }

    #[test]
    fn completion_suggests_flags() {
        let (_, candidates) = complete_line(&cache(&["fixture"], &[]), "tools fixture --r", 16);
        assert!(candidates.contains(&"--refresh".to_owned()));
    }

    #[test]
    fn completion_inside_quotes_preserves_the_quote() {
        let (start, candidates) = complete_line(
            &cache(&["fixture"], &[("fixture", &["echo", "add"])]),
            "call fixture \"e",
            15,
        );
        assert_eq!(start, 14, "replacement must start after the opening quote");
        assert_eq!(candidates, ["echo"]);
    }

    #[test]
    fn completion_inside_single_quotes_works() {
        let (start, candidates) = complete_line(
            &cache(&["fixture"], &[("fixture", &["echo"])]),
            "call fixture 'e",
            15,
        );
        assert_eq!(start, 14);
        assert_eq!(candidates, ["echo"]);
    }
}
