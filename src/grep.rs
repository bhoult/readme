//! Searching the text of every file under a folder, on all the cores. Results arrive
//! as they're found, kept in path order.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use ignore::{WalkBuilder, WalkState};

use crate::{
    cores,
    text::{find_all, fold_case},
    tree::SKIP_DIRS,
};

/// Bigger files aren't searched
const MAX_FILE: u64 = 10 << 20;
/// The search stops after this many matching lines
pub const MAX_MATCHES: usize = 10_000;
const PER_FILE: usize = 500;
/// How much of a file is checked for NUL bytes to tell binary from text
const SNIFF: usize = 8000;

/// How much of a matching line is kept, around the match
const KEEP: usize = 1000;

/// A matching line: its number (from 0), where the match starts (in chars), and the line,
/// or for a long one the part of it from char `from`, with the match in it.
pub struct Hit {
    pub line: usize,
    pub col: usize,
    pub from: usize,
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
    /// Text files left out for being over MAX_FILE
    too_big: Arc<AtomicUsize>,
    matches: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    found: mpsc::Receiver<Option<FileHits>>,
    /// The selected match: which file, and which of its lines
    pub selected: (usize, usize),
    /// Whether the selected match has been shown; from then on it stays selected as files
    /// arrive ahead of it, and until then the selection is the first match
    pub shown: bool,
    pub scroll: usize,
}

impl Grep {
    /// Starts searching for `query` under `root`. With `all`, hidden and ignored files
    /// are searched too.
    pub fn start(root: &Path, query: &str, all: bool) -> Self {
        let (sender, found) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let searched = Arc::new(AtomicUsize::new(0));
        let too_big = Arc::new(AtomicUsize::new(0));
        let matches = Arc::new(AtomicUsize::new(0));
        let (root, query_owned) = (root.to_path_buf(), query.to_string());
        let (stop_walk, searched_walk, matches_walk) = (stop.clone(), searched.clone(), matches.clone());
        let too_big_walk = too_big.clone();
        thread::spawn(move || {
            // Case matters only when the query has a capital
            let fold = !query_owned.chars().any(char::is_uppercase);
            let needle = if fold { fold_case(&query_owned) } else { query_owned.clone() };
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
                    let too_big = too_big_walk.clone();
                    Box::new(move |entry| {
                        if stop.load(Ordering::Relaxed) {
                            return WalkState::Quit;
                        }
                        let Ok(entry) = entry else { return WalkState::Continue };
                        if !entry.file_type().is_some_and(|t| t.is_file()) {
                            return WalkState::Continue;
                        }
                        searched.fetch_add(1, Ordering::Relaxed);
                        let hits = match search_file(entry.path(), &query, &needle, fold) {
                            Searched::Hits(hits) => Some(hits),
                            Searched::TooBig => {
                                too_big.fetch_add(1, Ordering::Relaxed);
                                None
                            }
                            Searched::Nothing => None,
                        };
                        if let Some(hits) = hits {
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
            too_big,
            matches,
            stop,
            found,
            selected: (0, 0),
            shown: false,
            scroll: 0,
        }
    }

    /// Takes in what's been found since last time.
    pub fn poll(&mut self) {
        loop {
            let file = match self.found.try_recv() {
                Ok(Some(file)) => file,
                // The end of the search, or of its thread
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            };
            let at = self.files.partition_point(|f| f.path < file.path);
            // Keep a match that's been shown selected as files arrive ahead of it
            if self.shown && at <= self.selected.0 {
                self.selected.0 += 1;
            }
            self.files.insert(at, file);
        }
    }

    pub fn matches(&self) -> usize {
        self.matches.load(Ordering::Relaxed)
    }

    /// Text files left out for being too big to search, once the search is done.
    pub fn too_big(&self) -> usize {
        if self.done { self.too_big.load(Ordering::Relaxed) } else { 0 }
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

    /// Lines in the list, including the one saying files were too big to search.
    pub fn rows(&self) -> usize {
        self.files.iter().map(|f| f.hits.len() + 1).sum::<usize>() + usize::from(self.too_big() > 0)
    }
}

impl Drop for Grep {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

enum Searched {
    Hits(Vec<Hit>),
    Nothing,
    /// Text, but too big to search
    TooBig,
}

/// The lines of a text file that hold the query.
fn search_file(path: &Path, query: &str, needle: &str, fold: bool) -> Searched {
    let Ok(mut file) = File::open(path) else { return Searched::Nothing };
    let size = file.metadata().map_or(0, |m| m.len());
    let mut bytes = Vec::new();
    // Binary files are left out quietly, whatever their size
    if (&mut file).take(SNIFF as u64).read_to_end(&mut bytes).is_err() || bytes.contains(&0) {
        return Searched::Nothing;
    }
    if size > MAX_FILE {
        return Searched::TooBig;
    }
    if file.take(MAX_FILE + 1 - bytes.len() as u64).read_to_end(&mut bytes).is_err() {
        return Searched::Nothing;
    }
    let text = String::from_utf8_lossy(&bytes);
    // Most files don't have it at all, which one look at the whole text tells
    let present = if fold { fold_case(&text).contains(needle) } else { text.contains(needle) };
    if !present {
        return Searched::Nothing;
    }
    let hits: Vec<Hit> = text
        .lines()
        .enumerate()
        .filter_map(|(line, text)| {
            let (col, _) = *find_all(text, query).first()?;
            // A long line (minified code, say) keeps the part around the match
            let from = col.saturating_sub(KEEP / 5);
            Some(Hit { line, col, from, text: text.chars().skip(from).take(KEEP).collect() })
        })
        .take(PER_FILE)
        .collect();
    if hits.is_empty() { Searched::Nothing } else { Searched::Hits(hits) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, Instant},
    };

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

    #[test]
    fn long_lines_other_alphabets_and_big_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A match far along a minified line keeps the part of the line around it
        fs::write(root.join("min.js"), format!("{}needle{}\n", "a".repeat(5000), "b".repeat(5000))).unwrap();
        // Case folds a char at a time, as / search does: Σ is σ, İ is i
        fs::write(root.join("greek.txt"), "ΟΔΟΣ\nİstanbul\n").unwrap();
        fs::write(root.join("big.txt"), "needle\n".repeat(2 << 20)).unwrap();
        fs::write(root.join("big.bin"), [b"needle\0".as_slice(), &vec![b'x'; 11 << 20]].concat()).unwrap();

        let grep = finish(Grep::start(root, "needle", false));
        assert_eq!(grep.files.len(), 1);
        let hit = &grep.files[0].hits[0];
        assert_eq!(hit.col, 5000);
        assert_eq!(&hit.text.chars().skip(hit.col - hit.from).take(6).collect::<String>(), "needle");
        // big.txt is text, and over the limit; big.bin is binary, which is left out quietly
        assert_eq!(grep.too_big(), 1);
        assert_eq!(grep.rows(), 3);

        for query in ["οδοσ", "istanbul"] {
            let grep = finish(Grep::start(root, query, false));
            assert_eq!(grep.files.len(), 1, "{query}");
            assert_eq!(grep.files[0].hits.len(), 1, "{query}");
        }
    }
}
