//! Searching the text of every file under a folder, on all the cores. Results arrive
//! as they're found, kept in path order.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use ignore::{WalkBuilder, WalkState};

use crate::{cores, text::find_all, tree::SKIP_DIRS};

/// Bigger files aren't searched
const MAX_FILE: u64 = 10 << 20;
/// The search stops after this many matching lines
pub const MAX_MATCHES: usize = 10_000;
const PER_FILE: usize = 500;
/// How much of a file is checked for NUL bytes to tell binary from text
const SNIFF: usize = 8000;

/// A matching line: its number (from 0), where the match starts (in chars), and the line.
pub struct Hit {
    pub line: usize,
    pub col: usize,
    pub text: String,
}

pub struct FileHits {
    pub path: PathBuf,
    pub hits: Vec<Hit>,
}

pub struct Grep {
    pub query: String,
    pub files: Vec<FileHits>,
    pub done: bool,
    pub searched: Arc<AtomicUsize>,
    matches: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    found: mpsc::Receiver<Option<FileHits>>,
    /// The selected match: which file, and which of its lines
    pub selected: (usize, usize),
    pub scroll: usize,
}

impl Grep {
    /// Starts searching for `query` under `root`. With `all`, hidden and ignored files
    /// are searched too.
    pub fn start(root: &Path, query: &str, all: bool) -> Self {
        let (sender, found) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let searched = Arc::new(AtomicUsize::new(0));
        let matches = Arc::new(AtomicUsize::new(0));
        let (root, query_owned) = (root.to_path_buf(), query.to_string());
        let (stop_walk, searched_walk, matches_walk) = (stop.clone(), searched.clone(), matches.clone());
        thread::spawn(move || {
            // Case matters only when the query has a capital
            let fold = !query_owned.chars().any(char::is_uppercase);
            let needle = if fold { query_owned.to_lowercase() } else { query_owned.clone() };
            WalkBuilder::new(&root)
                .standard_filters(!all)
                .filter_entry(move |e| {
                    all || e.depth() == 0 || !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
                })
                .threads(cores())
                .build_parallel()
                .run(|| {
                    let (sender, query, needle) = (sender.clone(), query_owned.clone(), needle.clone());
                    let (stop, searched, matches) = (stop_walk.clone(), searched_walk.clone(), matches_walk.clone());
                    Box::new(move |entry| {
                        if stop.load(Ordering::Relaxed) {
                            return WalkState::Quit;
                        }
                        let Ok(entry) = entry else { return WalkState::Continue };
                        if !entry.file_type().is_some_and(|t| t.is_file()) {
                            return WalkState::Continue;
                        }
                        searched.fetch_add(1, Ordering::Relaxed);
                        if let Some(hits) = search_file(entry.path(), &query, &needle, fold) {
                            let total = matches.fetch_add(hits.len(), Ordering::Relaxed) + hits.len();
                            let _ = sender.send(Some(FileHits { path: entry.into_path(), hits }));
                            if total >= MAX_MATCHES {
                                stop.store(true, Ordering::Relaxed);
                                return WalkState::Quit;
                            }
                        }
                        WalkState::Continue
                    })
                });
            let _ = sender.send(None);
        });
        Self {
            query: query.to_string(),
            files: Vec::new(),
            done: false,
            searched,
            matches,
            stop,
            found,
            selected: (0, 0),
            scroll: 0,
        }
    }

    /// Takes in what's been found since last time.
    pub fn poll(&mut self) {
        while let Ok(found) = self.found.try_recv() {
            let Some(file) = found else {
                self.done = true;
                continue;
            };
            let at = self.files.partition_point(|f| f.path < file.path);
            // Keep the same match selected as files arrive ahead of it
            if !self.files.is_empty() && at <= self.selected.0 {
                self.selected.0 += 1;
            }
            self.files.insert(at, file);
        }
    }

    pub fn matches(&self) -> usize {
        self.matches.load(Ordering::Relaxed)
    }

    /// Whether the search stopped at the limit rather than at the end.
    pub fn cut_short(&self) -> bool {
        self.done && self.matches() >= MAX_MATCHES
    }

    /// The selected file and match.
    pub fn current(&self) -> Option<(&Path, &Hit)> {
        let file = self.files.get(self.selected.0)?;
        Some((&file.path, file.hits.get(self.selected.1)?))
    }

