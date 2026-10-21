//! The directory tree in the left pane: one row per visible entry, re-read from
//! disk whenever it's rebuilt.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::SystemTime,
};

use ignore::{WalkBuilder, WalkState};

use crate::{
    archive::{self, Entry},
    cores,
    text::find_all,
};

/// Dependency and build folders, left out of the filter and the search unless hidden
/// files are showing, so big trees stay quick
pub const SKIP_DIRS: [&str; 6] = ["node_modules", "target", ".venv", "venv", "__pycache__", ".git"];

/// An archive's contents, and its modification time when they were read
type Listing = (Option<SystemTime>, Rc<Vec<Entry>>);

/// Most entries the filter looks through
const FILTER_LIMIT: usize = 100_000;

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
}

pub struct Tree {
    pub root: PathBuf,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub scroll: usize,
    /// Show dotfiles and what .gitignore leaves out
    pub show_hidden: bool,
    /// Scroll the selection into view at the next draw, once the height is known
    pub scroll_to_selected: bool,
    /// What's typed in the filter box, while it's open
    pub filter: Option<String>,
    /// Rows marked for moving or deleting, and where a range of them starts
    pub marks: HashSet<PathBuf>,
    pub anchor: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// Everything under the root, read once for the filter
    everything: Option<Vec<(PathBuf, bool)>>,
    /// Most entries the filter looks through
    pub filter_limit: usize,
    /// What's in each archive that's been opened
    archives: RefCell<HashMap<PathBuf, Listing>>,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            show_hidden: false,
            scroll_to_selected: false,
            filter: None,
            marks: HashSet::new(),
            anchor: None,
            expanded: HashSet::new(),
            everything: None,
            filter_limit: FILTER_LIMIT,
            archives: RefCell::default(),
        };
        tree.rebuild();
        tree
    }

    /// Re-reads every expanded directory (or, while filtering, shows the matches),
    /// keeping the selection on the same path.
    pub fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).map(|r| r.path.clone());
        let mut rows = Vec::new();
        match self.filter.clone().filter(|q| !q.trim().is_empty()) {
            Some(query) => self.filtered_into(&query, &mut rows),
            None => self.list_into(&self.root, 0, &mut rows),
        }
        self.rows = rows;
        if let Some(i) = selected.and_then(|p| self.rows.iter().position(|r| r.path == p)) {
            self.selected = i;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        // Only what's on view stays marked, so nothing hidden gets moved or deleted
        let shown: HashSet<&PathBuf> = self.rows.iter().map(|r| &r.path).collect();
        self.marks.retain(|p| shown.contains(p));
    }

    pub fn toggle_mark(&mut self, i: usize) {
        let Some(row) = self.rows.get(i) else { return };
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
        self.marks = self.rows[from..=to].iter().map(|r| r.path.clone()).collect();
    }

    /// What a move or delete applies to: the marked rows in tree order, or else the
    /// selected one. A marked folder takes everything in it along, so marked rows inside
    /// it aren't listed again (they'd be gone by the time their turn came).
    pub fn marked_or_selected(&self) -> Vec<PathBuf> {
        if self.marks.is_empty() {
            return self.rows.get(self.selected).map(|r| r.path.clone()).into_iter().collect();
        }
        self.rows
            .iter()
            .filter(|r| self.marks.contains(&r.path) && !r.path.ancestors().skip(1).any(|a| self.marks.contains(a)))
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
        for (path, is_dir) in list_dir(dir, self.show_hidden) {
            let name =
                path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let archive = !is_dir && archive::is_archive(&path);
            let open = (is_dir || archive) && self.expanded.contains(&path);
            let repo = is_dir && path.join(".git").exists();
            rows.push(Row { path: path.clone(), name, is_dir: is_dir || archive, depth, archive, repo });
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
            rows.push(Row { path, name, is_dir: entry.is_dir, depth, archive: false, repo: false });
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
        let entries = Rc::new(archive::list(archive).unwrap_or_default());
        cache.insert(archive.to_path_buf(), (modified, entries.clone()));
        entries
    }

    /// Every file under the root whose path holds all the words of `query`, with the
    /// directories leading to them.
    fn filtered_into(&mut self, query: &str, rows: &mut Vec<Row>) {
        let (root, all, limit) = (&self.root, self.show_hidden, self.filter_limit);
        let everything = self.everything.get_or_insert_with(|| walk_all(root, all, limit));
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut keep: HashSet<&Path> = HashSet::new();
        for (path, _) in everything.iter() {
            let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
            if words.iter().all(|w| !find_all(&rel, w).is_empty()) {
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
        fn add(dir: &Path, depth: usize, children: &mut HashMap<&Path, Vec<(&Path, bool)>>, rows: &mut Vec<Row>) {
            let Some(mut entries) = children.remove(dir) else { return };
            entries
                .sort_by_cached_key(|(p, is_dir)| (!is_dir, p.file_name().map(|n| n.to_string_lossy().to_lowercase())));
            for (path, is_dir) in entries {
                let name =
                    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                let archive = !is_dir && archive::is_archive(path);
                let repo = is_dir && path.join(".git").exists();
                rows.push(Row { path: path.to_path_buf(), name, is_dir: is_dir || archive, depth, archive, repo });
                if is_dir {
                    add(path, depth + 1, children, rows);
                }
            }
        }
        add(root, 0, &mut children, rows);
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

    /// Reads everything from disk again.
    pub fn refresh(&mut self) {
        self.everything = None;
        self.archives.borrow_mut().clear();
        self.rebuild();
    }

    /// Whether a row is unfolded. While filtering, directories always are, but
    /// archives aren't looked into.
    pub fn is_open(&self, row: &Row) -> bool {
        if self.filtering() { !row.archive && row.is_dir } else { self.expanded.contains(&row.path) }
    }

    pub fn toggle(&mut self, i: usize) {
        let Some(row) = self.rows.get(i).filter(|r| r.is_dir) else { return };
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
fn walk_all(root: &Path, all: bool, limit: usize) -> Vec<(PathBuf, bool)> {
    let found = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    WalkBuilder::new(root)
        .standard_filters(!all)
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

/// A directory's entries, directories first, honoring .gitignore unless `all`.
fn list_dir(dir: &Path, all: bool) -> Vec<(PathBuf, bool)> {
    let mut entries: Vec<(PathBuf, bool)> = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .standard_filters(!all)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.depth() == 1)
        .map(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir()) || (e.path_is_symlink() && e.path().is_dir());
            (e.into_path(), is_dir)
        })
        .collect();
    entries
        .sort_by_cached_key(|(path, is_dir)| (!is_dir, path.file_name().map(|n| n.to_string_lossy().to_lowercase())));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
        assert_eq!(names, vec!["src", "A.md", "b.md"]);

        tree.reveal(&root.join("src/main.rs"));
        let names: Vec<(&str, usize)> = tree.rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(names, vec![("src", 0), ("main.rs", 1), ("A.md", 0), ("b.md", 0)]);
        assert_eq!(tree.selected, 1);

        tree.collapse_or_parent();
        assert_eq!(tree.selected, 0);

        tree.show_hidden = true;
        tree.rebuild();
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec![".git", "src", "main.rs", "target", ".gitignore", "A.md", "b.md"]);

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
        tree.marks = tree.rows.iter().map(|r| r.path.clone()).collect();
        assert_eq!(tree.marked_or_selected(), vec![root.join("docs"), root.join("z.txt")]);
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
