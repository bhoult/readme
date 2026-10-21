//! The directory tree in the left pane: one row per visible entry, re-read from
//! disk whenever it's rebuilt.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::SystemTime,
};

use ignore::WalkBuilder;

use crate::{
    archive::{self, Entry},
    text::find_all,
};

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
    expanded: HashSet<PathBuf>,
    /// Everything under the root, read once for the filter
    everything: Option<Vec<(PathBuf, bool)>>,
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
            expanded: HashSet::new(),
            everything: None,
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
    }

    fn list_into(&self, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        for (path, is_dir) in list_dir(dir, self.show_hidden) {
            let name =
                path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let archive = !is_dir && archive::is_archive(&path);
            let open = (is_dir || archive) && self.expanded.contains(&path);
            rows.push(Row { path: path.clone(), name, is_dir: is_dir || archive, depth, archive });
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
            rows.push(Row { path, name, is_dir: entry.is_dir, depth, archive: false });
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
        let (root, all) = (&self.root, self.show_hidden);
        let everything = self.everything.get_or_insert_with(|| walk_all(root, all));
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
                rows.push(Row { path: path.to_path_buf(), name, is_dir: is_dir || archive, depth, archive });
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

/// Everything under `root`, for the filter. Unless `all`, this leaves out what the
/// tree hides, and dependency and build directories too, so big trees stay quick.
fn walk_all(root: &Path, all: bool) -> Vec<(PathBuf, bool)> {
    const SKIP: [&str; 6] = ["node_modules", "target", ".venv", "venv", "__pycache__", ".git"];
    WalkBuilder::new(root)
        .standard_filters(!all)
        .filter_entry(move |e| all || e.depth() == 0 || !SKIP.contains(&e.file_name().to_string_lossy().as_ref()))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.depth() > 0)
        .take(FILTER_LIMIT)
        .map(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir()) || (e.path_is_symlink() && e.path().is_dir());
            (e.into_path(), is_dir)
        })
        .collect()
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

        // Closing it opens the tree down to the selection
        tree.set_filter(Some("test".into()));
        tree.selected = tree.rows.iter().position(|r| r.name == "app_test.rs").unwrap();
        tree.set_filter(None);
        assert_eq!(tree.rows[tree.selected].name, "app_test.rs");
        assert!(tree.rows.iter().any(|r| r.name == "notes.txt"));
    }
}
