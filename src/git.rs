//! A folder's git history and branches, read with the git command: commits, what each
//! one changed, files as they were at any commit, and what each branch did on its own.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::{
    cores,
    text::{split_at_width, str_width},
};

const MAX_COMMITS: usize = 5000;
/// Most branches listed, the most recently updated
const MAX_BRANCHES: usize = 500;
/// Most of a branch's commits listed on its page
const MAX_LISTED: usize = 200;

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
#[derive(Clone)]
pub struct Change {
    pub status: char,
    /// Relative to the top of the repository
    pub path: String,
    /// What a renamed or copied file was called before
    pub from: Option<String>,
}

/// Most of git's output that's read: enough for any file readme shows, and a stop
/// on a huge file from an old commit or an enormous diff.
const MAX_OUTPUT: u64 = 50 << 20;

/// git's output, cut off at `max` bytes (the second value says whether it was). Paths
/// are taken as they are, so a file called `a[1].txt` doesn't also match `a1.txt`.
fn git_capped(repo: &Path, args: &[&str], max: u64) -> Result<(Vec<u8>, bool)> {
    let mut child = Command::new("git")
        .arg("--literal-pathspecs")
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
    child.stdout.take().context("no stdout")?.take(max + 1).read_to_end(&mut out)?;
    let cut = out.len() as u64 > max;
    if cut {
        out.truncate(max as usize);
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
    let (out, cut) = git_capped(repo, args, MAX_OUTPUT)?;
    if cut {
        bail!("more than {} MB", MAX_OUTPUT >> 20);
    }
    Ok(out)
}

/// git's output, with a note at the end if it had to be cut short.
fn git_text(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_text_within(repo, args, MAX_OUTPUT)
}

/// git's output in at most `max` bytes, with a note at the end if it had to be cut short.
fn git_text_within(repo: &Path, args: &[&str], max: u64) -> Result<Vec<u8>> {
    let (mut out, cut) = git_capped(repo, args, max)?;
    if cut {
        let note = format!("\n… cut short at {} MB\n", max >> 20);
        // Cut at the end of a line and leave room for the note, so the whole stays within
        // `max`; MAX_OUTPUT is also the most a buffer takes
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
            // A subject can hold the separators too, and start a record of its own. Hashes are
            // checked, as they go on to git's command line, where one like --output=… would
            // be taken as an option.
            let hash = f.next().filter(|h| is_hash(h))?.to_string();
            let short = f.next().filter(|h| is_hash(h))?.to_string();
            let parent = f.next()?.split(' ').next().filter(|p| !p.is_empty());
            if parent.is_some_and(|p| !is_hash(p)) {
                return None;
            }
            Some(Commit {
                hash,
                short,
                parent: parent.map(String::from),
                time: f.next()?.parse().unwrap_or(0),
                subject: f.next()?.to_string(),
                path: names.split('\0').map(|n| n.trim_start_matches('\n')).find(|n| !n.is_empty()).map(String::from),
            })
        })
        .collect())
}

