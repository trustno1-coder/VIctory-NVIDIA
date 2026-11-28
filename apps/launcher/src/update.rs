//! Updates from the official repository: what it has that this checkout has
//! not, and whether pulling it here is safe.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::paths::Repo;

pub(crate) const OFFICIAL: &str = "https://github.com/trustno1-coder/VIctory-NVIDIA";
pub(crate) const ORIGINAL: &str = "https://github.com/Marvisak/VIctory";
/// The launcher's author's repository.
pub(crate) const AUTHOR: &str = "https://github.com/alexx307/VIctory-NVIDIA";
const BRANCH: &str = "main";
const SHOWN_COMMITS: usize = 15;
/// Git runs without a console window flashing up over the launcher.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Serialize)]
pub(crate) struct UpdateInfo {
    remote: String,
    behind: u32,
    commits: Vec<String>,
    /// Why updating here is not offered, in words for the user.
    blocked: Option<String>,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("Git is needed to update: install it from git-scm.com")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The remote that points at the official repository; `origin` if none does.
pub(crate) fn official_remote(remotes: &str) -> String {
    let official = OFFICIAL
        .trim_start_matches("https://github.com/")
        .to_ascii_lowercase();
    remotes
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((parts.next()?, parts.next()?))
        })
        .find(|(_, url)| {
            url.to_ascii_lowercase()
                .trim_end_matches(".git")
                .ends_with(&official)
        })
        .map_or_else(|| "origin".to_owned(), |(name, _)| name.to_owned())
}

/// Why pulling here would not be safe, or `None`.
pub(crate) fn blocked_reason(branch: &str, changes: &str, behind: u32) -> Option<String> {
    if behind == 0 {
        return None;
    }
    if branch != BRANCH {
        return Some(format!(
            "This checkout is on the branch {branch}: switch to {BRANCH} to update from here."
        ));
    }
    if !changes.is_empty() {
        return Some(
            "This checkout has changes not committed: commit or put them aside to update.".into(),
        );
    }
    None
}

impl UpdateInfo {
    pub(crate) fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    pub(crate) fn up_to_date(&self) -> bool {
        self.behind == 0
    }

    pub(crate) fn remote(&self) -> &str {
        &self.remote
    }
}

pub(crate) fn check(repo: &Repo) -> Result<UpdateInfo> {
    let dir = repo.root();
    let remote = official_remote(&git(dir, &["remote", "-v"])?);
    git(dir, &["fetch", "--quiet", &remote, BRANCH])?;
    let upstream = format!("{remote}/{BRANCH}");
    let range = format!("HEAD..{upstream}");
    let behind = git(dir, &["rev-list", "--count", &range])?
        .parse()
        .unwrap_or(0);
    let shown = format!("-{SHOWN_COMMITS}");
    let commits = git(dir, &["log", &shown, "--format=%s", &range])?
        .lines()
        .map(str::to_owned)
        .collect();
    let branch = git(dir, &["branch", "--show-current"])?;
    let changes = git(dir, &["status", "--porcelain", "--untracked-files=no"])?;
    Ok(UpdateInfo {
        blocked: blocked_reason(&branch, &changes, behind),
        remote,
        behind,
        commits,
    })
}

/// The update script, run once this launcher has closed: it pulls, rebuilds
/// the layer if it was built, rebuilds this launcher and starts it again.
pub(crate) fn apply_command(repo: &Repo, remote: &str) -> Result<Command> {
    let exe = std::env::current_exe().context("finding the launcher's own file")?;
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(repo.root().join(r"scripts\update-launcher.ps1"))
        .arg("-Remote")
        .arg(remote)
        .arg("-Branch")
        .arg(BRANCH)
        .arg("-LauncherPid")
        .arg(std::process::id().to_string())
        .arg("-LauncherExe")
        .arg(exe)
        .current_dir(repo.root());
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_official_remote_is_found_by_its_address() {
        let remotes = "origin\thttps://github.com/someone/VIctory-NVIDIA.git (fetch)\n\
                       upstream\thttps://github.com/trustno1-coder/VIctory-NVIDIA.git (fetch)\n";
        assert_eq!(official_remote(remotes), "upstream");
        let own = "origin\thttps://github.com/Trustno1-Coder/VIctory-NVIDIA (fetch)\n";
        assert_eq!(official_remote(own), "origin");
        assert_eq!(official_remote(""), "origin");
    }

    #[test]
    fn updating_is_offered_only_on_a_clean_main() {
        assert_eq!(blocked_reason("main", "", 3), None);
        assert_eq!(blocked_reason("feature", " M a.rs", 0), None);
        assert!(
            blocked_reason("feature", "", 3)
                .unwrap()
                .contains("feature")
        );
        assert!(blocked_reason("main", " M a.rs", 3).is_some());
    }
}
