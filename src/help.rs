//! The help page: which readme this is, where it comes from, what it does, and its keys.

use crate::git;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// "readme 0.1.0, built 2026-10-27 10:41", for --version.
pub fn version() -> String {
    format!("readme {VERSION}, built {}", local_time(built()))
}

/// The help page, in Markdown. The features are the README's list of them, and the
/// keys are what --help says.
pub fn page() -> String {
    let built = built();
    let mut page = format!(
        "# readme {VERSION}\n\n{}.\n\n- Version {VERSION}, built {} ({} ago)\n- Source: <{}>\n- {} license\n\n## Features\n\n",
        env!("CARGO_PKG_DESCRIPTION"),
        local_time(built),
        git::age(built),
        env!("CARGO_PKG_REPOSITORY"),
        env!("CARGO_PKG_LICENSE"),
    );
    for feature in features() {
        page += &format!("- {feature}\n");
    }
    let keys = crate::USAGE.split_once("\nKeys\n").map_or(crate::USAGE, |(_, keys)| keys);
    page += &format!("\n## Keys\n\n```txt\n{keys}```\n");
    page
}

/// When this binary was built, in seconds since the epoch, as build.rs found it.
fn built() -> u64 {
    env!("README_BUILT").parse().unwrap_or(0)
}

/// The README's "At a glance" list, without its links.
fn features() -> Vec<String> {
    let readme = include_str!("../README.md");
    let list = readme.split_once("\n## At a glance\n").map_or("", |(_, rest)| rest);
    list.lines()
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.strip_prefix("- "))
        .map(unlink)
        .collect()
}

/// Markdown links reduced to their text: "[**Tree**](#the-tree)" to "**Tree**".
fn unlink(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        // A bracket that doesn't start a link stays as it is
        let link = rest[open..].find("](").map(|i| open + i).and_then(|close| {
            let end = close + rest[close..].find(')')?;
            (!rest[open + 1..close].contains(']')).then_some((close, end))
        });
        match link {
            Some((close, end)) => {
                out.push_str(&rest[..open]);
                out.push_str(&rest[open + 1..close]);
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    out + rest
}

/// "2026-10-27 10:41": the local time `secs` seconds after the epoch.
pub fn local_time(secs: u64) -> String {
    let time = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return format!("@{secs}");
    }
    format!("{}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_features_are_the_readmes_without_links() {
        let features = features();
        assert!(features.len() >= 10, "{features:?}");
        assert!(features[0].starts_with("**Tree** of the folder"), "{features:?}");
        assert!(features.iter().any(|f| f.starts_with("**Git branches**")), "{features:?}");
        assert!(features.iter().any(|f| f.contains("every language bat knows")), "{features:?}");
        assert!(features.iter().all(|f| !f.contains("](")), "{features:?}");
    }

    #[test]
    fn links_lose_their_targets() {
        assert_eq!(unlink("[**Tree**](#the-tree) of it"), "**Tree** of it");
        assert_eq!(unlink("[a] then [b](https://b.dev), [c]"), "[a] then b, [c]");
        assert_eq!(unlink("[unfinished](x"), "[unfinished](x");
    }

    #[test]
    fn times_are_local_and_to_the_minute() {
        // 2023-11-14 22:13 UTC: the 14th or 15th anywhere in the world
        let t = local_time(1_700_000_000);
        assert!(t.starts_with("2023-11-14 ") || t.starts_with("2023-11-15 "), "{t}");
        assert_eq!((t.len(), &t[13..14]), (16, ":"), "{t}");
    }

    #[test]
    fn the_page_says_which_readme_this_is() {
        let page = page();
        assert!(page.starts_with(&format!("# readme {VERSION}\n")), "{page}");
        assert!(page.contains(&format!("- Version {VERSION}, built 20")), "{page}");
        assert!(page.contains("- Source: <https://github.com/bhoult/readme>"), "{page}");
        assert!(page.contains("\n## Keys\n\n```txt\n  Tab "), "{page}");
        assert!(version().starts_with(&format!("readme {VERSION}, built 20")));
    }
}
