//! What the launcher itself remembers: where the user's own dump is.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Repo;

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LauncherConfig {
    /// The title's executable in the user's own dump.
    pub exe: Option<PathBuf>,
}

impl LauncherConfig {
    pub(crate) fn file() -> Option<PathBuf> {
        crate::paths::local_data().map(|d| d.join("launcher.toml"))
    }

    pub(crate) fn load(file: &Path) -> Self {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, file: &Path) -> Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string(self)?;
        std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))
    }
}

/// Why `exe` cannot be started, in words for the user, or `None` if it can.
pub(crate) fn exe_problem(exe: &Path, repo: &Repo) -> Option<String> {
    if !exe.is_file() {
        return Some(format!("{} is not there any more.", exe.display()));
    }
    let is_exe = exe
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if !is_exe {
        return Some("Pick the title's .exe file in your dump.".into());
    }
    if repo.contains(exe) {
        return Some(
            "Your dump must stay outside the repository folder: move it elsewhere.".into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("launcher.toml");
        assert_eq!(LauncherConfig::load(&file), LauncherConfig::default());
        let config = LauncherConfig {
            exe: Some(PathBuf::from(r"D:\dump\title.exe")),
        };
        config.save(&file).unwrap();
        assert_eq!(LauncherConfig::load(&file), config);
    }

    #[test]
    fn a_dump_inside_the_repository_is_refused() {
        let repo = Repo::of_this_build();
        let inside = repo.root().join("Cargo.toml");
        assert!(exe_problem(&inside, &repo).is_some());
        let outside = tempfile::tempdir().unwrap();
        let exe = outside.path().join("title.exe");
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(exe_problem(&exe, &repo), None);
        assert!(exe_problem(&outside.path().join("missing.exe"), &repo).is_some());
    }
}
