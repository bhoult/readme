//! A folder's git history, read with the git command: commits, what each one
//! changed, and files as they were at any commit.

use std::{
    io::Read,
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

/// Most of git's output that's read: enough for any file readme shows, and a stop
/// on a huge file from an old commit or an enormous diff.
const MAX_OUTPUT: u64 = 50 << 20;

/// git's output, cut off at MAX_OUTPUT (the second value says whether it was).
fn git_capped(repo: &Path, args: &[&str]) -> Result<(Vec<u8>, bool)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("couldn't run git")?;
    // Read alongside, so a chatty git can't stall on a full pipe
    let mut stderr = child.stderr.take().context("no stderr")?;
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut out = Vec::new();
    child.stdout.take().context("no stdout")?.take(MAX_OUTPUT + 1).read_to_end(&mut out)?;
    let cut = out.len() as u64 > MAX_OUTPUT;
    if cut {
        out.truncate(MAX_OUTPUT as usize);
        let _ = child.kill();
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if !cut && !status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), errors.trim());
    }
    Ok((out, cut))
}

/// git's output; an error if there's more than MAX_OUTPUT of it.
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let (out, cut) = git_capped(repo, args)?;
    if cut {
        bail!("more than {} MB", MAX_OUTPUT >> 20);
    }
    Ok(out)
}

/// git's output, with a note at the end if it had to be cut short.
fn git_text(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let (mut out, cut) = git_capped(repo, args)?;
    if cut {
        let note = format!("\n… cut short at {} MB\n", MAX_OUTPUT >> 20);
        // Cut at the end of a line and leave room for the note, so the whole stays within
        // MAX_OUTPUT, which is also the most a buffer takes
        out.truncate(out.len().saturating_sub(note.len()));
        out.truncate(out.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1));
        out.extend_from_slice(note.as_bytes());
    }
    Ok(out)
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
        // -z gives the file's name as it is; otherwise git quotes names with accents and the like
        args.extend(["--follow", "--name-only", "-z"]);
    }
    args.extend(["--", path.as_ref()]);
    let out = git(repo, &args)?;
    Ok(String::from_utf8_lossy(&out)
        .split('\x1e')
        .filter_map(|record| {
            // The commit's line ends in a newline, or with -z in a NUL, and the file's name
            // follows on a line of its own
            let (head, names) = record.split_once(['\n', '\0']).unwrap_or((record, ""));
            let mut f = head.split('\x1f');
            Some(Commit {
                hash: f.next().filter(|h| !h.is_empty())?.to_string(),
                short: f.next()?.to_string(),
                parent: f.next()?.split(' ').next().filter(|p| !p.is_empty()).map(String::from),
                time: f.next()?.parse().unwrap_or(0),
                subject: f.next()?.to_string(),
                path: names.split('\0').map(|n| n.trim_start_matches('\n')).find(|n| !n.is_empty()).map(String::from),
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
    git_text(repo, &["show", "--stat", "--patch", "--format=fuller", "--no-color", hash])
}

/// What a commit did to one file.
pub fn file_diff(repo: &Path, commit: &Commit, path: &str) -> Result<Vec<u8>> {
    match &commit.parent {
        Some(parent) => git_text(repo, &["diff", "-M", "--no-color", parent, &commit.hash, "--", path]),
        None => git_text(repo, &["show", "--format=", "--no-color", &commit.hash, "--", path]),
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

        // Names git would quote (accents, quote marks) come back as they are
        let odd = "café \"x\".txt";
        run(&root, &["mv", "renamed.txt", odd]);
        run(&root, &["commit", "-q", "-m", "accents"]);
        let file = log(&root, &root.join(odd), true).unwrap();
        assert_eq!(file[0].path.as_deref(), Some(odd));
        assert_eq!(file[1].path.as_deref(), Some("renamed.txt"));
        assert_eq!(show(&root, &file[0].hash, odd).unwrap(), b"two\n");
    }
}
