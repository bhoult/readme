//! Moving, renaming and deleting files from the tree. Nothing gets overwritten, and
//! deleting sends things to the desktop trash, where they can be restored from.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

/// Sends files and folders to the trash.
pub fn trash(paths: &[PathBuf]) -> Result<()> {
    #[cfg(not(test))]
    trash::delete_all(paths).context("couldn't move them to the trash")?;
    // Tests work in temporary folders and leave the real trash alone
    #[cfg(test)]
    for path in paths {
        if path.is_dir() { fs::remove_dir_all(path)? } else { fs::remove_file(path)? }
    }
    Ok(())
}

fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Renames a file or folder where it is; returns its new path.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        bail!("“{name}” isn't a name a file can have");
    }
    let target = path.with_file_name(name);
    if target == path {
        return Ok(target);
    }
    if exists(&target) {
        bail!("there's already a {name} there");
    }
    fs::rename(path, &target).with_context(|| format!("couldn't rename {}", file_name(path)))?;
    Ok(target)
}

/// Moves each item into `dir`, keeping its name. Stops at the first one that can't
/// go, and returns the moves made along with that problem.
pub fn move_into(items: &[PathBuf], dir: &Path) -> (Vec<(PathBuf, PathBuf)>, Option<anyhow::Error>) {
    let mut moved = Vec::new();
    for item in items {
        let target = dir.join(item.file_name().unwrap_or_default());
        let problem = if dir.starts_with(item) {
            Some(anyhow::anyhow!("{} can't go inside itself", file_name(item)))
        } else if exists(&target) {
            Some(anyhow::anyhow!("there's already a {} there", file_name(item)))
        } else {
            fs::rename(item, &target).err().map(|e| anyhow::anyhow!("couldn't move {}: {e}", file_name(item)))
        };
        if let Some(problem) = problem {
            return (moved, Some(problem));
        }
        moved.push((item.clone(), target));
    }
    (moved, None)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_and_move_refuse_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/b.txt"), "other b").unwrap();

        assert!(rename(&root.join("a.txt"), "b.txt").is_err());
        assert!(rename(&root.join("a.txt"), "x/y").is_err());
        let renamed = rename(&root.join("a.txt"), "c.txt").unwrap();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "a");

        let (moved, problem) = move_into(&[root.join("c.txt"), root.join("b.txt")], &root.join("sub"));
        assert_eq!(moved, vec![(root.join("c.txt"), root.join("sub/c.txt"))]);
        assert!(problem.is_some(), "b.txt is already in sub");
        assert_eq!(fs::read_to_string(root.join("sub/b.txt")).unwrap(), "other b");

        let (_, problem) = move_into(&[root.join("sub")], &root.join("sub"));
        assert!(problem.is_some(), "a folder can't go inside itself");
    }
}
