//! The directory tree in the left pane: one row per visible entry, re-read from
//! disk whenever it's rebuilt.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use ignore::{WalkBuilder, WalkState};

use crate::{
    archive::{self, Entry},
    cores,
    text::finds,
};

/// Dependency and build folders, left out of the filter and the search unless hidden
/// files are showing, so big trees stay quick
pub const SKIP_DIRS: [&str; 6] = ["node_modules", "target", ".venv", "venv", "__pycache__", ".git"];

/// An archive's contents, and its modification time when they were read
type Listing = (Option<SystemTime>, Rc<Vec<Entry>>);

/// Most entries the filter looks through
const FILTER_LIMIT: usize = 100_000;

/// The order of what's in a folder, folders always first.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Sort {
    /// By name, whatever the case
    #[default]
    Name,
    /// Biggest first; folders by name
    Size,
    /// Oldest first
    Oldest,
    /// Newest first
    Newest,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Name => Sort::Size,
            Sort::Size => Sort::Oldest,
            Sort::Oldest => Sort::Newest,
            Sort::Newest => Sort::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "by name",
            Sort::Size => "biggest first",
            Sort::Oldest => "oldest first",
            Sort::Newest => "newest first",
        }
    }
}

/// Where an entry goes in a folder: folders first, then by `rank`, its place in the
/// sort's order, and by name among equals.
fn order(path: &Path, is_dir: bool, rank: i128) -> (bool, i128, String) {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    (!is_dir, rank, name)
}

/// An entry's place in `sort`'s order, by its size or when it changed: the same for all
/// of them by name, and for folders by size.
fn rank(path: &Path, is_dir: bool, sort: Sort) -> i128 {
    let meta = || fs::metadata(path).or_else(|_| fs::symlink_metadata(path)).ok();
    let modified = || {
        let time = meta().and_then(|m| m.modified().ok());
        time.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i128)
    };
    match sort {
        Sort::Name => 0,
        Sort::Size if is_dir => 0,
        Sort::Size => -i128::from(meta().map_or(0, |m| m.len())),
        Sort::Oldest => modified(),
        Sort::Newest => -modified(),
    }
}

/// Why the root couldn't move up a folder.
pub enum NoWayUp {
    /// It's at the top already
    Top,
    /// The folder above can't be listed, so there'd be no way back down from it
    Unreadable(PathBuf, io::ErrorKind),
}

pub struct Row {
    /// For something inside an archive, the archive's path joined with the path within it
    pub path: PathBuf,
    pub name: String,
    /// Folds open: a directory, an archive, or a directory inside an archive
    pub is_dir: bool,
    pub depth: usize,
    pub archive: bool,
    /// A folder that's a git repository
    pub repo: bool,
    /// The ".." row at the top, for the folder above the root
    pub up: bool,
}

