use std::path::{Path, PathBuf};

/// Where the repository's scripts, build and downloaded SDKs live.
#[derive(Clone)]
pub(crate) struct Repo {
    root: PathBuf,
}

impl Repo {
    /// The checkout this launcher was built from: the scripts it runs are there.
    pub(crate) fn of_this_build() -> Self {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .ancestors()
            .nth(2)
            .unwrap_or(manifest)
            .to_path_buf();
        Self::at(root)
    }

    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn play_script(&self) -> PathBuf {
        self.root.join(r"scripts\play-nvidia.ps1")
    }

    pub(crate) fn menu_file(&self) -> PathBuf {
        self.root.join("victory-settings.txt")
    }

    pub(crate) fn built_victory(&self) -> PathBuf {
        self.root.join(r"target\release\victory.exe")
    }

    pub(crate) fn fetch_script(&self, sdk: Sdk) -> PathBuf {
        self.root.join(match sdk {
            Sdk::Dlss => "Get-NVIDIA-DLSS.cmd",
            Sdk::Fsr => "Get-AMD-FSR.cmd",
        })
    }

    /// Whether the files the play script looks for are there.
    pub(crate) fn has_sdk(&self, sdk: Sdk) -> bool {
        let (var, default, file) = match sdk {
            Sdk::Dlss => (
                "VICTORY_STREAMLINE_DIR",
                r"nvidia-sdk\Streamline\bin\x64",
                "sl.interposer.dll",
            ),
            Sdk::Fsr => (
                "VICTORY_FIDELITYFX_DIR",
                r"amd-sdk\FidelityFX\bin",
                "amd_fidelityfx_framegeneration_dx12.dll",
            ),
        };
        let dir = std::env::var_os(var).map_or_else(|| self.root.join(default), PathBuf::from);
        dir.join(file).is_file()
    }

    /// Whether `path` is this repository or somewhere inside it.
    pub(crate) fn contains(&self, path: &Path) -> bool {
        match (self.root.canonicalize(), path.canonicalize()) {
            (Ok(root), Ok(path)) => path.starts_with(root),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Sdk {
    Dlss,
    Fsr,
}

/// `%LOCALAPPDATA%\victory`, where the layer keeps its logs, saves and settings.
pub(crate) fn local_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("victory"))
}

pub(crate) fn logs_dir() -> Option<PathBuf> {
    local_data().map(|d| d.join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_build_finds_its_repository() {
        assert!(Repo::of_this_build().play_script().is_file());
    }

    #[test]
    fn paths_inside_the_repository_are_recognised() {
        let repo = Repo::of_this_build();
        assert!(repo.contains(&repo.root().join("Cargo.toml")));
        let outside = tempfile::tempdir().unwrap();
        assert!(!repo.contains(outside.path()));
    }
}
