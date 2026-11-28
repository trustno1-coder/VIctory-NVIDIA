//! Starting things in a console window of their own, so their output stays
//! visible and closing the launcher does not stop them.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::paths::{Repo, Sdk};

const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// The play script with the user's dump; it builds the layer the first time
/// and reads the choices from the menu file.
pub(crate) fn play_command(repo: &Repo, exe: &Path) -> Command {
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(repo.play_script())
        .arg("-Exe")
        .arg(exe)
        .current_dir(repo.root());
    cmd
}

pub(crate) fn fetch_command(repo: &Repo, sdk: Sdk) -> Command {
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/c")
        .arg(repo.fetch_script(sdk))
        .current_dir(repo.root());
    cmd
}

pub(crate) fn in_new_console(mut cmd: Command) -> Result<()> {
    cmd.creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .with_context(|| format!("starting {:?}", cmd.get_program()))?;
    Ok(())
}

pub(crate) fn open_folder(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .context("starting File Explorer")?;
    Ok(())
}

/// Opens `url` in the default browser.
pub(crate) fn open_url(url: &str) -> Result<()> {
    Command::new("explorer.exe")
        .arg(url)
        .spawn()
        .context("opening the browser")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_play_command_passes_the_dump_as_one_argument() {
        let repo = Repo::of_this_build();
        let exe = Path::new(r"D:\My Games\dump it's\title.exe");
        let cmd = play_command(&repo, exe);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args.last().copied(), Some(exe.as_os_str()));
        assert_eq!(args[args.len() - 2], "-Exe");
        assert_eq!(cmd.get_current_dir(), Some(repo.root()));
    }
}
