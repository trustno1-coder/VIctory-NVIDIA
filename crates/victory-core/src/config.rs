//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! See `victory.example.toml` at the repository root.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::{ensure_outside_repo, is_within};

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
        Self::load_from(Path::new("victory.toml"), |k| std::env::var_os(k))
    }

    fn load_from(
        default_path: &Path,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let explicit = explicit_config_path(&env);
        let path = explicit.as_deref().unwrap_or(default_path);
        let (source, parsed) = match std::fs::symlink_metadata(path) {
            Err(err) if err.kind() == ErrorKind::NotFound => {
                anyhow::ensure!(
                    explicit.is_none(),
                    "VICTORY_CONFIG points at {}, which does not exist",
                    path.display()
                );
                (None, FileConfig::default())
            }
            result => {
                let metadata = result.with_context(|| format!("checking {}", path.display()))?;
                // Follow links, but don't treat a dangling link as a missing optional file.
                let metadata = if metadata.is_symlink() {
                    std::fs::metadata(path)
                        .with_context(|| format!("resolving {}", path.display()))?
                } else {
                    metadata
                };
                anyhow::ensure!(
                    metadata.is_file(),
                    "{} is not a regular file",
                    path.display()
                );
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                (Some(path.to_path_buf()), parsed)
            }
        };
        Self::resolve(parsed.paths, source, env)
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
        if is_within(&data_dir, &cache_dir)? {
            anyhow::bail!(
                "data_dir ({}) must not be the cache_dir ({}) or inside it: the cache may be \
                 deleted at any time",
                data_dir.display(),
                cache_dir.display()
            );
        }
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

    fn isolated_paths(dir: &Path) -> impl Fn(&str) -> Option<OsString> {
        move |key| match key {
            "VICTORY_CACHE_DIR" => Some(dir.join("cache").into_os_string()),
            "VICTORY_DATA_DIR" => Some(dir.join("data").into_os_string()),
            _ => None,
        }
    }

    #[test]
    fn default_config_path_cannot_be_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("victory.toml");
        std::fs::create_dir(&path).unwrap();

        let error = Config::load_from(&path, |_| None).unwrap_err().to_string();
        assert!(error.contains("victory.toml"), "{error}");
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn explicit_config_path_cannot_be_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("custom.toml");
        std::fs::create_dir(&path).unwrap();
        let env = |k: &str| (k == "VICTORY_CONFIG").then(|| path.clone().into_os_string());

        let error = Config::load_from(&tmp.path().join("victory.toml"), env)
            .unwrap_err()
            .to_string();
        assert!(error.contains("custom.toml"), "{error}");
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn missing_default_config_is_optional() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("victory.toml");
        let cfg = Config::load_from(&path, isolated_paths(tmp.path())).unwrap();
        assert!(cfg.source.is_none());
    }

    #[test]
    fn missing_explicit_config_does_not_fall_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("custom.toml");
        let default = tmp.path().join("victory.toml");
        std::fs::write(&default, "[paths]\n").unwrap();
        let env = |k: &str| (k == "VICTORY_CONFIG").then(|| path.clone().into_os_string());

        let error = Config::load_from(&default, env).unwrap_err().to_string();
        assert!(
            error.contains("VICTORY_CONFIG") && error.contains("custom.toml"),
            "{error}"
        );
        assert!(error.contains("does not exist"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn default_config_can_link_to_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("victory.toml");
        std::fs::write(
            tmp.path().join("shared.toml"),
            "[paths]\ngame_dir = 'my-game'\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("shared.toml", &path).unwrap();

        let cfg = Config::load_from(&path, isolated_paths(tmp.path())).unwrap();
        assert_eq!(cfg.source, Some(path));
        assert_eq!(cfg.game_dir, Some(PathBuf::from("my-game")));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_default_config_link_is_not_treated_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("victory.toml");
        std::os::unix::fs::symlink("missing.toml", &path).unwrap();

        let error = Config::load_from(&path, |_| None).unwrap_err().to_string();
        assert!(
            error.contains("resolving") && error.contains("victory.toml"),
            "{error}"
        );
    }

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

    fn resolve_dirs(cache: &Path, data: &Path) -> Result<Config> {
        let file = FilePaths {
            cache_dir: Some(cache.to_path_buf()),
            data_dir: Some(data.to_path_buf()),
            ..Default::default()
        };
        Config::resolve(file, None, |_| None)
    }

    #[test]
    fn data_dir_equal_to_cache_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_dirs(tmp.path(), tmp.path()).is_err());
    }

    #[test]
    fn data_dir_inside_cache_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        assert!(resolve_dirs(&cache, &cache.join("data")).is_err());
        assert!(resolve_dirs(&cache, &cache.join("a/../data")).is_err());
    }

    #[test]
    fn sibling_with_similar_prefix_and_cache_inside_data_are_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        assert!(resolve_dirs(&cache, &tmp.path().join("cache-old")).is_ok());
        let data = tmp.path().join("data");
        assert!(resolve_dirs(&data.join("cache"), &data).is_ok());
    }
}
