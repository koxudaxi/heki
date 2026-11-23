//! `heki version`: the version of this heki and, with `--changelog`, what this release changed,
//! read from the `CHANGELOG.md` the binary was built with.

use anyhow::Result;
use std::process::ExitCode;

/// The changelog the binary was built from: the same file the source distribution carries.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// The newest release's section: from its `## [` heading up to the next release's.
fn newest_section(changelog: &str) -> Option<&str> {
    let mut headings = changelog.match_indices("\n## [").map(|(at, _)| at + 1);
    let start = headings.next()?;
    let end = headings.next().unwrap_or(changelog.len());
    Some(changelog[start..end].trim_end())
}

pub(crate) fn run(changelog: bool) -> Result<ExitCode> {
    println!("heki {}", heki_env::VERSION);
    if changelog {
        println!("\n{}", newest_section(CHANGELOG).unwrap_or_default());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_section_ends_before_the_next_release() {
        let text = "# Changelog\n\n## [2.0] - now\n\n- a\n\n## [1.0] - then\n\n- b\n";
        assert_eq!(newest_section(text), Some("## [2.0] - now\n\n- a"));
        assert_eq!(newest_section("# Changelog\n"), None);
        assert_eq!(
            newest_section("x\n## [1.0] - only\n- b\n"),
            Some("## [1.0] - only\n- b")
        );
    }

    #[test]
    fn the_embedded_section_is_the_top_one_of_the_changelog() {
        let section = newest_section(CHANGELOG).unwrap();
        assert!(section.starts_with("## ["), "{section}");
        let first = CHANGELOG.find("\n## [").unwrap() + 1;
        assert_eq!(CHANGELOG[..first].matches("\n## [").count(), 0);
        assert!(!section[3..].contains("\n## ["));
    }
}