pub struct Tree {
    pub root: PathBuf,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub scroll: usize,
    /// Show dotfiles and what .gitignore leaves out
    pub show_hidden: bool,
    /// Whether the ignore rules of the folders above the root apply. Not when they leave
    /// out the root itself, as a home folder kept in git with `*` in its .gitignore does
    pub rules_above: bool,
    /// How many entries at the top the hidden-file and ignore rules leave out
    pub hidden: usize,
    /// Why there's nothing at the top, when the root can't be read
    pub unreadable: Option<io::ErrorKind>,
    /// Everything at the top, counted for `hidden`, with the root and its modification
    /// time then: counted again only when it changes, not at every refresh
    counted: Option<(PathBuf, Option<SystemTime>, usize)>,
    /// Scroll the selection into view at the next draw, once the height is known
    pub scroll_to_selected: bool,
    /// What's typed in the filter box, while it's open
    pub filter: Option<String>,
    /// The order of what's in each folder
    pub sort: Sort,
    /// Rows marked for moving or deleting, and where a range of them starts
    pub marks: HashSet<PathBuf>,
    pub anchor: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// Everything under the root, read once for the filter
    everything: Option<Vec<(PathBuf, bool)>>,
    /// The places in the order sorted by of what the filter has shown from `everything`,
    /// and that order: read once each, like `everything`, as the filter sorts its matches
    /// again at every key typed
    ranks: (Sort, HashMap<PathBuf, i128>),
    /// Most entries the filter looks through
    pub filter_limit: usize,
    /// What's in each archive that's been opened
    archives: RefCell<HashMap<PathBuf, Listing>>,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            rules_above: !ignored_from_above(&root),
            root,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            show_hidden: false,
            hidden: 0,
            unreadable: None,
            counted: None,
            scroll_to_selected: false,
            filter: None,
            sort: Sort::Name,
            marks: HashSet::new(),
            anchor: None,
            expanded: HashSet::new(),
            everything: None,
            ranks: (Sort::Name, HashMap::new()),
            filter_limit: FILTER_LIMIT,
            archives: RefCell::default(),
        };
        tree.rebuild();
        // The first entry rather than ".."
        tree.selected = tree.rows.iter().position(|r| !r.up).unwrap_or(0);
        tree
    }

    /// Re-reads every expanded directory (or, while filtering, shows the matches),
    /// keeping the selection on the same path.
    pub fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).map(|r| r.path.clone());
        let mut rows = Vec::new();
        match self.filter.clone().filter(|q| !q.trim().is_empty()) {
            Some(query) => self.filtered_into(&query, &mut rows),
            None => {
                if let Some(parent) = self.root.parent() {
                    let path = parent.to_path_buf();
                    rows.push(Row {
                        path,
                        name: "..".into(),
                        is_dir: true,
                        depth: 0,
                        archive: false,
                        repo: false,
                        up: true,
                    });
                }
                self.list_into(&self.root, 0, &mut rows);
                let shown = rows.iter().filter(|r| r.depth == 0 && !r.up).count();
                self.unreadable = if shown == 0 { fs::read_dir(&self.root).err().map(|e| e.kind()) } else { None };
                self.hidden = if self.show_hidden { 0 } else { self.count_top().saturating_sub(shown) };
            }
        }
        self.rows = rows;
        if let Some(i) = selected.and_then(|p| self.rows.iter().position(|r| r.path == p)) {
            self.selected = i;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        // Only what's on view stays marked, so nothing hidden gets moved or deleted
        if !self.marks.is_empty() {
            let shown: HashSet<&PathBuf> = self.rows.iter().filter(|r| !r.up).map(|r| &r.path).collect();
            self.marks.retain(|p| shown.contains(p));
        }
    }

    /// The selected row, unless it's "..".
    pub fn selected_entry(&self) -> Option<&Row> {
        self.rows.get(self.selected).filter(|r| !r.up)
    }

    /// How many entries the root has, hidden or not.
    fn count_top(&mut self) -> usize {
        let modified = fs::metadata(&self.root).and_then(|m| m.modified()).ok();
        if let Some((root, when, total)) = &self.counted
            && *root == self.root
            && modified.is_some()
            && *when == modified
        {
            return *total;
        }
        let total = fs::read_dir(&self.root).map_or(0, Iterator::count);
        self.counted = Some((self.root.clone(), modified, total));
        total
    }

    pub fn toggle_mark(&mut self, i: usize) {
        let Some(row) = self.rows.get(i).filter(|r| !r.up) else { return };
        if !self.marks.remove(&row.path) {
            self.marks.insert(row.path.clone());
        }
        self.anchor = Some(row.path.clone());
    }

    /// Marks the rows from the anchor to the selection.
    pub fn mark_range(&mut self) {
        let Some(row) = self.rows.get(self.selected) else { return };
        let anchor = self.anchor.get_or_insert_with(|| row.path.clone()).clone();
        let start = self.rows.iter().position(|r| r.path == anchor).unwrap_or(self.selected);
        let (from, to) = (start.min(self.selected), start.max(self.selected));
        self.marks = self.rows[from..=to].iter().filter(|r| !r.up).map(|r| r.path.clone()).collect();
    }

    /// What a move or delete applies to: the marked rows in tree order, or else the
    /// selected one, never "..". A marked folder takes everything in it along, so marked
    /// rows inside it aren't listed again (they'd be gone by the time their turn came).
    pub fn marked_or_selected(&self) -> Vec<PathBuf> {
        if self.marks.is_empty() {
            return self.selected_entry().map(|r| r.path.clone()).into_iter().collect();
        }
        self.rows
            .iter()
            .filter(|r| {
                !r.up && self.marks.contains(&r.path) && !r.path.ancestors().skip(1).any(|a| self.marks.contains(a))
            })
            .map(|r| r.path.clone())
            .collect()
    }

    /// Carries open folders and marks over to where something was moved or renamed.
    pub fn renamed(&mut self, from: &Path, to: &Path) {
        let moved = |set: &mut HashSet<PathBuf>| {
            let paths: Vec<PathBuf> = set.iter().filter(|p| p.starts_with(from)).cloned().collect();
            for path in paths {
                set.remove(&path);
                set.insert(to.join(path.strip_prefix(from).unwrap_or(Path::new(""))));
            }
        };
        moved(&mut self.expanded);
        moved(&mut self.marks);
    }

    fn list_into(&self, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        let top = (!self.rules_above).then_some(self.root.as_path());
        for (path, is_dir) in list_dir(dir, self.show_hidden, top, self.sort) {
            let name =
                path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let archive = !is_dir && archive::is_archive(&path);
            let open = (is_dir || archive) && self.expanded.contains(&path);
            let repo = is_dir && path.join(".git").exists();
            rows.push(Row { path: path.clone(), name, is_dir: is_dir || archive, depth, archive, repo, up: false });
            if open && archive {
                self.list_archive(&path, "", depth + 1, rows);
            } else if open {
                self.list_into(&path, depth + 1, rows);
            }
        }
    }

    /// What's in an archive directly under `prefix` ("" for its top level).
    fn list_archive(&self, archive: &Path, prefix: &str, depth: usize, rows: &mut Vec<Row>) {
        let entries = self.archive_entries(archive);
        let parent = |p: &str| p.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
        let mut children: Vec<&Entry> = entries.iter().filter(|e| parent(&e.path) == prefix).collect();
        children.sort_by_cached_key(|e| (!e.is_dir, e.path.to_lowercase()));
        for entry in children {
            let path = archive.join(&entry.path);
            let name = entry.path.rsplit('/').next().unwrap_or(&entry.path).to_string();
            let open = entry.is_dir && self.expanded.contains(&path);
            rows.push(Row { path, name, is_dir: entry.is_dir, depth, archive: false, repo: false, up: false });
            if open {
                self.list_archive(archive, &entry.path, depth + 1, rows);
            }
        }
    }

    fn archive_entries(&self, archive: &Path) -> Rc<Vec<Entry>> {
        let modified = fs::metadata(archive).and_then(|m| m.modified()).ok();
        let mut cache = self.archives.borrow_mut();
        if let Some((_, entries)) = cache.get(archive).filter(|(when, _)| *when == modified) {
            return entries.clone();
        }
        // One that can't be read says why, rather than looking empty; opening the row
        // says it again
        let entries = Rc::new(archive::list(archive).unwrap_or_else(|e| {
            let why = format!("… can't read it: {e:#}").replace('/', "∕");
            vec![Entry { path: why, is_dir: false }]
        }));
        cache.insert(archive.to_path_buf(), (modified, entries.clone()));
        entries
    }

    /// Every file under the root whose path holds all the words of `query`, with the
    /// directories leading to them.
    fn filtered_into(&mut self, query: &str, rows: &mut Vec<Row>) {
        let (root, all, above, limit, sort) =
            (&self.root, self.show_hidden, self.rules_above, self.filter_limit, self.sort);
        // Read again, or sorted another way, the places are worked out again
        if self.everything.is_none() || self.ranks.0 != sort {
            self.ranks = (sort, HashMap::new());
        }
        let ranks = &mut self.ranks.1;
        let everything = self.everything.get_or_insert_with(|| walk_all(root, all, above, limit));
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut keep: HashSet<&Path> = HashSet::new();
        for (path, _) in everything.iter() {
            let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
            if words.iter().all(|w| finds(&rel, w)) {
                for dir in path.ancestors() {
                    if dir == root || !keep.insert(dir) {
                        break;
                    }
                }
            }
        }
        let mut children: HashMap<&Path, Vec<(&Path, bool)>> = HashMap::new();
        for (path, is_dir) in everything.iter() {
            if let (true, Some(parent)) = (keep.contains(path.as_path()), path.parent()) {
                children.entry(parent).or_default().push((path, *is_dir));
            }
        }
        fn add(
            dir: &Path,
            depth: usize,
            sort: Sort,
            ranks: &mut HashMap<PathBuf, i128>,
            children: &mut HashMap<&Path, Vec<(&Path, bool)>>,
            rows: &mut Vec<Row>,
        ) {
            let Some(mut entries) = children.remove(dir) else { return };
            entries.sort_by_cached_key(|(p, is_dir)| {
                let place = match sort {
                    Sort::Name => 0,
                    _ => *ranks.entry(p.to_path_buf()).or_insert_with(|| rank(p, *is_dir, sort)),
                };
                order(p, *is_dir, place)
            });
            for (path, is_dir) in entries {
                let name =
                    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                let archive = !is_dir && archive::is_archive(path);
                let repo = is_dir && path.join(".git").exists();
                let is_dir = is_dir || archive;
                rows.push(Row { path: path.to_path_buf(), name, is_dir, depth, archive, repo, up: false });
                if is_dir {
                    add(path, depth + 1, sort, ranks, children, rows);
                }
            }
        }
        add(root, 0, sort, ranks, &mut children, rows);
    }

    pub fn filtering(&self) -> bool {
        self.filter.as_deref().is_some_and(|q| !q.trim().is_empty())
    }

    /// Whether the filter stopped at its limit, so it looked through only part of the tree.
    pub fn filter_cut_short(&self) -> bool {
        self.filtering() && self.everything.as_ref().is_some_and(|e| e.len() >= self.filter_limit)
    }

    /// Opens, changes or (with None) closes the filter. Closing it leaves the tree
    /// opened down to whatever was selected.
    pub fn set_filter(&mut self, filter: Option<String>) {
        let closing = filter.is_none() && self.filter.is_some();
        self.filter = filter;
        let selected = self.rows.get(self.selected).map(|r| r.path.clone());
        if closing {
            self.everything = None;
            if let Some(path) = selected {
                return self.reveal(&path);
            }
        }
        self.rebuild();
    }

    /// Puts what's in each folder in `sort`'s order, the selection kept in view.
    pub fn set_sort(&mut self, sort: Sort) {
        self.sort = sort;
        self.rebuild();
        self.scroll_to_selected = true;
    }

    /// Reads everything from disk again.
    pub fn refresh(&mut self) {
        self.everything = None;
        self.archives.borrow_mut().clear();
        self.rebuild();
    }

    /// Whether a row is unfolded. While filtering, directories always are, but
    /// archives aren't looked into.
    pub fn is_open(&self, row: &Row) -> bool {
        if self.filtering() { !row.archive && row.is_dir } else { !row.up && self.expanded.contains(&row.path) }
    }

    pub fn toggle(&mut self, i: usize) {
        let Some(row) = self.rows.get(i).filter(|r| r.is_dir && !r.up) else { return };
        if !self.expanded.remove(&row.path) {
            self.expanded.insert(row.path.clone());
        }
        self.rebuild();
    }

    /// Folds the selected directory, or moves up to its parent.
    pub fn collapse_or_parent(&mut self) {
        let Some(row) = self.rows.get(self.selected) else { return };
        if row.is_dir && self.is_open(row) && !self.filtering() {
            self.toggle(self.selected);
        } else if row.depth > 0 {
            let depth = row.depth;
            if let Some(parent) = self.rows[..self.selected].iter().rposition(|r| r.depth < depth) {
                self.selected = parent;
            }
        }
    }

    /// Makes the folder above the root the root, with the old one open and selected. If
    /// the old one is hidden there (a dotfolder, or one .gitignore leaves out), hidden
    /// files show, so it doesn't vanish from under the selection.
    pub fn up(&mut self) -> Result<(), NoWayUp> {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else { return Err(NoWayUp::Top) };
        if let Err(e) = fs::read_dir(&parent) {
            return Err(NoWayUp::Unreadable(parent, e.kind()));
        }
        let below = std::mem::replace(&mut self.root, parent);
        self.rules_above = !ignored_from_above(&self.root);
        self.everything = None;
        self.expanded.insert(below.clone());
        self.reveal(&below);
        if !self.show_hidden && self.rows.get(self.selected).is_none_or(|r| r.path != below) {
            self.show_hidden = true;
            self.refresh();
            self.reveal(&below);
        }
        Ok(())
    }

    /// Opens the directories down to `path` and selects it.
    pub fn reveal(&mut self, path: &Path) {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            let mut dir = self.root.clone();
            for part in rel.parent().into_iter().flat_map(Path::components) {
                dir.push(part);
                self.expanded.insert(dir.clone());
            }
        }
        self.rebuild();
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.selected = i;
        }
        self.scroll_to_selected = true;
    }

    /// Scrolls so the selected row is on screen.
    pub fn keep_visible(&mut self, height: usize) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if height > 0 && self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
    }
}

