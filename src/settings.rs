//! What readme keeps from one run to the next, as `key = "value"` lines (TOML, as far as
//! it goes) in `$XDG_CONFIG_HOME/readme/settings.toml` or `~/.config/readme/settings.toml`:
//! the background, code colors, word wrap, hidden files, tree order and pictures.

use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
};

/// Where the settings are kept. None in tests, which mustn't read or change the user's own.
pub fn path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    // A relative XDG_CONFIG_HOME is to be ignored, the XDG spec says
    let config = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|dir| dir.is_absolute()).or_else(|| {
        env::var_os("HOME").filter(|home| !home.is_empty()).map(|home| PathBuf::from(home).join(".config"))
    })?;
    Some(config.join("readme").join("settings.toml"))
}

/// Every key the file sets, and what to, without quotes; the first, for a key set twice.
pub fn read(path: &Path) -> HashMap<String, String> {
    let mut settings = HashMap::new();
    for line in fs::read_to_string(path).unwrap_or_default().lines() {
        if let Some((key, value)) = setting(line) {
            settings.entry(key.to_string()).or_insert_with(|| value.to_string());
        }
    }
    settings
}

/// Sets each key to its value, leaving the file's other lines as they are.
pub fn set(path: &Path, pairs: &[(&str, &str)]) -> io::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        // Rather than write over a file that's there but can't be read
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for (key, value) in pairs {
        let line = format!("{key} = \"{value}\"");
        match lines.iter().position(|l| setting(l).is_some_and(|(k, _)| k == *key)) {
            Some(i) => lines[i] = line,
            None => lines.push(line),
        }
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, lines.join("\n") + "\n")
}

/// The key `line` sets, and the value, if it's a setting's line.
fn setting(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || key.starts_with('#') {
        return None;
    }
    let value = value.trim();
    let value = match value.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next().unwrap_or_default(),
        // Unquoted, up to a comment
        None => value.split('#').next().unwrap_or_default().trim(),
    };
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_read_without_their_quotes_or_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        assert!(read(&path).is_empty());
        let text = "# background = \"White\"\nbackgrounds = \"x\"\nbackground = \"Navy\" # mine\nwrap=on # mine\n";
        fs::write(&path, text).unwrap();
        let settings = read(&path);
        assert_eq!(settings.get("background").map(String::as_str), Some("Navy"));
        assert_eq!(settings.get("wrap").map(String::as_str), Some("on"));
        assert_eq!(settings.len(), 3, "{settings:?}");
    }

    #[test]
    fn setting_some_keeps_the_other_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readme/settings.toml");
        set(&path, &[("background", "Navy")]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "background = \"Navy\"\n");
        fs::write(&path, "# mine\nwrap = \"on\"\nbackground = \"Navy\"\n").unwrap();
        set(&path, &[("background", "OS default"), ("pictures", "auto")]).unwrap();
        let text = "# mine\nwrap = \"on\"\nbackground = \"OS default\"\npictures = \"auto\"\n";
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}
