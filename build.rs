//! Stamps the binary with when it was built, for the help page and --version.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // No rerun-if lines: without them Cargo runs this again whenever a file in the
    // package changes, so a binary rebuilt from changed code says when that was
    println!("cargo:rustc-env=README_BUILT={now}");
}
