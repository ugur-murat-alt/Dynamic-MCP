//! Desired service descriptors and file-backed service installation primitives.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

const MANAGED_MARKER_PREFIX: &str = "dynamic-mcp-managed: v1 name=";
const MANAGED_HASH_PREFIX: &str = "dynamic-mcp-hash: fnv1a64=";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceManagerKind {
    Systemd,
    Launchd,
    WindowsScm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceScope {
    User,
    System,
}

/// Secret-free inputs used to render a daemon service descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceInstallOptions {
    pub scope: ServiceScope,
    pub name: String,
    pub description: String,
    pub executable: PathBuf,
    pub config_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub descriptor_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceDescriptor {
    pub kind: ServiceManagerKind,
    pub scope: ServiceScope,
    pub name: String,
    pub executable: PathBuf,
    pub config_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub argv: Vec<String>,
    pub artifact: ServiceArtifact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceArtifact {
    File { path: PathBuf, content: String },
    WindowsScm(WindowsServiceSpec),
}

/// A shell-free Windows SCM request. A native backend owns its persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsServiceSpec {
    pub service_name: String,
    pub display_name: String,
    pub description: String,
    pub argv: Vec<String>,
    pub binary_path: String,
    pub managed_marker: String,
}

impl WindowsServiceSpec {
    #[must_use]
    pub fn managed_display_name(&self) -> String {
        format!("{} [{}]", self.display_name, self.managed_marker)
    }

    #[must_use]
    pub fn ownership_marker(&self) -> String {
        format!("{MANAGED_MARKER_PREFIX}{}", self.service_name)
    }

    #[must_use]
    pub fn inspect_display_name(&self, display_name: &str) -> ServiceStatus {
        if display_name.matches(&self.ownership_marker()).count() != 1 {
            return ServiceStatus::Foreign;
        }
        ServiceStatus::Installed {
            drifted: !display_name.contains(&self.managed_marker),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceStatus {
    NotInstalled,
    Installed { drifted: bool },
    Foreign,
}

/// Renders a platform-specific descriptor without starting a shell or service manager.
pub fn render_descriptor(
    kind: ServiceManagerKind,
    options: &ServiceInstallOptions,
) -> Result<ServiceDescriptor, String> {
    validate_options(kind, options)?;
    let argv = match kind {
        ServiceManagerKind::WindowsScm => windows_daemon_argv(options),
        ServiceManagerKind::Systemd | ServiceManagerKind::Launchd => daemon_argv(options),
    };
    let artifact = match kind {
        ServiceManagerKind::Systemd => ServiceArtifact::File {
            path: options
                .descriptor_dir
                .join(format!("{}.service", options.name)),
            content: render_systemd(options, &argv),
        },
        ServiceManagerKind::Launchd => ServiceArtifact::File {
            path: options
                .descriptor_dir
                .join(format!("dev.dynamic-mcp.{}.plist", options.name)),
            content: render_launchd(options, &argv),
        },
        ServiceManagerKind::WindowsScm => {
            ServiceArtifact::WindowsScm(render_windows_scm(options, &argv))
        }
    };

    Ok(ServiceDescriptor {
        kind,
        scope: options.scope,
        name: options.name.clone(),
        executable: options.executable.clone(),
        config_dir: options.config_dir.clone(),
        runtime_dir: options.runtime_dir.clone(),
        argv,
        artifact,
    })
}

/// Atomically installs a desired descriptor, rejecting descriptors not managed by Dynamic MCP.
pub fn install_descriptor(descriptor: &ServiceDescriptor) -> Result<bool, String> {
    validate_descriptor(descriptor)?;
    match &descriptor.artifact {
        ServiceArtifact::File { path, content } => {
            match inspect_file(path, &descriptor.name, Some(content.as_bytes()))? {
                ServiceStatus::Foreign => Err(format!(
                    "{} exists and is not managed by dynamic-mcp",
                    path.display()
                )),
                ServiceStatus::NotInstalled | ServiceStatus::Installed { .. } => {
                    write_if_changed(path, content.as_bytes())
                }
            }
        }
        ServiceArtifact::WindowsScm(_) => install_windows_scm(descriptor),
    }
}

pub fn validate_descriptor(descriptor: &ServiceDescriptor) -> Result<(), String> {
    validate_descriptor_paths(descriptor)
}

/// Removes a managed descriptor without invoking a shell or service manager.
pub fn remove_descriptor(descriptor: &ServiceDescriptor) -> Result<bool, String> {
    match &descriptor.artifact {
        ServiceArtifact::File { path, .. } => match inspect_file(path, &descriptor.name, None)? {
            ServiceStatus::NotInstalled => Ok(false),
            ServiceStatus::Foreign => Err(format!(
                "{} exists and is not managed by dynamic-mcp",
                path.display()
            )),
            ServiceStatus::Installed { .. } => fs::remove_file(path)
                .map(|()| true)
                .map_err(|error| format!("could not remove {}: {error}", path.display())),
        },
        ServiceArtifact::WindowsScm(_) => remove_windows_scm(descriptor),
    }
}

/// Inspects descriptor ownership and content drift without invoking a shell or service manager.
pub fn inspect_descriptor(descriptor: &ServiceDescriptor) -> Result<ServiceStatus, String> {
    match &descriptor.artifact {
        ServiceArtifact::File { path, content } => {
            inspect_file(path, &descriptor.name, Some(content.as_bytes()))
        }
        ServiceArtifact::WindowsScm(_) => inspect_windows_scm(descriptor),
    }
}

fn validate_options(
    kind: ServiceManagerKind,
    options: &ServiceInstallOptions,
) -> Result<(), String> {
    validate_name(&options.name)?;
    if options.description.trim().is_empty() || has_control_character(&options.description) {
        return Err(
            "service description must be non-empty and contain no control characters".to_owned(),
        );
    }
    for (label, path) in [
        ("executable", &options.executable),
        ("config directory", &options.config_dir),
        ("runtime directory", &options.runtime_dir),
        ("descriptor directory", &options.descriptor_dir),
    ] {
        if !path.is_absolute() {
            return Err(format!("{label} must be an absolute path"));
        }
        if path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        {
            return Err(format!("{label} must not contain control characters"));
        }
    }
    if kind == ServiceManagerKind::WindowsScm && options.scope != ServiceScope::System {
        return Err("Windows SCM only supports system services".to_owned());
    }
    Ok(())
}

fn validate_descriptor_paths(descriptor: &ServiceDescriptor) -> Result<(), String> {
    if !descriptor.executable.is_file() {
        return Err(format!(
            "{} is not a regular executable file",
            descriptor.executable.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if descriptor
            .executable
            .metadata()
            .map_err(|error| {
                format!(
                    "could not inspect executable {}: {error}",
                    descriptor.executable.display()
                )
            })?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(format!(
                "{} is not executable",
                descriptor.executable.display()
            ));
        }
    }
    for (label, path) in [
        ("config directory", &descriptor.config_dir),
        ("runtime directory", &descriptor.runtime_dir),
    ] {
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        if !path.is_absolute() {
            return Err(format!("{label} must be an absolute path"));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err("service name must match [A-Za-z0-9][A-Za-z0-9_.-]{0,63}".to_owned())
    }
}

fn daemon_argv(options: &ServiceInstallOptions) -> Vec<String> {
    vec![
        options.executable.display().to_string(),
        "--runtime-dir".to_owned(),
        options.runtime_dir.display().to_string(),
        "daemon".to_owned(),
        "run".to_owned(),
        "--config-dir".to_owned(),
        options.config_dir.display().to_string(),
    ]
}

fn windows_daemon_argv(options: &ServiceInstallOptions) -> Vec<String> {
    vec![
        options.executable.display().to_string(),
        "--runtime-dir".to_owned(),
        options.runtime_dir.display().to_string(),
        "daemon".to_owned(),
        "service-run".to_owned(),
        "--config-dir".to_owned(),
        options.config_dir.display().to_string(),
        "--name".to_owned(),
        options.name.clone(),
    ]
}

fn render_systemd(options: &ServiceInstallOptions, argv: &[String]) -> String {
    let body = format!(
        "# {MANAGED_MARKER_PREFIX}{}\n[Unit]\nDescription={}\n\n[Service]\nType=simple\nExecStart={}\nRestart=on-failure\n\n[Install]\nWantedBy={}\n",
        options.name,
        escape_systemd_text(&options.description),
        argv.iter()
            .map(|argument| escape_systemd_argument(argument))
            .collect::<Vec<_>>()
            .join(" "),
        match options.scope {
            ServiceScope::User => "default.target",
            ServiceScope::System => "multi-user.target",
        }
    );
    with_hash(body, "# ")
}

fn render_launchd(options: &ServiceInstallOptions, argv: &[String]) -> String {
    let arguments = argv
        .iter()
        .map(|argument| format!("\t\t<string>{}</string>\n", escape_xml(argument)))
        .collect::<String>();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<!-- {MANAGED_MARKER_PREFIX}{} -->\n<dict>\n\t<key>Label</key>\n\t<string>dev.dynamic-mcp.{}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n{}\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n</dict>\n</plist>\n",
        options.name, options.name, arguments
    );
    with_hash(body, "<!-- ")
}

fn render_windows_scm(options: &ServiceInstallOptions, argv: &[String]) -> WindowsServiceSpec {
    let binary_path = argv
        .iter()
        .map(|argument| quote_windows_argument(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let payload = format!(
        "{MANAGED_MARKER_PREFIX}{}\nservice_name={}\ndisplay_name={}\nbinary_path={}\n",
        options.name, options.name, options.description, binary_path
    );
    WindowsServiceSpec {
        service_name: options.name.clone(),
        display_name: options.description.clone(),
        description: options.description.clone(),
        argv: argv.to_vec(),
        binary_path,
        managed_marker: format!(
            "{MANAGED_MARKER_PREFIX}{} {MANAGED_HASH_PREFIX}{:016x}",
            options.name,
            fnv1a64(payload.as_bytes())
        ),
    }
}

fn with_hash(body: String, prefix: &str) -> String {
    let hash = fnv1a64(body.as_bytes());
    if prefix == "<!-- " {
        format!("{}<!-- {MANAGED_HASH_PREFIX}{hash:016x} -->\n{}", body, "")
    } else {
        format!("{}{}{}{:016x}\n", body, prefix, MANAGED_HASH_PREFIX, hash)
    }
}

fn inspect_file(
    path: &Path,
    expected_name: &str,
    desired: Option<&[u8]>,
) -> Result<ServiceStatus, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ServiceStatus::NotInstalled);
        }
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    if !metadata.file_type().is_file() {
        return Ok(ServiceStatus::Foreign);
    }
    let content = match String::from_utf8(
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?,
    ) {
        Ok(content) => content,
        Err(_) => return Ok(ServiceStatus::Foreign),
    };
    let marker = format!("{MANAGED_MARKER_PREFIX}{expected_name}");
    if content.matches(&marker).count() != 1 || content.matches(MANAGED_MARKER_PREFIX).count() != 1
    {
        return Ok(ServiceStatus::Foreign);
    }
    let Some((content_without_hash, recorded_hash)) = split_managed_hash(&content) else {
        return Ok(ServiceStatus::Foreign);
    };
    Ok(ServiceStatus::Installed {
        drifted: recorded_hash != fnv1a64(content_without_hash.as_bytes())
            || desired.is_some_and(|desired| desired != content.as_bytes()),
    })
}

fn split_managed_hash(content: &str) -> Option<(String, u64)> {
    let lines = content.lines().collect::<Vec<_>>();
    let hash_lines = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(MANAGED_HASH_PREFIX))
        .collect::<Vec<_>>();
    let [(index, line)] = hash_lines.as_slice() else {
        return None;
    };
    let index = *index;
    let value = line
        .split(MANAGED_HASH_PREFIX)
        .nth(1)?
        .trim_end_matches(" -->")
        .trim();
    if value.len() != 16 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let hash = u64::from_str_radix(value, 16).ok()?;
    let mut without_hash = lines;
    without_hash.remove(index);
    Some((format!("{}\n", without_hash.join("\n")), hash))
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
    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "could not create a temporary file in {}: {error}",
            parent.display()
        )
    })?;
    temporary
        .write_all(content)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| {
            format!(
                "could not write a temporary file for {}: {error}",
                path.display()
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(|error| format!("could not secure {}: {error}", path.display()))?;
    }
    temporary.persist(path).map_err(|error| {
        format!(
            "could not atomically replace {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(true)
}

fn escape_systemd_argument(argument: &str) -> String {
    format!(
        "\"{}\"",
        argument
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

fn escape_systemd_text(value: &str) -> String {
    value.replace('%', "%%")
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'"'))
    {
        return argument.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn has_control_character(value: &str) -> bool {
    value.chars().any(char::is_control)
}

fn fnv1a64(value: &[u8]) -> u64 {
    value.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(windows)]
fn install_windows_scm(descriptor: &ServiceDescriptor) -> Result<bool, String> {
    crate::windows_service_backend::install(descriptor, false)
        .map(|result| result.artifact_updated)
        .map_err(|error| error.message.to_owned())
}

#[cfg(not(windows))]
fn install_windows_scm(_descriptor: &ServiceDescriptor) -> Result<bool, String> {
    Err("Windows SCM descriptors can only be installed on Windows".to_owned())
}

#[cfg(windows)]
fn remove_windows_scm(descriptor: &ServiceDescriptor) -> Result<bool, String> {
    crate::windows_service_backend::uninstall(descriptor)
        .map(|result| result.artifact_removed)
        .map_err(|error| error.message.to_owned())
}

#[cfg(not(windows))]
fn remove_windows_scm(_descriptor: &ServiceDescriptor) -> Result<bool, String> {
    Err("Windows SCM descriptors can only be removed on Windows".to_owned())
}

#[cfg(windows)]
fn inspect_windows_scm(descriptor: &ServiceDescriptor) -> Result<ServiceStatus, String> {
    crate::windows_service_backend::status(descriptor)
        .map(|report| report.artifact)
        .map_err(|error| error.message.to_owned())
}

#[cfg(not(windows))]
fn inspect_windows_scm(_descriptor: &ServiceDescriptor) -> Result<ServiceStatus, String> {
    Err("Windows SCM descriptors can only be inspected on Windows".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use super::{
        MANAGED_HASH_PREFIX, ServiceArtifact, ServiceInstallOptions, ServiceManagerKind,
        ServiceScope, ServiceStatus, inspect_descriptor, install_descriptor,
        quote_windows_argument, remove_descriptor, render_descriptor,
    };

    fn options(root: &Path) -> ServiceInstallOptions {
        let executable = root.join("bin with spaces/mcp-host");
        fs::create_dir_all(executable.parent().expect("binary parent")).expect("binary parent");
        fs::write(&executable, "binary").expect("binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .expect("binary permissions");
        }
        let config_dir = root.join("config with spaces");
        let runtime_dir = root.join("runtime with spaces");
        fs::create_dir_all(&config_dir).expect("config directory");
        fs::create_dir_all(&runtime_dir).expect("runtime directory");
        ServiceInstallOptions {
            scope: ServiceScope::User,
            name: "dynamic-mcp".to_owned(),
            description: "Dynamic MCP Host".to_owned(),
            executable,
            config_dir,
            runtime_dir,
            descriptor_dir: root.join("descriptors"),
        }
    }

    fn file_content(descriptor: &super::ServiceDescriptor) -> (&Path, &str) {
        match &descriptor.artifact {
            ServiceArtifact::File { path, content } => (path, content),
            ServiceArtifact::WindowsScm(_) => panic!("expected a file descriptor"),
        }
    }

    #[test]
    fn systemd_renderer_golden_escapes_paths_with_spaces() {
        let root = tempdir().expect("temporary directory");
        let descriptor = render_descriptor(ServiceManagerKind::Systemd, &options(root.path()))
            .expect("systemd descriptor");
        let (_, content) = file_content(&descriptor);

        assert_eq!(
            content,
            format!(
                "# dynamic-mcp-managed: v1 name=dynamic-mcp\n[Unit]\nDescription=Dynamic MCP Host\n\n[Service]\nType=simple\nExecStart=\"{}\" \"--runtime-dir\" \"{}\" \"daemon\" \"run\" \"--config-dir\" \"{}\"\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n# {MANAGED_HASH_PREFIX}{:016x}\n",
                root.path().join("bin with spaces/mcp-host").display(),
                root.path().join("runtime with spaces").display(),
                root.path().join("config with spaces").display(),
                super::fnv1a64(
                    format!(
                        "# dynamic-mcp-managed: v1 name=dynamic-mcp\n[Unit]\nDescription=Dynamic MCP Host\n\n[Service]\nType=simple\nExecStart=\"{}\" \"--runtime-dir\" \"{}\" \"daemon\" \"run\" \"--config-dir\" \"{}\"\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
                        root.path().join("bin with spaces/mcp-host").display(),
                        root.path().join("runtime with spaces").display(),
                        root.path().join("config with spaces").display(),
                    )
                    .as_bytes()
                )
            )
        );
    }

    #[test]
    fn launchd_renderer_golden_preserves_argv_boundaries() {
        let root = tempdir().expect("temporary directory");
        let descriptor = render_descriptor(ServiceManagerKind::Launchd, &options(root.path()))
            .expect("launchd descriptor");
        let (_, content) = file_content(&descriptor);

        assert!(content.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(content.contains("<string>dev.dynamic-mcp.dynamic-mcp</string>"));
        assert!(content.contains(&format!(
            "<string>{}</string>",
            root.path().join("bin with spaces/mcp-host").display()
        )));
        assert_eq!(content.matches("<string>").count(), 8);
        assert!(content.contains(MANAGED_HASH_PREFIX));
    }

    #[test]
    fn install_is_atomic_idempotent_and_reports_drift() {
        let root = tempdir().expect("temporary directory");
        let descriptor = render_descriptor(ServiceManagerKind::Systemd, &options(root.path()))
            .expect("descriptor");
        let (path, _) = file_content(&descriptor);

        assert!(install_descriptor(&descriptor).expect("first install"));
        let modified = fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert!(!install_descriptor(&descriptor).expect("unchanged install"));
        assert_eq!(
            fs::metadata(path)
                .expect("metadata")
                .modified()
                .expect("mtime"),
            modified
        );
        fs::write(
            path,
            format!("{}# drift\n", fs::read_to_string(path).expect("content")),
        )
        .expect("drift");
        assert_eq!(
            inspect_descriptor(&descriptor).expect("drift status"),
            ServiceStatus::Installed { drifted: true }
        );
        assert!(install_descriptor(&descriptor).expect("repair drift"));
        assert_eq!(
            inspect_descriptor(&descriptor).expect("repaired status"),
            ServiceStatus::Installed { drifted: false }
        );

        let mut changed_options = options(root.path());
        changed_options.description = "Changed description".to_owned();
        let changed = render_descriptor(ServiceManagerKind::Systemd, &changed_options)
            .expect("changed descriptor");
        assert_eq!(
            inspect_descriptor(&changed).expect("desired drift status"),
            ServiceStatus::Installed { drifted: true }
        );
    }

    #[test]
    fn foreign_files_are_not_overwritten_or_removed() {
        let root = tempdir().expect("temporary directory");
        let descriptor = render_descriptor(ServiceManagerKind::Systemd, &options(root.path()))
            .expect("descriptor");
        let (path, _) = file_content(&descriptor);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(path, "[Service]\nExecStart=/foreign\n").expect("foreign service");

        assert_eq!(
            inspect_descriptor(&descriptor).expect("foreign status"),
            ServiceStatus::Foreign
        );
        assert!(
            install_descriptor(&descriptor)
                .expect_err("foreign install")
                .contains("not managed")
        );
        assert!(
            remove_descriptor(&descriptor)
                .expect_err("foreign remove")
                .contains("not managed")
        );
        assert_eq!(
            fs::read_to_string(path).expect("foreign file"),
            "[Service]\nExecStart=/foreign\n"
        );
    }

    #[test]
    fn invalid_names_and_relative_paths_are_rejected() {
        let root = tempdir().expect("temporary directory");
        for name in [
            "",
            "-dynamic",
            "dynamic mcp",
            "dynamic/mcp",
            &"a".repeat(65),
        ] {
            let mut value = options(root.path());
            value.name = name.to_owned();
            assert!(render_descriptor(ServiceManagerKind::Systemd, &value).is_err());
        }
        let mut value = options(root.path());
        value.runtime_dir = "relative".into();
        assert!(render_descriptor(ServiceManagerKind::Systemd, &value).is_err());
        let mut value = options(root.path());
        value.config_dir = root.path().join("config\ninjected");
        assert!(render_descriptor(ServiceManagerKind::Systemd, &value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_foreign_file_is_not_an_install_error() {
        let root = tempdir().expect("temporary directory");
        let descriptor = render_descriptor(ServiceManagerKind::Systemd, &options(root.path()))
            .expect("descriptor");
        let (path, _) = file_content(&descriptor);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(path, [0xff]).expect("foreign file");

        assert_eq!(
            inspect_descriptor(&descriptor).expect("foreign status"),
            ServiceStatus::Foreign
        );
    }

    #[test]
    fn windows_scm_spec_quotes_argv_without_a_shell() {
        let root = tempdir().expect("temporary directory");
        let mut value = options(root.path());
        value.scope = ServiceScope::System;
        let descriptor =
            render_descriptor(ServiceManagerKind::WindowsScm, &value).expect("Windows descriptor");
        let ServiceArtifact::WindowsScm(spec) = descriptor.artifact else {
            panic!("expected Windows SCM spec");
        };

        assert!(
            spec.binary_path
                .starts_with(&format!("\"{}\"", value.executable.display()))
        );
        assert!(
            spec.binary_path
                .contains(&format!("\"{}\"", value.runtime_dir.display()))
        );
        assert!(spec.binary_path.contains("service-run"));
        assert!(spec.managed_marker.contains("name=dynamic-mcp"));
        assert!(spec.managed_display_name().contains(&spec.managed_marker));
        assert_eq!(
            spec.inspect_display_name(&spec.managed_display_name()),
            ServiceStatus::Installed { drifted: false }
        );
        assert_eq!(
            spec.inspect_display_name("foreign service"),
            ServiceStatus::Foreign
        );
        assert_eq!(
            spec.inspect_display_name("Dynamic MCP [dynamic-mcp-managed: v1 name=dynamic-mcp]"),
            ServiceStatus::Installed { drifted: true }
        );
        assert_eq!(quote_windows_argument("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
