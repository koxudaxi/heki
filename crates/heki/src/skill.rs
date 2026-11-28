//! `heki skill install`: the Agent Skill of `skills/heki/`, embedded in the binary when it is
//! built, copied where a coding agent reads skills. The skill a binary installs is the one that
//! describes its own commands and output.

use crate::check::shown_path;
use crate::{SkillAgent, SkillInstallArgs, SkillScope};
use anyhow::Result;
use heki_env::{Docs, Problem};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The skill's directory name, and its `name` in `SKILL.md`.
const NAME: &str = "heki";

/// The files of the skill, relative to its directory.
pub(crate) const FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../../../skills/heki/SKILL.md")),
    (
        "references/adopt.md",
        include_str!("../../../skills/heki/references/adopt.md"),
    ),
    (
        "references/contracts.md",
        include_str!("../../../skills/heki/references/contracts.md"),
    ),
    (
        "references/output.md",
        include_str!("../../../skills/heki/references/output.md"),
    ),
    (
        "references/troubleshooting.md",
        include_str!("../../../skills/heki/references/troubleshooting.md"),
    ),
];

/// The directory `agent` reads the skill from at `scope`.
fn destination(agent: SkillAgent, scope: SkillScope) -> Result<PathBuf> {
    let root = match scope {
        SkillScope::Project => std::env::current_dir()?,
        SkillScope::User => std::env::home_dir().ok_or_else(no_home)?,
    };
    Ok(below(&root, agent))
}

/// The skill's directory below `root` (a project root, or the home directory), where `agent`
/// reads skills.
pub(crate) fn below(root: &Path, agent: SkillAgent) -> PathBuf {
    let skills = match agent {
        SkillAgent::ClaudeCode => Path::new(".claude").join("skills"),
        SkillAgent::Codex => Path::new(".agents").join("skills"),
    };
    root.join(skills).join(NAME)
}

fn no_home() -> Problem {
    Problem::new("cannot find the home directory")
        .help("set `HOME`, or install the skill into the project with `--scope project`")
        .docs(Docs::InstallSkill)
}

/// What is where a skill goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Free,
    /// A directory: a skill installed before, which only a replacement asked for changes.
    Installed,
    /// A file or a link, which is never replaced.
    Taken,
}

pub(crate) fn place(destination: &Path) -> Place {
    match destination.symlink_metadata() {
        Ok(found) if found.is_dir() => Place::Installed,
        Ok(_) => Place::Taken,
        Err(_) => Place::Free,
    }
}

/// The skill's place `shown` holds a file or a link.
pub(crate) fn taken(shown: &Path) -> Problem {
    Problem::new("a file or a link is where the skill goes")
        .at_file(shown)
        .docs(Docs::InstallSkill)
}

/// A skill is installed at `shown` already.
pub(crate) fn installed(shown: &Path) -> Problem {
    Problem::new("the skill is already installed")
        .at_file(shown)
        .docs(Docs::InstallSkill)
}

/// The skill could not be written at `shown`.
pub(crate) fn unwritable(shown: &Path, cause: &std::io::Error, help: &str) -> Problem {
    Problem::new(format!("cannot write the skill: {cause}"))
        .at_file(shown)
        .help(help)
        .docs(Docs::InstallSkill)
}

pub(crate) fn install(args: &SkillInstallArgs) -> Result<ExitCode> {
    let destination = destination(args.agent, args.scope)?;
    let shown = shown_path(&destination);
    match place(&destination) {
        Place::Taken => {
            return Err(taken(&shown)
                .help("remove it, or install the skill with the other `--scope`")
                .into());
        }
        Place::Installed if !args.overwrite => {
            return Err(installed(&shown)
                .help("pass `--overwrite` to replace it with the skill of this heki")
                .into());
        }
        Place::Installed | Place::Free => {}
    }
    write(&destination).map_err(|e| {
        unwritable(
            &shown,
            &e,
            "make the directory writable, or install the skill with the other `--scope`",
        )
    })?;
    eprintln!(
        "Installed the heki Agent Skill (experimental) at {}",
        heki_diagnostics::path_text(&shown)
    );
    Ok(ExitCode::SUCCESS)
}

/// Writes the skill at `destination`, in place of a skill there. It is written next to its
/// place and moved there whole, so an agent never reads half of it.
pub(crate) fn write(destination: &Path) -> std::io::Result<()> {
    let parent = destination
        .parent()
        .expect("the skill's directory has a parent");
    let staged = parent.join(format!(".{NAME}.installing-{}", std::process::id()));
    write_files(&staged)?;
    if destination.exists() {
        let replaced = parent.join(format!(".{NAME}.replaced-{}", std::process::id()));
        std::fs::rename(destination, &replaced)?;
        std::fs::rename(&staged, destination)?;
        std::fs::remove_dir_all(&replaced)?;
    } else {
        std::fs::rename(&staged, destination)?;
    }
    Ok(())
}

/// The skill's files in a new directory `dir`.
fn write_files(dir: &Path) -> std::io::Result<()> {
    for (rel, text) in FILES {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("a file of the skill has a directory"))?;
        std::fs::write(path, text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_missing_home_directory_says_to_install_into_the_project() {
        let problem = super::no_home();
        assert!(problem.help[0].contains("--scope project"), "{problem:?}");
    }
}
