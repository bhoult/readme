//! A folder's git history, read with the git command: commits, what each one
//! changed, and files as they were at any commit.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

const MAX_COMMITS: usize = 5000;

pub struct Commit {
    pub hash: String,
    pub short: String,
    pub parent: Option<String>,
    /// Seconds since the epoch
    pub time: u64,
    pub subject: String,
    /// In a file's history, what the file was called at this commit
    pub path: Option<String>,
}

/// A file a commit added (A), modified (M), deleted (D), renamed (R) or copied (C).
pub struct Change {
    pub status: char,
    /// Relative to the top of the repository
    pub path: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("couldn't run git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// The top of the repository `dir` is in, if it's in one.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let top = String::from_utf8(out).ok()?;
    Some(PathBuf::from(top.trim_end()))
}

/// Commits that touched `path`, newest first: a folder, or with `follow` a single
/// file, followed back through renames.
pub fn log(repo: &Path, path: &Path, follow: bool) -> Result<Vec<Commit>> {
    let max = format!("--max-count={MAX_COMMITS}");
    let path = path.to_string_lossy();
    let mut args = vec!["log", max.as_str(), "--format=%x1e%H%x1f%h%x1f%P%x1f%at%x1f%s"];
    if follow {
        args.extend(["--follow", "--name-only"]);
    }
    args.extend(["--", path.as_ref()]);
    let out = git(repo, &args)?;
    Ok(String::from_utf8_lossy(&out)
        .split('\x1e')
        .filter_map(|record| {
            let mut lines = record.lines();
            let mut f = lines.next()?.split('\x1f');
            Some(Commit {
                hash: f.next().filter(|h| !h.is_empty())?.to_string(),
                short: f.next()?.to_string(),
                parent: f.next()?.split(' ').next().filter(|p| !p.is_empty()).map(String::from),
                time: f.next()?.parse().unwrap_or(0),
                subject: f.next()?.to_string(),
                // --name-only puts the file's name after a blank line
                path: lines.map(str::trim).find(|l| !l.is_empty()).map(String::from),
            })
        })
        .collect())
}

/// The files a commit changed, compared with its first parent.
pub fn changes(repo: &Path, commit: &Commit) -> Result<Vec<Change>> {
    let mut args = vec!["diff-tree", "-r", "-z", "-M", "--name-status", "--no-commit-id"];
    match &commit.parent {
        Some(parent) => args.extend([parent.as_str(), commit.hash.as_str()]),
        None => args.extend(["--root", commit.hash.as_str()]),
    }
    let out = git(repo, &args)?;
    let mut fields = out.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let mut changes = Vec::new();
    while let Some(status) = fields.next().filter(|s| !s.is_empty()) {
        let letter = status.chars().next().unwrap_or('M');
        // Renames and copies list the old path, then the new one
        if matches!(letter, 'R' | 'C') {
            fields.next();
        }
        let Some(path) = fields.next() else { break };
        changes.push(Change { status: letter, path });
    }
    Ok(changes)
}

/// Every file in the repository as of a commit.
pub fn files(repo: &Path, hash: &str) -> Result<Vec<String>> {
    let out = git(repo, &["ls-tree", "-r", "-z", "--name-only", hash])?;
    Ok(out.split(|&b| b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned()).collect())
}

/// A file's contents as of a commit.
pub fn show(repo: &Path, hash: &str, path: &str) -> Result<Vec<u8>> {
    git(repo, &["show", &format!("{hash}:{path}")])
}

/// The whole commit: its message, which files changed, and the diff.
pub fn commit_text(repo: &Path, hash: &str) -> Result<Vec<u8>> {
    git(repo, &["show", "--stat", "--patch", "--format=fuller", "--no-color", hash])
}

/// What a commit did to one file.
pub fn file_diff(repo: &Path, commit: &Commit, path: &str) -> Result<Vec<u8>> {
    match &commit.parent {
        Some(parent) => git(repo, &["diff", "-M", "--no-color", parent, &commit.hash, "--", path]),
        None => git(repo, &["show", "--format=", "--no-color", &commit.hash, "--", path]),
    }
}

/// "5m", "3h", "2d", "6w", "4mo", "2y"
pub fn age(time: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let s = now.saturating_sub(time);
    match s {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 14 * 86_400 => format!("{}d", s / 86_400),
        s if s < 60 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s if s < 365 * 86_400 => format!("{}mo", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn history_of_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        run(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "first"]);
        fs::write(root.join("a.txt"), "two\n").unwrap();
        fs::write(root.join("b.txt"), "new\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "second"]);

        assert_eq!(repo_root(&root).unwrap(), root);
        let commits = log(&root, &root, false).unwrap();
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["second", "first"]);
        assert!(commits[1].parent.is_none());

        let changed: Vec<(char, String)> =
            changes(&root, &commits[0]).unwrap().into_iter().map(|c| (c.status, c.path)).collect();
        assert_eq!(changed, vec![('M', "a.txt".into()), ('A', "b.txt".into())]);
        let first: Vec<(char, String)> =
            changes(&root, &commits[1]).unwrap().into_iter().map(|c| (c.status, c.path)).collect();
        assert_eq!(first, vec![('A', "a.txt".into())]);

        assert_eq!(show(&root, &commits[1].hash, "a.txt").unwrap(), b"one\n");
        assert_eq!(files(&root, &commits[0].hash).unwrap(), vec!["a.txt", "b.txt"]);
        let diff = String::from_utf8(file_diff(&root, &commits[0], "a.txt").unwrap()).unwrap();
        assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
        assert!(repo_root(&std::env::temp_dir().join("surely-not-a-repo-xyz")).is_none());

        // A file's history follows it back through a rename
        run(&root, &["mv", "a.txt", "renamed.txt"]);
        run(&root, &["commit", "-q", "-m", "rename"]);
        let file = log(&root, &root.join("renamed.txt"), true).unwrap();
        let names: Vec<(&str, Option<&str>)> = file.iter().map(|c| (c.subject.as_str(), c.path.as_deref())).collect();
        assert_eq!(names, vec![("rename", Some("renamed.txt")), ("second", Some("a.txt")), ("first", Some("a.txt"))]);
    }
}
