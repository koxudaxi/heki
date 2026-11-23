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
    let skills = match agent {
        SkillAgent::ClaudeCode => Path::new(".claude").join("skills"),
        SkillAgent::Codex => Path::new(".agents").join("skills"),
    };
    let root = match scope {
        SkillScope::Project => std::env::current_dir()?,
        SkillScope::User => std::env::home_dir().ok_or_else(no_home)?,
    };
    Ok(root.join(skills).join(NAME))
}

fn no_home() -> Problem {
    Problem::new("cannot find the home directory")
        .help("set `HOME`, or install the skill into the project with `--scope project`")
        .docs(Docs::InstallSkill)
}

pub(crate) fn install(args: &SkillInstallArgs) -> Result<ExitCode> {
    let destination = destination(args.agent, args.scope)?;
    let shown = shown_path(&destination);
    if let Ok(found) = destination.symlink_metadata() {
        if !found.is_dir() {
            return Err(Problem::new("a file or a link is where the skill goes")
                .at_file(&shown)
                .help("remove it, or install the skill with the other `--scope`")
                .docs(Docs::InstallSkill)
                .into());
        }
        if !args.overwrite {
            return Err(Problem::new("the skill is already installed")
                .at_file(&shown)
                .help("pass `--overwrite` to replace it with the skill of this heki")
                .docs(Docs::InstallSkill)
                .into());
        }
    }
    let parent = destination
        .parent()
        .expect("the skill's directory has a parent");
    // The skill is written next to its place and moved there whole, so an agent never reads
    // half of it.
    let staged = parent.join(format!(".{NAME}.installing-{}", std::process::id()));
    let unwritable = |e: std::io::Error| {
        Problem::new(format!("cannot write the skill: {e}"))
            .at_file(&shown)
            .help("make the directory writable, or install the skill with the other `--scope`")
            .docs(Docs::InstallSkill)
    };
    write_files(&staged).map_err(unwritable)?;
    if destination.exists() {
        let replaced = parent.join(format!(".{NAME}.replaced-{}", std::process::id()));
        std::fs::rename(&destination, &replaced).map_err(unwritable)?;
        std::fs::rename(&staged, &destination).map_err(unwritable)?;
        std::fs::remove_dir_all(&replaced).map_err(unwritable)?;
    } else {
        std::fs::rename(&staged, &destination).map_err(unwritable)?;
    }
    eprintln!(
        "Installed the heki Agent Skill (experimental) at {}",
        shown.display()
    );
    Ok(ExitCode::SUCCESS)
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
