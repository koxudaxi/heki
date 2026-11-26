//! The glob syntax of every configured pattern: `globset`'s, with one addition. `globset` drops
//! an empty alternative (`{,.*}` matches `.x` but not the empty name), where a shell's brace
//! expansion keeps it, so `outer{,.*}` names `outer` and everything below it. A pattern with an
//! empty alternative stands for the globs it expands to.

use globset::{Glob, GlobMatcher, GlobSetBuilder};

/// Whether `\` escapes the next character (as `globset` reads it; a Windows path separator
/// otherwise).
const BACKSLASH_ESCAPES: bool = cfg!(not(windows));

/// A pattern: the globs it stands for, one of which has to match.
#[derive(Debug, Clone)]
pub struct AnyGlob {
    matchers: Vec<GlobMatcher>,
}

impl AnyGlob {
    pub fn new(pattern: &str) -> Result<Self, globset::Error> {
        let matchers = globs(pattern)?.iter().map(Glob::compile_matcher).collect();
        Ok(Self { matchers })
    }

    pub fn is_match(&self, name: &str) -> bool {
        self.matchers.iter().any(|glob| glob.is_match(name))
    }

    pub fn globs(&self) -> impl Iterator<Item = &Glob> {
        self.matchers.iter().map(GlobMatcher::glob)
    }
}

/// The globs `pattern` stands for. Compiling a matcher of one panics when its regex is too large;
/// a set reports that when it is built, so a set takes the globs themselves.
pub fn globs(pattern: &str) -> Result<Vec<Glob>, globset::Error> {
    expand(pattern).iter().map(|glob| Glob::new(glob)).collect()
}

/// Add the globs of `pattern` to `set`.
pub fn add_to_set(set: &mut GlobSetBuilder, pattern: &str) -> Result<(), globset::Error> {
    for glob in globs(pattern)? {
        set.add(glob);
    }
    Ok(())
}

/// The patterns without an empty alternative that `pattern` stands for: the pattern itself
/// unless a `{...}` group has one. Text `globset` rejects (an unclosed group or class) is
/// returned as it is, for `globset` to report.
fn expand(pattern: &str) -> Vec<String> {
    if !pattern.contains('{') {
        return vec![pattern.to_owned()];
    }
    let mut at = 0;
    match parse(pattern, &mut at, false) {
        Some(alternatives) => alternatives
            .iter()
            .flat_map(|sequence| render(sequence))
            .collect(),
        None => vec![pattern.to_owned()],
    }
}

enum Piece<'p> {
    Text(&'p str),
    /// The alternatives of a `{...}` group.
    Group(Vec<Vec<Piece<'p>>>),
}

/// The alternatives of a pattern (one when `in_group` is false) or of a group up to its `}`;
/// `None` for an unbalanced brace or an unclosed class.
fn parse<'p>(text: &'p str, at: &mut usize, in_group: bool) -> Option<Vec<Vec<Piece<'p>>>> {
    let mut alternatives = Vec::new();
    let mut sequence = Vec::new();
    let mut start = *at;
    loop {
        let rest = &text[*at..];
        let Some(c) = rest.chars().next() else {
            if in_group {
                return None;
            }
            sequence.push(Piece::Text(&text[start..]));
            alternatives.push(sequence);
            return Some(alternatives);
        };
        match c {
            '\\' if BACKSLASH_ESCAPES => {
                *at += 1 + rest[1..].chars().next().map_or(0, char::len_utf8);
            }
            '[' => *at += class_len(rest)?,
            '{' => {
                sequence.push(Piece::Text(&text[start..*at]));
                *at += 1;
                sequence.push(Piece::Group(parse(text, at, true)?));
                start = *at;
            }
            ',' | '}' if in_group => {
                sequence.push(Piece::Text(&text[start..*at]));
                alternatives.push(std::mem::take(&mut sequence));
                *at += 1;
                start = *at;
                if c == '}' {
                    return Some(alternatives);
                }
            }
            '}' => return None,
            _ => *at += c.len_utf8(),
        }
    }
}

/// The length of the character class at the start of `rest` (`[a-z]`, `[!a]`, `[]a]`); `None`
/// when it is not closed.
fn class_len(rest: &str) -> Option<usize> {
    let body = rest[1..].strip_prefix(['!', '^']).unwrap_or(&rest[1..]);
    let body = body.strip_prefix(']').unwrap_or(body);
    let end = body.find(']')?;
    Some(rest.len() - body.len() + end + 1)
}

/// The patterns a sequence stands for: each group with an empty alternative gives one pattern
/// without it and one with the other alternatives.
fn render(sequence: &[Piece<'_>]) -> Vec<String> {
    sequence.iter().fold(vec![String::new()], |done, piece| {
        let options = match piece {
            Piece::Text(text) => vec![(*text).to_owned()],
            Piece::Group(alternatives) => group_options(alternatives),
        };
        done.iter()
            .flat_map(|head| options.iter().map(move |option| format!("{head}{option}")))
            .collect()
    })
}

fn group_options(alternatives: &[Vec<Piece<'_>>]) -> Vec<String> {
    let all: Vec<String> = alternatives.iter().flat_map(|a| render(a)).collect();
    if all.iter().all(|a| !a.is_empty()) {
        return vec![format!("{{{}}}", all.join(","))];
    }
    let present: Vec<String> = all.into_iter().filter(|a| !a.is_empty()).collect();
    match present.len() {
        0 => vec![String::new()],
        1 => vec![String::new(), present[0].clone()],
        _ => vec![String::new(), format!("{{{}}}", present.join(","))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_alternatives_expand_and_other_patterns_stay() {
        for (pattern, expanded) in [
            ("pkg:f", vec!["pkg:f"]),
            ("a{b,c}d", vec!["a{b,c}d"]),
            ("a{,.*}", vec!["a", "a.*"]),
            ("a{.*,}", vec!["a", "a.*"]),
            ("a{,b,c}", vec!["a", "a{b,c}"]),
            ("a{}", vec!["a"]),
            ("{,a}{,b}", vec!["", "b", "a", "ab"]),
            ("x{a{,b},c}", vec!["x{a,ab,c}"]),
            ("x{y,{,z}}", vec!["x", "x{y,z}"]),
            ("a[{,]b", vec!["a[{,]b"]),
            ("a[!]{,]b", vec!["a[!]{,]b"]),
            ("é{,ü}", vec!["é", "éü"]),
        ] {
            assert_eq!(expand(pattern), expanded, "{pattern}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn an_escaped_brace_opens_no_group() {
        assert_eq!(expand("a\\{,b"), vec!["a\\{,b"]);
        assert_eq!(expand("a\\{{,b}"), vec!["a\\{", "a\\{b"]);
    }

    #[test]
    fn unbalanced_patterns_are_left_to_the_glob_parser() {
        for pattern in [
            "a{,b", "a},b", "a{,b}}", "}a{,b}", "a[{,b", "a{b[,c}", "a{,\\",
        ] {
            assert_eq!(expand(pattern), vec![pattern], "{pattern}");
        }
    }

    #[test]
    fn a_pattern_with_an_empty_alternative_matches_the_name_and_what_follows() {
        let any = AnyGlob::new("outer{,.*}").unwrap();
        assert!(any.is_match("outer") && any.is_match("outer.<locals>.inner"));
        assert!(!any.is_match("outers") && !any.is_match("other"));
        assert_eq!(any.globs().count(), 2);
        let mut set = GlobSetBuilder::new();
        add_to_set(&mut set, "a{,b}").unwrap();
        assert!(set.build().unwrap().is_match("a"));
        assert!(AnyGlob::new("a{,b").is_err());
    }
}
