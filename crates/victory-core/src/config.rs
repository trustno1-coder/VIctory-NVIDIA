//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! See `victory.example.toml` at the repository root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of the title.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let explicit = explicit_config_path(|k| std::env::var_os(k));
        let path = explicit
            .clone()
            .unwrap_or_else(|| PathBuf::from("victory.toml"));
        let file = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Some((path, text))
        } else if let Some(p) = explicit {
            anyhow::bail!(
                "VICTORY_CONFIG points at {}, which does not exist",
                p.display()
            );
        } else {
            None
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                (Some(path), parsed)
            }
            None => (None, FileConfig::default()),
        };
        Self::resolve(parsed.paths, source, |k| std::env::var_os(k))
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let test_titles = match env("VICTORY_TEST_TITLES") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => file.test_titles,
        };
        let cache_dir = env_path("VICTORY_CACHE_DIR")
            .or(file.cache_dir)
            .map_or_else(|| default_dir("cache"), Ok)?;
        let data_dir = env_path("VICTORY_DATA_DIR")
            .or(file.data_dir)
            .map_or_else(|| default_dir("data"), Ok)?;
        Ok(Config {
            game_dir: env_path("VICTORY_GAME_DIR").or(file.game_dir),
            cache_dir,
            data_dir,
            test_titles,
            source,
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set VICTORY_GAME_DIR or [paths].game_dir in \
             victory.toml (see victory.example.toml)",
        )
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }

    /// Removes the preloaded shader caches: `<cache>/shaders` (the
    /// translated shaders) and its sibling `pipelines` folder (the host
    /// pipelines made ahead) — what F1's CLEAR SHADERS asks a start for.
    /// Nothing else under the victory home is touched, and a missing folder
    /// is not an error. What went: the files and their bytes, for the log.
    pub fn clear_shader_caches(&self) -> Result<(u64, u64)> {
        let shaders = self.cache_subdir("shaders")?;
        let pipelines = shaders.with_file_name("pipelines");
        let (mut files, mut bytes) = (0, 0);
        for dir in [&shaders, &pipelines] {
            let (f, b) = remove_folder(dir)?;
            files += f;
            bytes += b;
        }
        Ok((files, bytes))
    }
}

/// The config file explicitly named by the environment, if any. An empty
/// `VICTORY_CONFIG` means unset, like the other path variables.
fn explicit_config_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Removes `dir` with everything in it, saying how much went: the files and
/// their bytes. A folder that is not there is nothing removed, not an error.
fn remove_folder(dir: &Path) -> Result<(u64, u64)> {
    if !dir.is_dir() {
        return Ok((0, 0));
    }
    let (mut files, mut bytes) = (0, 0);
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let read = std::fs::read_dir(&folder).with_context(|| format!("reading {}", folder.display()))?;
        for entry in read {
            let entry = entry.with_context(|| format!("reading {}", folder.display()))?;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                folders.push(entry.path());
            } else {
                files += 1;
                bytes += entry.metadata().map_or(0, |m| m.len());
            }
        }
    }
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok((files, bytes))
}

/// The folder for everything of ours (settings, logs, caches, saves,
/// downloads) when `VICTORY_HOME` names one: for a full C: drive, e.g.
/// `setx VICTORY_HOME D:\victory`. Else `%LOCALAPPDATA%\victory`.
pub fn home_override() -> Option<PathBuf> {
    std::env::var_os("VICTORY_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `%LOCALAPPDATA%/victory/<name>` (elsewhere the XDG or home cache), or
/// `VICTORY_HOME/<name>`.
fn default_dir(name: &str) -> Result<PathBuf> {
    if let Some(home) = home_override() {
        return Ok(home.join(name));
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(|| OsString::from("from-env"));
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths::default();
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(|| value.clone());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn empty_config_env_is_ignored() {
        let empty = |k: &str| (k == "VICTORY_CONFIG").then(OsString::new);
        assert_eq!(explicit_config_path(empty), None);
        let set = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("custom.toml"));
        assert_eq!(
            explicit_config_path(set),
            Some(PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }

    #[test]
    fn clearing_the_shaders_takes_only_the_caches() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config {
            cache_dir: home.path().join("cache"),
            data_dir: home.path().join("data"),
            ..Default::default()
        };
        // What a preload leaves: the translated shaders (a folder inside
        // them too) and the pipelines made ahead.
        for (dir, file, bytes) in [
            (cfg.cache_dir.join("shaders"), "one", 100usize),
            (cfg.cache_dir.join("shaders").join("inside"), "two", 50),
            (cfg.cache_dir.join("pipelines"), "three", 25),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{file}.bin")), vec![0u8; bytes]).unwrap();
        }
        // What a clear must leave alone: the menu's settings, the saves, the
        // logs and every other folder under the victory home.
        std::fs::write(home.path().join("victory-settings.txt"), b"saved by the menu").unwrap();
        std::fs::create_dir_all(cfg.data_dir.join("00000000").join("storage")).unwrap();
        std::fs::write(cfg.data_dir.join("00000000").join("storage").join("save.bin"), b"save")
            .unwrap();
        std::fs::create_dir_all(home.path().join("logs")).unwrap();
        std::fs::write(home.path().join("logs").join("a.log"), b"log").unwrap();
        std::fs::create_dir_all(cfg.cache_dir.join("images")).unwrap();
        std::fs::write(cfg.cache_dir.join("images").join("picture.bin"), b"picture").unwrap();

        let (files, bytes) = cfg.clear_shader_caches().unwrap();
        assert_eq!((files, bytes), (3, 175));
        assert!(!cfg.cache_dir.join("shaders").exists());
        assert!(!cfg.cache_dir.join("pipelines").exists());
        assert!(home.path().join("victory-settings.txt").is_file());
        assert!(cfg.data_dir.join("00000000").join("storage").join("save.bin").is_file());
        assert!(home.path().join("logs").join("a.log").is_file());
        assert!(cfg.cache_dir.join("images").join("picture.bin").is_file());
    }

    #[test]
    fn clearing_without_the_folders_is_no_error() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config {
            cache_dir: home.path().join("cache"),
            data_dir: home.path().join("data"),
            ..Default::default()
        };
        // Neither folder is there (the shaders one is made and taken away
        // again): nothing removed, no error.
        assert_eq!(cfg.clear_shader_caches().unwrap(), (0, 0));
        assert!(!cfg.cache_dir.join("shaders").exists());
        assert!(!cfg.cache_dir.join("pipelines").exists());
    }
}