/// Everything under `root`, for the filter, read on all the cores. Unless `all`,
/// this leaves out what the tree hides, and SKIP_DIRS too.
fn walk_all(root: &Path, all: bool, rules_above: bool, limit: usize) -> Vec<(PathBuf, bool)> {
    let found = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    WalkBuilder::new(root)
        .standard_filters(!all)
        .parents(!all && rules_above)
        .filter_entry(move |e| all || e.depth() == 0 || !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        .threads(cores())
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else { return WalkState::Continue };
                if entry.depth() == 0 {
                    return WalkState::Continue;
                }
                if count.fetch_add(1, Ordering::Relaxed) >= limit {
                    return WalkState::Quit;
                }
                let is_dir =
                    entry.file_type().is_some_and(|t| t.is_dir()) || (entry.path_is_symlink() && entry.path().is_dir());
                if let Ok(mut found) = found.lock() {
                    found.push((entry.into_path(), is_dir));
                }
                WalkState::Continue
            })
        });
    found.into_inner().unwrap_or_default()
}

/// Whether the ignore rules of the folders above `dir` leave it out, as a home folder
/// kept in git with `*` in its .gitignore does to everything in it.
fn ignored_from_above(dir: &Path) -> bool {
    let Some(parent) = dir.parent() else { return false };
    // Only a repository or an .ignore file above has rules for it
    if !parent.ancestors().any(|a| a.join(".git").exists() || a.join(".ignore").exists()) {
        return false;
    }
    // A folder above that can't be listed can't say, and the walk would find nothing
    if fs::read_dir(parent).is_err() {
        return false;
    }
    let wanted = dir.to_path_buf();
    // A dotfile isn't left out by a rule
    !WalkBuilder::new(parent)
        .max_depth(Some(1))
        .hidden(false)
        .filter_entry(move |e| e.depth() == 0 || e.path() == wanted)
        .build()
        .filter_map(Result::ok)
        .any(|e| e.depth() == 1)
}