/// A commit's hash, as git prints it in full or short: hex digits and nothing else.
fn is_hash(text: &str) -> bool {
    (4..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The files a commit changed, compared with its first parent.
pub fn changes(repo: &Path, commit: &Commit) -> Result<Vec<Change>> {
    let mut args = vec!["diff-tree", "-r", "-z", "-M", "--name-status", "--no-commit-id"];
    match &commit.parent {
        Some(parent) => args.extend(["--end-of-options", parent.as_str(), commit.hash.as_str()]),
        None => args.extend(["--root", "--end-of-options", commit.hash.as_str()]),
    }
    Ok(name_status(&git(repo, &args)?))
}

/// The files that differ from commit `from` to commit `to`, only those under `scope`
/// if it's given.
pub fn changes_between(repo: &Path, from: &str, to: &str, scope: Option<&Path>) -> Result<Vec<Change>> {
    let scope = scope.map(Path::to_string_lossy);
    let mut args = vec!["diff-tree", "-r", "-z", "-M", "--name-status", "--end-of-options", from, to];
    if let Some(scope) = &scope {
        args.extend(["--", scope.as_ref()]);
    }
    Ok(name_status(&git(repo, &args)?))
}

/// The changes in `--name-status -z` output.
fn name_status(out: &[u8]) -> Vec<Change> {
    let mut fields = out.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let mut changes = Vec::new();
    while let Some(status) = fields.next().filter(|s| !s.is_empty()) {
        let letter = status.chars().next().unwrap_or('M');
        // Renames and copies list the old path, then the new one
        let from = if matches!(letter, 'R' | 'C') { fields.next() } else { None };
        let Some(path) = fields.next() else { break };
        changes.push(Change { status: letter, path, from });
    }
    changes
}

/// Every file in the repository as of a commit.
pub fn files(repo: &Path, hash: &str) -> Result<Vec<String>> {
    let out = git(repo, &["ls-tree", "-r", "-z", "--name-only", "--end-of-options", hash])?;
    Ok(out.split(|&b| b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned()).collect())
}

/// A file's contents as of a commit.
pub fn show(repo: &Path, hash: &str, path: &str) -> Result<Vec<u8>> {
    git(repo, &["show", "--end-of-options", &format!("{hash}:{path}")])
}

/// What git prints for diffs: its own patches, not a diff program or text conversion
/// the user's or the repository's settings name (difftastic, say), which aren't diffs
/// readme can show, and which a repository that came with its .git/config could use to
/// run anything.
const PLAIN_DIFF: [&str; 3] = ["--no-ext-diff", "--no-textconv", "--no-color"];

/// The whole commit: its message, which files changed, and the diff.
pub fn commit_text(repo: &Path, hash: &str) -> Result<Vec<u8>> {
    let mut args = vec!["show", "--stat", "--patch", "--format=fuller"];
    args.extend(PLAIN_DIFF);
    args.extend(["--end-of-options", hash]);
    git_text(repo, &args)
}

/// What a commit did to one file.
pub fn file_diff(repo: &Path, commit: &Commit, path: &str) -> Result<Vec<u8>> {
    let Some(parent) = &commit.parent else {
        let mut args = vec!["show", "--format="];
        args.extend(PLAIN_DIFF);
        args.extend(["--end-of-options", &commit.hash, "--", path]);
        return git_text(repo, &args);
    };
    let from = changes(repo, commit).ok().and_then(|c| c.into_iter().find(|c| c.path == path)?.from);
    diff_between(repo, parent, &commit.hash, path, from.as_deref())
}

/// What changed in one file from commit `from` to commit `to`; `old` is what it was
/// called at `from`, if it was renamed.
pub fn diff_between(repo: &Path, from: &str, to: &str, path: &str, old: Option<&str>) -> Result<Vec<u8>> {
    // Given only the new name, git sees a renamed file as all new; given the old one
    // too, it shows the rename and what changed
    let mut args = vec!["diff", "-M"];
    args.extend(PLAIN_DIFF);
    args.extend(["--end-of-options", from, to, "--", path]);
    args.extend(old);
    git_text(repo, &args)
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

// Branches

/// A local branch, or a remote's that no local branch matches.
pub struct Branch {
    /// "main", or "origin/feature" for a remote's
    pub name: String,
    pub remote: bool,
    /// The commit it's at
    pub tip: String,
    /// When its last commit was made: seconds since the epoch, and in local time
    pub time: u64,
    pub date: String,
    /// Who made its last commit
    pub author: String,
    pub email: String,
    /// The one checked out
    pub current: bool,
}

pub struct Branches {
    /// The most recently updated first
    pub list: Vec<Branch>,
    /// The one the others are compared with
    pub base: usize,
    /// There were more than MAX_BRANCHES, and only the newest are listed
    pub more: bool,
}

/// The repository's branches, and the one the others are compared with: the remote's
/// default branch (the one origin/HEAD names), or else main, master, or the one checked
/// out. A remote's branch is left out where a local one of the same name is at the same
/// commit.
pub fn branches(repo: &Path) -> Result<Branches> {
    const FORMAT: &str = "--format=%(objectname)%00%(refname)%00%(committerdate:unix)%00\
        %(committerdate:format-local:%Y-%m-%d %H:%M)%00%(HEAD)%00%(symref)%00%(objecttype)%00\
        %(authorname)%00%(authoremail)";
    let out = git(repo, &["for-each-ref", "--sort=-committerdate", FORMAT, "refs/heads", "refs/remotes"])?;
    let text = String::from_utf8_lossy(&out);
    let mut refs: Vec<(&str, Branch)> = Vec::new();
    // The branch a remote's HEAD names as its default; origin's counts over others'
    let mut default = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\0').collect();
        let &[tip, refname, time, date, head, symref, kind, author, email] = fields.as_slice() else { continue };
        if !symref.is_empty() {
            if refname == "refs/remotes/origin/HEAD" || default.is_none() {
                default = Some(symref);
            }
            continue;
        }
        let (name, remote) = match (refname.strip_prefix("refs/heads/"), refname.strip_prefix("refs/remotes/")) {
            (Some(name), _) => (name, false),
            (_, Some(name)) => (name, true),
            _ => continue,
        };
        // Only commits have history to look into, and only a hash goes on git's command
        // line, never a name, which could be taken for an option
        if kind != "commit" || !is_hash(tip) {
            continue;
        }
        let email = email.strip_prefix('<').and_then(|e| e.strip_suffix('>')).unwrap_or(email);
        let branch = Branch {
            name: name.to_string(),
            remote,
            tip: tip.to_string(),
            time: time.parse().unwrap_or(0),
            date: date.to_string(),
            author: author.to_string(),
            email: email.to_string(),
            current: head == "*",
        };
        refs.push((refname, branch));
    }
    // A remote's branch with a local one of the same name at the same commit, which
    // stands in for it
    let local: HashMap<&str, usize> =
        refs.iter().enumerate().filter(|(_, (_, b))| !b.remote).map(|(i, (_, b))| (b.name.as_str(), i)).collect();
    let matched: Vec<Option<usize>> = refs
        .iter()
        .map(|(_, b)| {
            let (_, name) = b.name.split_once('/').filter(|_| b.remote)?;
            local.get(name).copied().filter(|&i| refs[i].1.tip == b.tip)
        })
        .collect();
    let find = |refname: &str| refs.iter().position(|(r, _)| *r == refname);
    let base = default
        .and_then(find)
        .or_else(|| find("refs/heads/main"))
        .or_else(|| find("refs/heads/master"))
        .or_else(|| refs.iter().position(|(_, b)| b.current))
        .or_else(|| refs.iter().position(|(_, b)| !b.remote))
        .or((!refs.is_empty()).then_some(0));
    let Some(base) = base else { bail!("no branches yet") };
    let base = matched[base].unwrap_or(base);

    let mut list = Vec::new();
    let mut base_at = 0;
    for (i, (_, branch)) in refs.into_iter().enumerate() {
        if matched[i].is_none() {
            if i == base {
                base_at = list.len();
            }
            list.push(branch);
        }
    }
    let more = list.len() > MAX_BRANCHES;
    if more {
        // The base stays, however old it is
        let kept = (base_at >= MAX_BRANCHES).then(|| list.remove(base_at));
        list.truncate(MAX_BRANCHES - usize::from(kept.is_some()));
        if let Some(branch) = kept {
            base_at = list.len();
            list.push(branch);
        }
    }
    Ok(Branches { list, base: base_at, more })
}

/// What a branch did on its own, since it split off from the base.
#[derive(Default)]
pub struct Work {
    /// Its own commits: on it, and not on the base
    pub commits: usize,
    /// Who made most of them: name, email, and how many
    pub owner: Option<(String, String, usize)>,
    /// Where it split off from the base; none if they have no history in common
    pub split: Option<String>,
    /// The files it changed, those under the file or folder asked about
    pub changes: Vec<Change>,
    /// Why it couldn't be looked into
    pub error: Option<String>,
}

/// What the branch at `tip` did since it split off from `base`, to `scope` (or with
/// none, to the whole repository).
pub fn branch_work(repo: &Path, base: &str, tip: &str, scope: Option<&Path>) -> Result<Work> {
    let range = format!("{base}..{tip}");
    let max = format!("--max-count={MAX_COMMITS}");
    let out = git(repo, &["log", &max, "--format=%ae%x1f%an", "--end-of-options", &range])?;
    // Newest first, so each author keeps the name they last gave
    let mut authors: Vec<(String, String, usize)> = Vec::new();
    let mut commits = 0;
    for line in String::from_utf8_lossy(&out).lines() {
        commits += 1;
        let (email, name) = line.split_once('\x1f').unwrap_or((line, ""));
        match authors.iter_mut().find(|a| a.1.eq_ignore_ascii_case(email)) {
            Some(author) => author.2 += 1,
            None => authors.push((name.to_string(), email.to_string(), 1)),
        }
    }
    // Whoever made the most; between equals, whoever made the latest
    let owner = authors.into_iter().rev().max_by_key(|a| a.2);
    let split = merge_base(repo, base, tip)?;
    let changes = match &split {
        Some(at) => changes_between(repo, at, tip, scope)?,
        None => Vec::new(),
    };
    Ok(Work { commits, owner, split, changes, error: None })
}

/// Where `tip` split off from `base`: the newest commit both have, or none if they have
/// no history in common (a gh-pages branch, say).
fn merge_base(repo: &Path, base: &str, tip: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--end-of-options", base, tip])
        .stdin(Stdio::null())
        .output()
        .context("couldn't run git")?;
    let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.status.code() {
        Some(0) if is_hash(&found) => Ok(Some(found)),
        // What git says when there's no commit in common
        Some(1) if found.is_empty() => Ok(None),
        _ => bail!("git merge-base: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// A branch's page: whose it is, when it last changed, its own commits, and what it did
/// to `scope` (the whole repository if none), which `what` names: "src/", "notes.md".
pub fn branch_text(
    repo: &Path,
    branches: &Branches,
    i: usize,
    work: Option<&Work>,
    scope: Option<&Path>,
    what: &str,
) -> Result<Vec<u8>> {
    let (branch, base) = (&branches.list[i], &branches.list[branches.base]);
    let mut text = format!("{}\n", branch.name);
    let updated = format!(
        "Updated   {} ({} ago), the last commit by {} <{}>\n",
        branch.date,
        age(branch.time),
        branch.author,
        branch.email
    );
    let changing = scope.map_or(String::new(), |_| format!(" that change {what}"));
    let work = match work {
        _ if i == branches.base => {
            text += "The branch the others are compared with\n";
            text += &updated;
            let lines = commit_lines(repo, &branch.tip, scope)?;
            text += &format!("\nIts latest commits{changing}:\n{}", lines.join("\n"));
            return Ok(text.into_bytes());
        }
        Some(Work { error: Some(why), .. }) => {
            text += &format!("Couldn't look into it: {why}\n");
            return Ok(text.into_bytes());
        }
        Some(work) => work,
        None => return Ok((text + "Couldn't look into it\n").into_bytes()),
    };
    if work.commits == 0 {
        text += &updated;
        text += &format!("Nothing of its own: every commit on it is on {} too\n", base.name);
        return Ok(text.into_bytes());
    }
    if let Some((name, email, made)) = &work.owner {
        let share = match (made, work.commits) {
            (_, 1) => "its one commit".to_string(),
            (m, c) if *m == c => format!("all {c} of its commits"),
            (m, c) => format!("{m} of its {c} commits"),
        };
        text += &format!("Owner     {name} <{email}>, who made {share}\n");
    }
    text += &updated;
    let range = format!("{}..{}", base.tip, branch.tip);
    let Some(split) = &work.split else {
        text += &format!("It has no history in common with {}, so there's nothing to compare\n", base.name);
        let lines = commit_lines(repo, &range, None)?;
        text += &format!("\nIts commits, newest first:\n{}\n", lines.join("\n"));
        return Ok(text.into_bytes());
    };
    text += &format!("Compared  with {}, from {} where it split off\n", base.name, &split[..split.len().min(7)]);
    let lines = commit_lines(repo, &range, scope)?;
    if !lines.is_empty() {
        let latest = if lines.len() == MAX_LISTED { format!(" (the latest {MAX_LISTED})") } else { String::new() };
        text += &format!("\nIts commits{changing}, newest first{latest}:\n{}\n", lines.join("\n"));
    }
    let scope = scope.map(Path::to_string_lossy);
    let mut args = vec!["diff", "--stat", "-M"];
    args.extend(PLAIN_DIFF);
    if work.changes.is_empty() {
        // What it changed, though none of it is under the scope
        text += &match &scope {
            Some(_) => format!("\nIt leaves {what} as it was. What it does change:\n"),
            None => "\nIn all, it changes nothing\n".to_string(),
        };
        args.extend(["--end-of-options", split, &branch.tip]);
    } else {
        text += "\n";
        args.extend(["--patch", "--end-of-options", split, &branch.tip]);
        args.extend(scope.as_deref().map(|s| ["--", s]).into_iter().flatten());
    }
    let room = MAX_OUTPUT.saturating_sub(text.len() as u64);
    let mut page = text.into_bytes();
    page.extend(git_text_within(repo, &args, room)?);
    Ok(page)
}

/// A line for each commit in `range` (newest first, at most MAX_LISTED) that touched
/// `scope`: "  1a2b3c4   3d  Alice Smith  Fix the thing".
fn commit_lines(repo: &Path, range: &str, scope: Option<&Path>) -> Result<Vec<String>> {
    let max = format!("--max-count={MAX_LISTED}");
    let scope = scope.map(Path::to_string_lossy);
    let mut args = vec!["log", &max, "--format=%h%x1f%ct%x1f%an%x1f%s", "--end-of-options", range];
    args.extend(scope.as_deref().map(|s| ["--", s]).into_iter().flatten());
    let out = git(repo, &args)?;
    let text = String::from_utf8_lossy(&out);
    let rows: Vec<[&str; 4]> = text
        .lines()
        .map(|line| {
            let mut fields = line.splitn(4, '\x1f');
            [(); 4].map(|_| fields.next().unwrap_or(""))
        })
        .collect();
    let width = rows.iter().map(|r| str_width(r[2])).max().unwrap_or(0).min(20);
    Ok(rows
        .iter()
        .map(|[short, time, name, subject]| {
            let name = split_at_width(name, width).0;
            let pad = " ".repeat(width - str_width(name));
            let age = age(time.parse().unwrap_or(0));
            format!("  {short}  {age:>3}  {name}{pad}  {subject}")
        })
        .collect())
}

/// Each branch's own work, looked into on all the cores. Results come in as they're done.
pub struct Survey {
    /// Each branch's, once it's come in; never the base's
    pub work: Vec<Option<Work>>,
    /// How many are still to come
    pub left: usize,
    stop: Arc<AtomicBool>,
    found: mpsc::Receiver<(usize, Work)>,
}

impl Survey {
    /// Starts looking into what each branch but the base did to `scope`.
    pub fn start(repo: &Path, branches: &Branches, scope: Option<&Path>) -> Self {
        let base = branches.list[branches.base].tip.clone();
        let jobs: Vec<(usize, String)> = branches
            .list
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != branches.base)
            .map(|(i, b)| (i, b.tip.clone()))
            .collect();
        let (sender, found) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let left = jobs.len();
        let (repo, scope, halt) = (repo.to_path_buf(), scope.map(Path::to_path_buf), stop.clone());
        thread::spawn(move || {
            let next = AtomicUsize::new(0);
            let (jobs, next, repo, scope, base, halt) = (&jobs, &next, &repo, &scope, &base, &halt);
            thread::scope(|s| {
                for _ in 0..cores().min(jobs.len()) {
                    let sender = sender.clone();
                    s.spawn(move || {
                        while !halt.load(Ordering::Relaxed) {
                            let Some((i, tip)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                            let work = branch_work(repo, base, tip, scope.as_deref())
                                .unwrap_or_else(|e| Work { error: Some(format!("{e:#}")), ..Work::default() });
                            if sender.send((*i, work)).is_err() {
                                break;
                            }
                        }
                    });
                }
            });
        });
        Self { work: branches.list.iter().map(|_| None).collect(), left, stop, found }
    }

    /// Takes in what's been found since last time.
    pub fn poll(&mut self) {
        loop {
            match self.found.try_recv() {
                Ok((i, work)) => {
                    self.work[i] = Some(work);
                    self.left = self.left.saturating_sub(1);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // The workers are gone without finishing: one panicked
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.left = 0;
                    break;
                }
            }
        }
    }
}

impl Drop for Survey {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

    /// git as `who` <who@x>, at `when` seconds since the epoch, away from the user's own settings.
    fn run_as(dir: &Path, who: &str, when: u64, args: &[&str]) {
        let (date, email) = (format!("{when} +0000"), format!("{}@x", who.to_lowercase()));
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .envs([("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")])
            .envs([("GIT_AUTHOR_NAME", who), ("GIT_COMMITTER_NAME", who)])
            .envs([("GIT_AUTHOR_EMAIL", &email), ("GIT_COMMITTER_EMAIL", &email)])
            .envs([("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A clone of a repository whose main has moved on since a feature branch split off from
    /// it, with a gh-pages-like branch that shares no history. Days count from `T0`.
    const T0: u64 = 1_700_000_000;
    const DAY: u64 = 86_400;

    fn clone_with_branches(dir: &Path) -> PathBuf {
        let up = dir.join("up");
        fs::create_dir_all(up.join("src")).unwrap();
        fs::write(up.join("src/a.txt"), "one\n").unwrap();
        fs::write(up.join("README"), "r\n").unwrap();
        run_as(dir, "Ann", T0, &["init", "-q", "-b", "main", "up"]);
        run_as(&up, "Ann", T0, &["add", "."]);
        run_as(&up, "Ann", T0, &["commit", "-q", "-m", "start"]);
        run_as(&up, "Ann", T0, &["checkout", "-q", "-b", "feature"]);
        fs::write(up.join("src/a.txt"), "two\n").unwrap();
        run_as(&up, "Alice", T0 + DAY, &["commit", "-q", "-am", "change a"]);
        fs::write(up.join("src/b.txt"), "new\n").unwrap();
        run_as(&up, "Alice", T0 + 2 * DAY, &["add", "."]);
        run_as(&up, "Alice", T0 + 2 * DAY, &["commit", "-q", "-m", "add b"]);
        run_as(&up, "Bob", T0 + 3 * DAY, &["commit", "-q", "--allow-empty", "-m", "nudge"]);
        run_as(&up, "Ann", T0, &["checkout", "-q", "main"]);
        fs::write(up.join("README"), "r2\n").unwrap();
        run_as(&up, "Ann", T0 + 4 * DAY, &["commit", "-q", "-am", "main moves"]);
        run_as(&up, "Pat", T0, &["checkout", "-q", "--orphan", "pages"]);
        run_as(&up, "Pat", T0, &["rm", "-rqf", "."]);
        fs::write(up.join("index.html"), "page\n").unwrap();
        run_as(&up, "Pat", T0 + 5 * DAY, &["add", "."]);
        run_as(&up, "Pat", T0 + 5 * DAY, &["commit", "-q", "-m", "pages"]);
        run_as(&up, "Ann", T0, &["checkout", "-q", "-f", "main"]);
        run_as(dir, "Ann", T0, &["clone", "-q", "up", "clone"]);
        let clone = dir.join("clone");
        // A local feature at origin's commit stands in for origin/feature
        run_as(&clone, "Ann", T0, &["branch", "-q", "feature", "origin/feature"]);
        clone
    }

    #[test]
    fn branches_whose_they_are_and_what_they_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = clone_with_branches(&dir.path().canonicalize().unwrap());
        let src = root.join("src");

        let b = branches(&root).unwrap();
        let names: Vec<&str> = b.list.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["origin/pages", "main", "feature"], "newest first, origin's own left out");
        assert_eq!((b.list[b.base].name.as_str(), b.list[b.base].current), ("main", true));
        assert_eq!((b.list[2].author.as_str(), b.list[2].email.as_str()), ("Bob", "bob@x"));
        assert!(b.list[0].remote && !b.more);

        // The feature's own commits and changes, not main's since it split off
        let base = &b.list[b.base].tip;
        let work = branch_work(&root, base, &b.list[2].tip, Some(&src)).unwrap();
        assert_eq!(work.commits, 3);
        assert_eq!(work.owner.as_ref().map(|o| (o.0.as_str(), o.1.as_str(), o.2)), Some(("Alice", "alice@x", 2)));
        let changed: Vec<(char, &str)> = work.changes.iter().map(|c| (c.status, c.path.as_str())).collect();
        assert_eq!(changed, vec![('M', "src/a.txt"), ('A', "src/b.txt")]);
        let readme = branch_work(&root, base, &b.list[2].tip, Some(&root.join("README"))).unwrap();
        assert!(readme.changes.is_empty() && readme.commits == 3);

        let page = branch_text(&root, &b, 2, Some(&work), Some(&src), "src/").unwrap();
        let page = String::from_utf8(page).unwrap();
        assert!(page.contains("Owner     Alice <alice@x>, who made 2 of its 3 commits"), "{page}");
        assert!(page.contains("by Bob <bob@x>") && page.contains("Compared  with main"), "{page}");
        assert!(page.contains("+two") && page.contains("src/b.txt") && !page.contains("r2"), "{page}");
        assert!(!page.contains("nudge"), "only the commits that change src/: {page}");

        // No history in common: nothing to compare, but its commits still show
        let pages = branch_work(&root, base, &b.list[0].tip, None).unwrap();
        assert!(pages.split.is_none() && pages.commits == 1 && pages.changes.is_empty());
        let page = String::from_utf8(branch_text(&root, &b, 0, Some(&pages), None, "clone").unwrap()).unwrap();
        assert!(page.contains("no history in common with main") && page.contains("pages"), "{page}");
        let page = String::from_utf8(branch_text(&root, &b, b.base, None, Some(&src), "src/").unwrap()).unwrap();
        assert!(page.contains("The branch the others are compared with") && page.contains("start"), "{page}");

        // The survey finds the same on all the cores, for every branch but the base
        let mut survey = Survey::start(&root, &b, Some(&src));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while survey.left > 0 && std::time::Instant::now() < deadline {
            survey.poll();
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(survey.left, 0);
        assert!(survey.work[b.base].is_none());
        assert_eq!(survey.work[2].as_ref().unwrap().changes.len(), 2);
        assert!(survey.work[0].as_ref().unwrap().split.is_none());
    }

    #[test]
    fn a_branch_name_never_reaches_gits_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = clone_with_branches(&dir.path().canonicalize().unwrap());
        // Where git, run in the repository, would write; a ref name can't hold the temporary
        // folder's own name, which starts with a dot
        let target = root.join("written");
        run_as(&root, "Ann", T0, &["update-ref", "refs/heads/--output=written", "origin/feature"]);
        let b = branches(&root).unwrap();
        let i = b.list.iter().position(|b| b.name.starts_with("--output=")).expect("listed");
        let base = &b.list[b.base].tip;
        let work = branch_work(&root, base, &b.list[i].tip, None).unwrap();
        let _ = branch_text(&root, &b, i, Some(&work), None, "clone");
        assert!(!target.exists());
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
        // ...and its diff there is a rename, not a whole new file
        let diff = String::from_utf8(file_diff(&root, &file[0], "renamed.txt").unwrap()).unwrap();
        assert!(diff.contains("rename from a.txt") && !diff.contains("+two"), "{diff}");

        // Names git would quote (accents, quote marks) come back as they are
        let odd = "café \"x\".txt";
        run(&root, &["mv", "renamed.txt", odd]);
        run(&root, &["commit", "-q", "-m", "accents"]);
        let file = log(&root, &root.join(odd), true).unwrap();
        assert_eq!(file[0].path.as_deref(), Some(odd));
        assert_eq!(file[1].path.as_deref(), Some("renamed.txt"));
        assert_eq!(show(&root, &file[0].hash, odd).unwrap(), b"two\n");
    }

    #[test]
    fn a_subject_cant_pass_for_a_commit_or_an_option() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        run(&root, &["init", "-q"]);
        fs::write(root.join("a[1].txt"), "one\n").unwrap();
        fs::write(root.join("a1.txt"), "other\n").unwrap();
        run(&root, &["add", "."]);
        let target = root.join("overwritten");
        let forged = format!("innocent\x1e--output={}\x1fabcd\x1f\x1f0\x1flooks normal", target.display());
        run(&root, &["commit", "-q", "-m", &forged]);
        fs::write(root.join("a1.txt"), "changed\n").unwrap();
        run(&root, &["commit", "-q", "-am", "only a1"]);

        let commits = log(&root, &root, false).unwrap();
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["only a1", "innocent"]);
        for c in &commits {
            let _ = commit_text(&root, &c.hash);
            let _ = changes(&root, c);
        }
        assert!(!target.exists());

        // A name with glob characters in it is only itself
        let file = log(&root, &root.join("a[1].txt"), true).unwrap();
        assert_eq!(file.len(), 1);

        // Diffs are git's own, whatever diff program the repository asks for
        run(&root, &["config", "diff.external", "false"]);
        let diff = String::from_utf8(file_diff(&root, &commits[0], "a1.txt").unwrap()).unwrap();
        assert!(diff.contains("-other") && diff.contains("+changed"), "{diff}");
    }
}
