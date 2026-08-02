use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use tempfile::NamedTempFile;

use crate::{cli::HarnessTarget, harness_paths::opencode_config_directory};

const SKILL: &str = include_str!("../../../skills/dynamic-mcp/SKILL.md");
const BLOCK_START: &str = "<!-- dynamic-mcp:start -->";
const BLOCK_END: &str = "<!-- dynamic-mcp:end -->";
const INSTRUCTION_BLOCK: &str = "<!-- dynamic-mcp:start -->\n## Dynamic MCP Tool Policy\n\nWhen Dynamic MCP tools are available, load the `dynamic-mcp` skill and use the MCP tools for server discovery, connection management, tool discovery, and invocation. Do not use the `mcp-host` terminal CLI for runtime operations when the MCP tool surface is available. The CLI is reserved for installation, daemon bootstrap, and diagnostics.\n<!-- dynamic-mcp:end -->";

#[derive(Debug)]
pub struct InstalledHarnessFiles {
    pub skill_path: PathBuf,
    pub instruction_path: PathBuf,
    pub skill_updated: bool,
    pub instruction_updated: bool,
}

pub fn install_harness_files(target: HarnessTarget) -> Result<InstalledHarnessFiles, String> {
    let base =
        BaseDirs::new().ok_or_else(|| "could not determine the user home directory".to_owned())?;
    let (skill_path, instruction_path) = match target {
        HarnessTarget::OpenCode => {
            let directory = opencode_config_directory()?;
            (
                directory.join("skills/dynamic-mcp/SKILL.md"),
                directory.join("AGENTS.md"),
            )
        }
        HarnessTarget::ClaudeCode => (
            base.home_dir().join(".claude/skills/dynamic-mcp/SKILL.md"),
            base.home_dir().join(".claude/CLAUDE.md"),
        ),
        HarnessTarget::All => return Err("all is not a concrete harness target".to_owned()),
    };

    let skill_updated = write_if_changed(&skill_path, ensure_trailing_newline(SKILL).as_bytes())?;
    let instruction_updated = upsert_managed_block(&instruction_path, INSTRUCTION_BLOCK)?;
    Ok(InstalledHarnessFiles {
        skill_path,
        instruction_path,
        skill_updated,
        instruction_updated,
    })
}

fn upsert_managed_block(path: &Path, block: &str) -> Result<bool, String> {
    let current = match fs::read_to_string(path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let starts = current.match_indices(BLOCK_START).collect::<Vec<_>>();
    let ends = current.match_indices(BLOCK_END).collect::<Vec<_>>();
    let block = ensure_trailing_newline(block);

    let updated = match (starts.as_slice(), ends.as_slice()) {
        ([], []) if current.trim().is_empty() => block,
        ([], []) => format!("{}\n{}", current.trim_end(), block),
        ([(start, _)], [(end, _)]) if start < end => {
            let end = end + BLOCK_END.len();
            format!(
                "{}{}{}",
                &current[..*start],
                block.trim_end(),
                &current[end..]
            )
        }
        _ => {
            return Err(format!(
                "{} contains duplicate or unbalanced Dynamic MCP managed markers",
                path.display()
            ));
        }
    };

    write_if_changed(path, ensure_trailing_newline(&updated).as_bytes())
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<bool, String> {
    match fs::read(path) {
        Ok(existing) if existing == content => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    }

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "could not create a temporary file in {}: {error}",
            parent.display()
        )
    })?;
    temp.write_all(content)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| {
            format!(
                "could not write a temporary file for {}: {error}",
                path.display()
            )
        })?;
    if let Ok(metadata) = fs::metadata(path) {
        temp.as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| {
                format!(
                    "could not preserve permissions for {}: {error}",
                    path.display()
                )
            })?;
    }
    temp.persist(path).map_err(|error| {
        format!(
            "could not atomically replace {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(true)
}

fn ensure_trailing_newline(value: &str) -> String {
    format!("{}\n", value.trim_end())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        BLOCK_END, BLOCK_START, INSTRUCTION_BLOCK, upsert_managed_block, write_if_changed,
    };

    #[test]
    fn managed_block_is_created_updated_and_idempotent() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("AGENTS.md");
        fs::write(&path, "# Existing\n\nKeep this.\n").expect("existing instructions");

        assert!(upsert_managed_block(&path, INSTRUCTION_BLOCK).expect("block should append"));
        assert!(!upsert_managed_block(&path, INSTRUCTION_BLOCK).expect("block should be stable"));
        let content = fs::read_to_string(&path).expect("managed instructions");
        assert!(content.starts_with("# Existing\n\nKeep this."));
        assert_eq!(content.matches(BLOCK_START).count(), 1);
        assert_eq!(content.matches(BLOCK_END).count(), 1);
    }

    #[test]
    fn duplicate_or_unbalanced_markers_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("CLAUDE.md");
        fs::write(
            &path,
            format!("{BLOCK_START}\n{BLOCK_START}\n{BLOCK_END}\n"),
        )
        .expect("duplicate markers");

        let error = upsert_managed_block(&path, INSTRUCTION_BLOCK)
            .expect_err("duplicate markers should fail");
        assert!(error.contains("duplicate or unbalanced"));
    }

    #[test]
    fn unchanged_file_is_not_rewritten() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("SKILL.md");

        assert!(write_if_changed(&path, b"skill\n").expect("first write"));
        let modified = fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert!(!write_if_changed(&path, b"skill\n").expect("second write"));
        assert_eq!(
            fs::metadata(&path)
                .expect("metadata")
                .modified()
                .expect("mtime"),
            modified
        );
    }
}
