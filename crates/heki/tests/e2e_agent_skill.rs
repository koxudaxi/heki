//! The Agent Skill in `skills/heki/` against the binary and the documentation: its frontmatter
//! follows the Agent Skills format, every command and option it and the Coding Agents page name
//! exists, its tables of output fields are the page's, and the skill, the page, and the Claude
//! Code marketplace point at each other.

mod common;

use common::{Project, repo_file, repo_root};
use std::collections::BTreeMap;
use std::path::Path;

const SKILL_DIR: &str = "skills/heki";
const PAGE: &str = "docs/coding-agents.md";
const SITE: &str = "https://heki.koxudaxi.dev/";

/// Every file of the skill, relative to the repository root, with `/` separators.
fn skill_files() -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap();
            let rel = format!("{rel}/{name}");
            if path.is_dir() {
                walk(&path, &rel, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo_root().join(SKILL_DIR), SKILL_DIR, &mut out);
    out.sort();
    out
}

/// The frontmatter and the body of `SKILL.md`.
fn skill() -> (String, String) {
    let text = repo_file(&format!("{SKILL_DIR}/SKILL.md"));
    let rest = text
        .strip_prefix("---\n")
        .expect("SKILL.md starts with its frontmatter");
    let (front, body) = rest.split_once("\n---\n").expect("the frontmatter ends");
    (front.to_owned(), body.to_owned())
}

#[test]
fn the_skill_follows_the_agent_skills_format() {
    let (front, body) = skill();
    let lines: Vec<&str> = front.lines().collect();
    // The name is the directory's: lowercase letters, digits, and hyphens.
    assert!(lines.contains(&"name: heki"), "{front}");
    assert_eq!(Path::new(SKILL_DIR).file_name().unwrap(), "heki");
    assert!(lines.contains(&"license: MIT"), "{front}");
    assert!(
        front.contains("\nmetadata:\n  status: experimental"),
        "{front}"
    );
    // A folded description: the indented lines below `description: >`, 1 to 1024 characters.
    let start = lines.iter().position(|l| *l == "description: >").unwrap();
    let description: Vec<&str> = lines[start + 1..]
        .iter()
        .take_while(|l| l.starts_with("  "))
        .map(|l| l.trim())
        .collect();
    let description = description.join(" ");
    assert!(
        (1..=1024).contains(&description.chars().count()),
        "{description}"
    );
    for words in [
        "heki inspect",
        "heki check --output-format json",
        "related",
        "heki rule <CODE>",
        "heuristic",
        "heki check --generate-prompt",
    ] {
        assert!(description.contains(words), "{words}: {description}");
    }
    // Every top-level key is one the format defines.
    for line in lines.iter().filter(|l| !l.starts_with(' ')) {
        let key = line.split(':').next().unwrap();
        assert!(
            [
                "name",
                "description",
                "license",
                "compatibility",
                "metadata",
                "allowed-tools"
            ]
            .contains(&key),
            "{line}"
        );
    }
    assert!(body.lines().count() < 500, "a SKILL.md body stays short");
}

/// The commands of `text`: each inline code span, and each line of a fenced block.
fn code_segments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced {
            out.push(line.trim().to_owned());
        } else {
            out.extend(line.split('`').skip(1).step_by(2).map(str::to_owned));
        }
    }
    out
}

/// `(command, option)` for each `--option` of a `heki` command in `segment`: `heki` is its first
/// word, or follows `$`, `uvx`, or `run` (`uv run heki`); the command is `""` for an option of
/// `heki` itself. A pipe or `&&` ends the command.
fn named_options(segment: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let Some(at) = words
        .iter()
        .enumerate()
        .position(|(i, w)| *w == "heki" && (i == 0 || ["$", "uvx", "run"].contains(&words[i - 1])))
    else {
        return Vec::new();
    };
    let rest = &words[at + 1..];
    let command = rest
        .first()
        .filter(|w| !w.starts_with('-'))
        .copied()
        .unwrap_or("");
    rest.iter()
        .take_while(|w| !["|", "&&", ";"].contains(w))
        .filter(|w| w.starts_with("--") && w.len() > 2)
        .map(|w| {
            let option = w.trim_end_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-'));
            (command.to_owned(), option.to_owned())
        })
        .collect()
}

