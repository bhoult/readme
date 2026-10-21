//! Files inside zip and tar archives (plain or compressed with gzip, bzip2, xz or
//! zstd), listed and read without unpacking anything to disk.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const MAX_ENTRIES: usize = 100_000;
const MAX_READ: u64 = 50 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
}

fn format(path: &Path) -> Option<Format> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    Some(if ends(&[".zip", ".jar", ".war", ".whl", ".epub"]) {
        Format::Zip
    } else if ends(&[".tar"]) {
        Format::Tar
    } else if ends(&[".tar.gz", ".tgz"]) {
        Format::TarGz
    } else if ends(&[".tar.bz2", ".tbz2", ".tbz"]) {
        Format::TarBz2
    } else if ends(&[".tar.xz", ".txz"]) {
        Format::TarXz
    } else if ends(&[".tar.zst", ".tzst"]) {
        Format::TarZst
    } else {
        return None;
    })
}

/// Whether the name says this is an archive readme can open.
pub fn is_archive(path: &Path) -> bool {
    format(path).is_some()
}

/// For a path that runs on inside an archive (`notes.zip/docs/a.md`), the archive and
/// the path within it.
pub fn split(path: &Path) -> Option<(PathBuf, String)> {
    for dir in path.ancestors().skip(1) {
        if dir.is_file() {
            let inner = path.strip_prefix(dir).ok()?.to_string_lossy();
            return is_archive(dir).then(|| (dir.to_path_buf(), clean(&inner)));
        }
        if dir.exists() {
            return None;
        }
    }
    None
}

pub struct Entry {
    /// '/'-separated, with no leading "./" or slashes
    pub path: String,
    pub is_dir: bool,
}

fn clean(name: &str) -> String {
    name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect::<Vec<_>>().join("/")
}

fn tar(path: &Path, format: Format) -> Result<tar::Archive<Box<dyn Read>>> {
    let file = BufReader::new(File::open(path)?);
    let reader: Box<dyn Read> = match format {
        Format::TarGz => Box::new(flate2::read::MultiGzDecoder::new(file)),
        Format::TarBz2 => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        Format::TarXz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(file)),
        Format::TarZst => Box::new(zstd::stream::read::Decoder::with_buffer(file)?),
        _ => Box::new(file),
    };
    Ok(tar::Archive::new(reader))
}

fn zip(path: &Path) -> Result<zip::ZipArchive<BufReader<File>>> {
    Ok(zip::ZipArchive::new(BufReader::new(File::open(path)?))?)
}

/// Everything in the archive, including directories that only show up in paths.
pub fn list(path: &Path) -> Result<Vec<Entry>> {
    let format = format(path).context("not an archive")?;
    let mut names: Vec<(String, bool)> = Vec::new();
    if format == Format::Zip {
        let zip = zip(path)?;
        for name in zip.file_names().take(MAX_ENTRIES) {
            let name = name?;
            names.push((clean(&name), name.ends_with('/')));
        }
    } else {
        for entry in tar(path, format)?.entries()?.take(MAX_ENTRIES) {
            let entry = entry?;
            let is_dir = entry.header().entry_type().is_dir();
            names.push((clean(&entry.path()?.to_string_lossy()), is_dir));
        }
    }
    let mut dirs: BTreeSet<String> = names.iter().filter(|(_, d)| *d).map(|(n, _)| n.clone()).collect();
    for (name, _) in &names {
        let mut rest = name.as_str();
        while let Some((parent, _)) = rest.rsplit_once('/') {
            dirs.insert(parent.to_string());
            rest = parent;
        }
    }
    let mut entries: Vec<Entry> = names
        .into_iter()
        .filter(|(n, d)| !d && !n.is_empty() && !dirs.contains(n))
        .map(|(path, _)| Entry { path, is_dir: false })
        .collect();
    entries.extend(dirs.into_iter().filter(|d| !d.is_empty()).map(|path| Entry { path, is_dir: true }));
    Ok(entries)
}

/// The contents of one file in the archive.
pub fn read(path: &Path, entry: &str) -> Result<Vec<u8>> {
    let format = format(path).context("not an archive")?;
    let mut out = Vec::new();
    if format == Format::Zip {
        let mut zip = zip(path)?;
        let index = (0..zip.len())
            .find(|&i| zip.name_for_index(i).is_some_and(|n| n.is_ok_and(|n| clean(&n) == entry)))
            .context("not in the archive")?;
        let file = zip.by_index(index)?;
        if file.size() > MAX_READ {
            bail!("too large to open ({} MB)", file.size() >> 20);
        }
        file.take(MAX_READ).read_to_end(&mut out)?;
        return Ok(out);
    }
    for item in tar(path, format)?.entries()? {
        let item = item?;
        if clean(&item.path()?.to_string_lossy()) == entry {
            let size = item.header().size()?;
            if size > MAX_READ {
                bail!("too large to open ({} MB)", size >> 20);
            }
            item.take(MAX_READ).read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    bail!("not in the archive")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn names(entries: &[Entry]) -> Vec<(String, bool)> {
        let mut v: Vec<_> = entries.iter().map(|e| (e.path.clone(), e.is_dir)).collect();
        v.sort();
        v
    }

    #[test]
    fn zip_and_compressed_tar() {
        let dir = tempfile::tempdir().unwrap();

        let zip_path = dir.path().join("a.zip");
        let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("docs/readme.md", options).unwrap();
        zip.write_all(b"# hi\n").unwrap();
        zip.start_file("top.txt", options).unwrap();
        zip.write_all(b"top\n").unwrap();
        zip.finish().unwrap();
        let want = vec![("docs".into(), true), ("docs/readme.md".into(), false), ("top.txt".into(), false)];
        assert_eq!(names(&list(&zip_path).unwrap()), want);
        assert_eq!(read(&zip_path, "docs/readme.md").unwrap(), b"# hi\n");

        let tgz_path = dir.path().join("b.tar.gz");
        let gz = flate2::write::GzEncoder::new(File::create(&tgz_path).unwrap(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "./src/main.rs", &b"main"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        assert_eq!(names(&list(&tgz_path).unwrap()), vec![("src".into(), true), ("src/main.rs".into(), false)]);
        assert_eq!(read(&tgz_path, "src/main.rs").unwrap(), b"main");
        assert!(read(&tgz_path, "nope").is_err());

        let (archive, inner) = split(&tgz_path.join("src/main.rs")).unwrap();
        assert_eq!((archive, inner.as_str()), (tgz_path.clone(), "src/main.rs"));
        assert!(split(&dir.path().join("missing/file")).is_none());
    }
}
