use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use directories::BaseDirs;

pub fn opencode_config_directory() -> Result<PathBuf, String> {
    let base =
        BaseDirs::new().ok_or_else(|| "could not determine the user home directory".to_owned())?;
    Ok(opencode_config_directory_from(
        base.home_dir(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    ))
}

fn opencode_config_directory_from(home: &Path, xdg_config_home: Option<&OsStr>) -> PathBuf {
    let root = xdg_config_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    root.join("opencode")
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::Path};

    use super::opencode_config_directory_from;

    #[test]
    fn opencode_paths_follow_xdg_on_every_platform() {
        assert_eq!(
            opencode_config_directory_from(Path::new("/home/user"), None),
            Path::new("/home/user/.config/opencode")
        );
        assert_eq!(
            opencode_config_directory_from(
                Path::new("/home/user"),
                Some(OsStr::new("/custom/config")),
            ),
            Path::new("/custom/config/opencode")
        );
        assert_eq!(
            opencode_config_directory_from(Path::new("/home/user"), Some(OsStr::new(""))),
            Path::new("/home/user/.config/opencode")
        );
    }
}