#[test]
fn every_command_and_option_the_skill_and_the_page_name_exists() {
    let p = Project::new("skill_options");
    let mut help: BTreeMap<String, String> = BTreeMap::new();
    let mut checked = 0;
    for file in skill_files().into_iter().chain([PAGE.to_owned()]) {
        for segment in code_segments(&repo_file(&file)) {
            for (command, option) in named_options(&segment) {
                let text = help.entry(command.clone()).or_insert_with(|| {
                    let args: Vec<&str> = [command.as_str(), "--help"]
                        .into_iter()
                        .filter(|a| !a.is_empty())
                        .collect();
                    let (code, out, err) = p.heki(&args);
                    assert_eq!(code, 0, "{file}: `heki {command}` is no command: {err}");
                    out
                });
                assert!(
                    text.contains(&format!("{option} ")) || text.contains(&format!("{option}\n")),
                    "{file}: `heki {command}` has no option `{option}` (in `{segment}`)"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 20, "{checked}");
    assert!(help.contains_key("check") && help.contains_key("inspect"));
}

/// The rows of the first table after `marker` in `text`, with each link's target dropped
/// (`[Rule Precision](precision.md)` reads `[Rule Precision]`): the skill links the site, the
/// page links its neighbors.
fn table_after(text: &str, marker: &str) -> Vec<String> {
    let start = text.find(marker).unwrap_or_else(|| panic!("no `{marker}`"));
    text[start..]
        .lines()
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'))
        .map(|row| {
            let mut out = String::new();
            let mut rest = row;
            while let Some(at) = rest.find("](") {
                out.push_str(&rest[..=at]);
                let close = rest[at..].find(')').unwrap();
                rest = &rest[at + close + 1..];
            }
            out.push_str(rest);
            out
        })
        .collect()
}

#[test]
fn the_skill_lists_the_output_fields_the_page_lists() {
    let page = repo_file(PAGE);
    let output = repo_file(&format!("{SKILL_DIR}/references/output.md"));
    let check = table_after(&page, "Each diagnostic has:");
    assert!(check.len() > 10, "{check:?}");
    assert_eq!(
        table_after(&output, "## `heki check --output-format json`"),
        check
    );
    let inspect = table_after(&page, "## Before an edit: `heki inspect`");
    assert!(inspect.len() > 8, "{inspect:?}");
    assert_eq!(
        table_after(&output, "## `heki inspect --output-format json`"),
        inspect
    );
}

/// The heading anchors of a Markdown page, as the site writes them.
fn anchors(page: &str) -> Vec<String> {
    page.lines()
        .filter_map(|l| l.strip_prefix('#'))
        .map(|heading| {
            let words: String = heading
                .trim_start_matches('#')
                .trim()
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                .collect();
            words.split_whitespace().collect::<Vec<_>>().join("-")
        })
        .collect()
}

/// Every link target of a Markdown text: `[text](target)` and `<https://...>`.
fn link_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices("](") {
        let rest = &text[i + 2..];
        out.push(rest[..rest.find(')').unwrap()].to_owned());
    }
    for (i, _) in text.match_indices("<https://") {
        let rest = &text[i + 1..];
        out.push(rest[..rest.find('>').unwrap()].to_owned());
    }
    out
}

#[test]
fn the_skill_the_page_and_the_marketplace_point_at_each_other() {
    let page = repo_file(PAGE);
    assert!(
        page.contains("https://github.com/koxudaxi/heki/tree/main/skills/heki"),
        "the page links the skill"
    );
    for anchor in link_targets(&page)
        .iter()
        .filter_map(|t| t.strip_prefix('#'))
    {
        assert!(
            anchors(&page).iter().any(|a| a == anchor),
            "{PAGE}: #{anchor}"
        );
    }

    // The skill links the page, and its links lead to files of the skill or pages of the site.
    let (_, body) = skill();
    assert!(
        body.contains(&format!("{SITE}coding-agents/")),
        "the skill links the page"
    );
    for file in skill_files() {
        let dir = Path::new(&file).parent().unwrap();
        for target in link_targets(&repo_file(&file)) {
            if let Some(path) = target.strip_prefix(SITE) {
                let (page_path, anchor) = path.split_once('#').unwrap_or((path, ""));
                // `coding-agents/` is `coding-agents.md`, `llms.txt` itself.
                let doc = page_path
                    .strip_suffix('/')
                    .map_or_else(|| page_path.to_owned(), |name| format!("{name}.md"));
                let text = repo_file(&format!("docs/{doc}"));
                assert!(
                    anchor.is_empty() || anchors(&text).iter().any(|a| a == anchor),
                    "{file}: {target}"
                );
            } else if !target.starts_with("https://") {
                assert!(
                    repo_root().join(dir).join(&target).is_file(),
                    "{file}: {target}"
                );
            }
        }
    }

    // The Claude Code marketplace installs this directory as the plugin `heki`.
    let marketplace: serde_json::Value =
        serde_json::from_str(&repo_file(".claude-plugin/marketplace.json")).unwrap();
    assert_eq!(marketplace["name"], "heki");
    let plugins = marketplace["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(
        (&plugins[0]["name"], &plugins[0]["source"]),
        (
            &serde_json::json!("heki"),
            &serde_json::json!(format!("./{SKILL_DIR}"))
        )
    );
}