/// A directory's entries, directories first and then in `sort`'s order, honoring
/// .gitignore unless `all`. Given `top`, a folder that `dir` is in, only the rules from
/// `top` down apply.
fn list_dir(dir: &Path, all: bool, top: Option<&Path>, sort: Sort) -> Vec<(PathBuf, bool)> {
    // The rules are picked up on the way down, so the walk starts at `top`, though it looks
    // into nothing but the folders on the way to `dir`. It doesn't go through symlinks, so
    // beyond one, only the rules from `dir` down apply.
    let from = top
        .filter(|top| {
            dir.strip_prefix(top)
                .is_ok_and(|rel| rel.ancestors().all(|a| a.as_os_str().is_empty() || !top.join(a).is_symlink()))
        })
        .unwrap_or(dir);
    let depth = dir.strip_prefix(from).map_or(0, |rel| rel.components().count());
    let mut walk = WalkBuilder::new(from);
    walk.max_depth(Some(depth + 1)).standard_filters(!all);
    if top.is_some() {
        walk.parents(false);
    }
    if depth > 0 {
        let dir = dir.to_path_buf();
        walk.filter_entry(move |e| dir.starts_with(e.path()) || e.path().parent() == Some(dir.as_path()));
    }
    let mut entries: Vec<(PathBuf, bool)> = walk
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.depth() == depth + 1)
        .map(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir()) || (e.path_is_symlink() && e.path().is_dir());
            (e.into_path(), is_dir)
        })
        .collect();
    entries.sort_by_cached_key(|(path, is_dir)| order(path, *is_dir, rank(path, *is_dir, sort)));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sorts_by_name_size_and_date_with_folders_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let day = std::time::Duration::from_secs(86_400);
        let now = SystemTime::now();
        // Name, size, and days old
        for (name, size, age) in [("b.txt", 300, 2), ("A.txt", 100, 1), ("c.txt", 200, 3)] {
            let file = fs::File::create(root.join(name)).unwrap();
            file.set_len(size).unwrap();
            file.set_modified(now - day * age).unwrap();
        }
        for (name, age) in [("old", 5), ("new", 4)] {
            fs::create_dir(root.join(name)).unwrap();
            fs::File::open(root.join(name)).unwrap().set_modified(now - day * age).unwrap();
        }
        let mut tree = Tree::new(root.to_path_buf());
        let names = |tree: &Tree| tree.rows.iter().filter(|r| !r.up).map(|r| r.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&tree), ["new", "old", "A.txt", "b.txt", "c.txt"]);
        tree.set_sort(tree.sort.next());
        assert_eq!(tree.sort, Sort::Size);
        assert_eq!(names(&tree), ["new", "old", "b.txt", "c.txt", "A.txt"]);
        tree.set_sort(tree.sort.next());
        assert_eq!(names(&tree), ["old", "new", "c.txt", "b.txt", "A.txt"]);
        tree.set_sort(tree.sort.next());
        assert_eq!(names(&tree), ["new", "old", "A.txt", "b.txt", "c.txt"]);
        assert_eq!(tree.sort, Sort::Newest);
        // The filter's matches go in the same order
        tree.set_filter(Some("txt".into()));
        assert_eq!(names(&tree), ["A.txt", "b.txt", "c.txt"]);
        tree.set_sort(Sort::Oldest);
        assert_eq!(names(&tree), ["c.txt", "b.txt", "A.txt"]);
        assert_eq!(tree.sort.next(), Sort::Newest);
        // read once, like what the filter looks through, until that's read again
        tree.set_sort(Sort::Size);
        assert_eq!(names(&tree), ["b.txt", "c.txt", "A.txt"]);
        fs::File::options().write(true).open(root.join("A.txt")).unwrap().set_len(1000).unwrap();
        tree.rebuild();
        assert_eq!(names(&tree), ["b.txt", "c.txt", "A.txt"]);
        tree.refresh();
        assert_eq!(names(&tree), ["A.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn lists_dirs_first_and_skips_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("b.md"), "").unwrap();
        fs::write(root.join("A.md"), "").unwrap();

        let mut tree = Tree::new(root.to_path_buf());
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["..", "src", "A.md", "b.md"]);
        assert_eq!(tree.selected, 1);
        // .git, .gitignore and target
        assert_eq!(tree.hidden, 3);

        tree.reveal(&root.join("src/main.rs"));
        let names: Vec<(&str, usize)> = tree.rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(names, vec![("..", 0), ("src", 0), ("main.rs", 1), ("A.md", 0), ("b.md", 0)]);
        assert_eq!(tree.selected, 2);

        tree.collapse_or_parent();
        assert_eq!(tree.selected, 1);

        tree.show_hidden = true;
        tree.rebuild();
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["..", ".git", "src", "main.rs", "target", ".gitignore", "A.md", "b.md"]);
        assert_eq!(tree.hidden, 0);

        tree.collapse_or_parent();
        assert!(tree.rows.iter().all(|r| r.name != "main.rs"));
    }

    #[test]
    fn a_marked_folder_takes_its_marked_contents_along() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("docs")).unwrap();
        fs::write(root.join("docs/a.md"), "").unwrap();
        fs::write(root.join("z.txt"), "").unwrap();
        let mut tree = Tree::new(root.to_path_buf());
        tree.reveal(&root.join("docs/a.md"));
        tree.marks = tree.rows.iter().filter(|r| !r.up).map(|r| r.path.clone()).collect();
        assert_eq!(tree.marked_or_selected(), vec![root.join("docs"), root.join("z.txt")]);
    }

    #[test]
    fn up_moves_the_root_and_dot_dot_is_never_changed() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join("a/b")).unwrap();
        fs::write(top.join("a/b/inner.txt"), "").unwrap();
        fs::write(top.join("a/beside.txt"), "").unwrap();
        let mut tree = Tree::new(top.join("a/b"));
        assert!(tree.up().is_ok());
        assert_eq!(tree.root, top.join("a"));
        let rows: Vec<(&str, usize)> = tree.rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(rows, vec![("..", 0), ("b", 0), ("inner.txt", 1), ("beside.txt", 0)]);
        // The folder it came from is still open, and selected
        assert_eq!(tree.rows[tree.selected].name, "b");

        // ".." can't be marked, or picked for a move or delete
        tree.selected = 0;
        assert!(tree.marked_or_selected().is_empty());
        tree.toggle_mark(0);
        assert!(tree.marks.is_empty());
        tree.selected = 3;
        tree.anchor = Some(top.to_path_buf());
        tree.mark_range();
        assert!(!tree.marks.contains(top) && tree.marks.len() == 3);
        tree.toggle(0);
        assert_eq!(tree.root, top.join("a"));

        // At the very top there's no ".."
        let mut tree = Tree::new(PathBuf::from("/"));
        assert!(tree.rows.iter().all(|r| !r.up));
        assert!(matches!(tree.up(), Err(NoWayUp::Top)));
    }

    #[test]
    fn up_keeps_a_way_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A folder that can be gone through but not listed, as home folders often are
        let locked = dir.path().join("home");
        let inside = locked.join("public");
        fs::create_dir_all(&inside).unwrap();
        fs::write(inside.join("index.html"), "").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o311)).unwrap();
        let mut tree = Tree::new(inside.clone());
        let went = tree.up();
        let listable = fs::read_dir(&locked).is_ok();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        // Unless the tests run as root, which can list anything
        if !listable {
            assert!(matches!(went, Err(NoWayUp::Unreadable(..))));
            assert_eq!(tree.root, inside);
        }

        // Up from a dotfolder, hidden files show so it's still there
        let dot = dir.path().join("top/.config");
        fs::create_dir_all(&dot).unwrap();
        fs::write(dot.join("settings.toml"), "").unwrap();
        let mut tree = Tree::new(dot.clone());
        assert!(tree.up().is_ok());
        assert!(tree.show_hidden);
        assert_eq!(tree.rows[tree.selected].path, dot);
    }

    #[test]
    fn a_folder_that_rules_above_leave_out_still_shows() {
        // A home folder kept in git, ignoring everything it doesn't track
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::create_dir(home.join(".git")).unwrap();
        fs::write(home.join(".gitignore"), "*\n").unwrap();
        let root = home.join("notes");
        for path in ["todo.md", "sub/deep.txt", "sub/scratch.tmp", "repo/keep.rs", "repo/build.log"] {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), "").unwrap();
        }
        // Rules inside the root still count, from the root down
        fs::write(root.join(".ignore"), "*.tmp\n").unwrap();
        fs::create_dir(root.join("repo/.git")).unwrap();
        fs::write(root.join("repo/.gitignore"), "*.log\n").unwrap();

        let mut tree = Tree::new(root.clone());
        assert!(!tree.rules_above);
        tree.reveal(&root.join("sub/deep.txt"));
        tree.reveal(&root.join("repo/keep.rs"));
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["..", "repo", "keep.rs", "sub", "deep.txt", "todo.md"]);
        tree.set_filter(Some("e".into()));
        let names: Vec<&str> = tree.rows.iter().filter(|r| !r.is_dir).map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["keep.rs", "deep.txt"]);
        tree.set_filter(None);

        // Up in the home folder its rules apply again, so the folder that was the root would
        // be left out, and hidden files show to keep it in view
        assert!(tree.up().is_ok());
        assert!(tree.rules_above);
        assert!(tree.show_hidden);
        assert_eq!(tree.rows[tree.selected].path, root);
        assert_eq!(tree.unreadable, None);
        // Hiding them again, the header says how much the rules leave out
        tree.show_hidden = false;
        tree.refresh();
        assert!(tree.rows.iter().all(|r| r.up));
        assert_eq!(tree.hidden, 3);
    }

    #[test]
    fn a_folder_that_cant_be_read_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("locked");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("secret.txt"), "").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        let tree = Tree::new(root.clone());
        let readable = fs::read_dir(&root).is_ok();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        // Unless the tests run as root, which can read anything
        if !readable {
            assert_eq!(tree.unreadable, Some(io::ErrorKind::PermissionDenied));
        }
    }

    #[test]
    fn filter_shows_matches_with_their_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in ["src/app.rs", "src/deep/app_test.rs", "src/tree.rs", "docs/app.md", "notes.txt"] {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), "").unwrap();
        }
        let mut tree = Tree::new(root.to_path_buf());
        tree.set_filter(Some("app".into()));
        let rows: Vec<(&str, usize)> = tree.rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(rows, vec![("docs", 0), ("app.md", 1), ("src", 0), ("deep", 1), ("app_test.rs", 2), ("app.rs", 1)]);
        // Every word has to match somewhere in the path
        tree.set_filter(Some("src app".into()));
        assert!(tree.rows.iter().all(|r| r.name != "app.md"));
        tree.set_filter(Some("zzz".into()));
        assert!(tree.rows.is_empty());
        assert!(!tree.filter_cut_short());
        // Past its limit, it says it looked through only part of the tree
        tree.filter_limit = 3;
        tree.refresh();
        assert!(tree.filter_cut_short());
        tree.filter_limit = FILTER_LIMIT;
        tree.refresh();

        // Closing it opens the tree down to the selection
        tree.set_filter(Some("test".into()));
        tree.selected = tree.rows.iter().position(|r| r.name == "app_test.rs").unwrap();
        tree.set_filter(None);
        assert_eq!(tree.rows[tree.selected].name, "app_test.rs");
        assert!(tree.rows.iter().any(|r| r.name == "notes.txt"));
    }
}
