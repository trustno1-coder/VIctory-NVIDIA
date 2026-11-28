//! `victory-settings.txt`: `VICTORY_X=value` lines that the play script sets
//! as switches at every start (the F1 menu writes it too).

use std::path::Path;

use anyhow::{Context, Result};

pub(crate) fn entries(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(entry)
}

fn entry(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.trim_start().split_once('=')?;
    let key = key.trim_end();
    let valid = key.starts_with("VICTORY_")
        && key[8..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    valid.then(|| (key, value.trim()))
}

/// `text` with `values` in place: each key once, where it first was, or
/// added at the end. Every other line is kept as it was.
pub(crate) fn with_values(text: &str, values: &[(&str, String)]) -> String {
    let mut written = vec![false; values.len()];
    let mut out = String::new();
    for line in text.lines() {
        let key = entry(line).map(|(k, _)| k);
        match values.iter().position(|(k, _)| Some(*k) == key) {
            Some(i) if !written[i] => {
                out.push_str(&format!("{}={}\n", values[i].0, values[i].1));
                written[i] = true;
            }
            Some(_) => {}
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (i, (k, v)) in values.iter().enumerate() {
        if !written[i] {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
    out
}

pub(crate) fn read(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap_or_default()
}

pub(crate) fn set(file: &Path, values: &[(&str, String)]) -> Result<()> {
    std::fs::write(file, with_values(&read(file), values))
        .with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_replace_their_lines_and_keep_the_rest() {
        let old = "# a comment\nVICTORY_A=1\nVICTORY_B=2\nVICTORY_A=3\nnot a setting\n";
        let new = with_values(old, &[("VICTORY_A", "9".into()), ("VICTORY_C", "x".into())]);
        assert_eq!(
            new,
            "# a comment\nVICTORY_A=9\nVICTORY_B=2\nnot a setting\nVICTORY_C=x\n"
        );
    }

    #[test]
    fn only_victory_switches_are_entries() {
        let text = "VICTORY_MENU_FPS = 60\nvictory_x=1\nPATH=c\nVICTORY_x=2\n";
        assert_eq!(
            entries(text).collect::<Vec<_>>(),
            [("VICTORY_MENU_FPS", "60")]
        );
    }
}