    /// Moves `delta` matches on, across files.
    pub fn step(&mut self, delta: isize) {
        let (mut file, mut hit) = self.selected;
        for _ in 0..delta.unsigned_abs() {
            if delta > 0 {
                if hit + 1 < self.files.get(file).map_or(0, |f| f.hits.len()) {
                    hit += 1;
                } else if file + 1 < self.files.len() {
                    (file, hit) = (file + 1, 0);
                }
            } else if hit > 0 {
                hit -= 1;
            } else if file > 0 {
                file -= 1;
                hit = self.files[file].hits.len().saturating_sub(1);
            }
        }
        self.selected = (file, hit);
    }

    /// The selected match's line in the list, where each file has a line of its own
    /// before its matches.
    pub fn selected_row(&self) -> usize {
        let (file, hit) = self.selected;
        self.files[..file.min(self.files.len())].iter().map(|f| f.hits.len() + 1).sum::<usize>() + hit + 1
    }

    /// The match on a line of the list; a file's own line stands for its first match.
    pub fn at_row(&self, row: usize) -> Option<(usize, usize)> {
        let mut start = 0;
        for (i, file) in self.files.iter().enumerate() {
            let end = start + 1 + file.hits.len();
            if row < end {
                return Some((i, row.saturating_sub(start + 1)));
            }
            start = end;
        }
        None
    }

    pub fn rows(&self) -> usize {
        self.files.iter().map(|f| f.hits.len() + 1).sum()
    }
}

impl Drop for Grep {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The lines of a text file that hold the query.
fn search_file(path: &Path, query: &str, needle: &str, fold: bool) -> Option<Vec<Hit>> {
    if fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes[..bytes.len().min(SNIFF)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    // Most files don't have it at all, which one look at the whole text tells
    let present = if fold { text.to_lowercase().contains(needle) } else { text.contains(needle) };
    if !present {
        return None;
    }
    let hits: Vec<Hit> = text
        .lines()
        .enumerate()
        .filter_map(|(line, text)| {
            let (col, _) = *find_all(text, query).first()?;
            Some(Hit { line, col, text: text.chars().take(1000).collect() })
        })
        .take(PER_FILE)
        .collect();
    (!hits.is_empty()).then_some(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn finish(mut grep: Grep) -> Grep {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !grep.done && Instant::now() < deadline {
            grep.poll();
            thread::sleep(Duration::from_millis(5));
        }
        grep
    }

    #[test]
    fn finds_text_and_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("notes.txt"), "nothing\nsay Hello there\nhello again\n").unwrap();
        fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();
        fs::write(root.join("blob.bin"), b"hello\0binary").unwrap();
        fs::write(root.join("other.txt"), "goodbye\n").unwrap();

        let grep = finish(Grep::start(root, "hello", false));
        let found: Vec<(String, Vec<(usize, usize)>)> = grep
            .files
            .iter()
            .map(|f| {
                let name = f.path.strip_prefix(root).unwrap().display().to_string();
                (name, f.hits.iter().map(|h| (h.line, h.col)).collect())
            })
            .collect();
        assert_eq!(found, vec![("notes.txt".into(), vec![(1, 4), (2, 0)]), ("src/lib.rs".into(), vec![(0, 3)])]);
        assert_eq!(grep.matches(), 3);

        // A capital makes case matter
        let grep = finish(Grep::start(root, "Hello", false));
        assert_eq!(grep.files.len(), 1);
        assert_eq!(grep.files[0].hits.len(), 1);
    }

    #[test]
    fn steps_through_matches_across_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();
        fs::write(dir.path().join("b.txt"), "x\n").unwrap();
        let mut grep = finish(Grep::start(dir.path(), "x", false));
        assert_eq!(grep.selected_row(), 1);
        grep.step(2);
        assert_eq!(grep.selected, (1, 0));
        assert_eq!(grep.selected_row(), 4);
        grep.step(5);
        assert_eq!(grep.selected, (1, 0));
        grep.step(-1);
        assert_eq!(grep.selected, (0, 1));
        assert_eq!(grep.at_row(3), Some((1, 0)), "a file's own line is its first match");
    }
}
