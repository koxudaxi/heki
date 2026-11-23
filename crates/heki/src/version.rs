//! `heki version`: the version of this heki and, with `--changelog`, what this release changed,
//! read from the `CHANGELOG.md` the binary was built with.

use anyhow::Result;
use std::process::ExitCode;

/// The changelog the binary was built from: the same file the source distribution carries.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// The section of `version`: from its `## [version]` heading up to the next release's.
fn section_of<'a>(changelog: &'a str, version: &str) -> Option<&'a str> {
    let heading = format!("\n## [{version}]");
    let start = changelog.find(&heading)? + 1;
    let end = changelog[start..]
        .find("\n## [")
        .map_or(changelog.len(), |at| start + at);
    Some(changelog[start..end].trim_end())
}

/// What `heki version --changelog` prints after the version line.
fn changelog_text(changelog: &str, version: &str) -> String {
    section_of(changelog, version).map_or_else(
        || format!("The changelog has no section for {version}: this is a development build."),
        str::to_owned,
    )
}

pub(crate) fn run(changelog: bool) -> Result<ExitCode> {
    println!("heki {}", heki_env::VERSION);
    if changelog {
        println!("\n{}", changelog_text(CHANGELOG, heki_env::VERSION));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_ends_before_the_next_release() {
        let text = "# Changelog\n\n## [2.0] - now\n\n- a\n\n## [1.0] - then\n\n- b\n";
        assert_eq!(section_of(text, "1.0"), Some("## [1.0] - then\n\n- b"));
        assert_eq!(section_of(text, "2.0"), Some("## [2.0] - now\n\n- a"));
        assert_eq!(section_of(text, "3.0"), None);
        assert_eq!(section_of(text, "1"), None);
    }

    #[test]
    fn a_version_without_a_section_says_so_and_prints_nothing_else() {
        let text = "# Changelog\n\n## [1.0] - then\n\n- b\n";
        assert_eq!(
            changelog_text(text, "1.1"),
            "The changelog has no section for 1.1: this is a development build."
        );
        assert_eq!(changelog_text(text, "1.0"), "## [1.0] - then\n\n- b");
    }
}
