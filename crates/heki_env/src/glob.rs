//! The glob syntax of every configured pattern: `globset`'s, with one addition. `globset` drops
//! an empty alternative (`{,.*}` matches `.x` but not the empty name), where a shell's brace
//! expansion keeps it, so `outer{,.*}` names `outer` and everything below it. A pattern with an
//! empty alternative stands for the globs it expands to.

use globset::{Glob, GlobMatcher, GlobSetBuilder};

/// How many globs one pattern may stand for. Each group with an empty alternative doubles them,
/// so a pattern of many such groups is refused instead of expanded.
pub const MAX_GLOBS: usize = 256;

/// Why a pattern does not compile.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Glob(#[from] globset::Error),
    #[error(
        "it stands for more than {MAX_GLOBS} globs: write fewer groups with an empty alternative"
    )]
    TooManyGlobs,
}

/// Whether `\` escapes the next character (as `globset` reads it; a Windows path separator
/// otherwise).
const BACKSLASH_ESCAPES: bool = cfg!(not(windows));

/// A pattern: the globs it stands for, one of which has to match.
#[derive(Debug, Clone)]
pub struct AnyGlob {
    matchers: Vec<GlobMatcher>,
}

impl AnyGlob {
    pub fn new(pattern: &str) -> Result<Self, Error> {
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
pub fn globs(pattern: &str) -> Result<Vec<Glob>, Error> {
    let expanded = expand(pattern).ok_or(Error::TooManyGlobs)?;
    Ok(expanded
        .iter()
        .map(|glob| Glob::new(glob))
        .collect::<Result<_, _>>()?)
}

/// Add the globs of `pattern` to `set`.
pub fn add_to_set(set: &mut GlobSetBuilder, pattern: &str) -> Result<(), Error> {
    for glob in globs(pattern)? {
        set.add(glob);
    }
    Ok(())
}

/// The names `pattern` lists, each as the patterns without a `{...}` group it is written out
/// as, the same alternatives [`globs`] gives `globset` to match. A group without an empty
/// alternative lists one name per alternative (`Base.{a,c}` lists `Base.a` and `Base.c`,
/// `pkg.{a.Base,a.Sub}` lists `pkg.a.Base` and `pkg.a.Sub`). A group with one is one name and its
/// extensions, written out with the name first (`m:f{,.*}` is `m:f` and `m:f.*`). `None` for
/// text `globset` rejects (an unclosed group or class), and when it stands for more than
/// [`MAX_GLOBS`] patterns.
pub fn alternatives(pattern: &str) -> Option<Vec<Vec<String>>> {
    let mut at = 0;
    let alternatives = parse(pattern, &mut at, false)?;
    let sequence: Vec<Piece<'_>> = alternatives.into_iter().flatten().collect();
    write_out(&sequence)
}

fn write_out(sequence: &[Piece<'_>]) -> Option<Vec<Vec<String>>> {
    let mut done = vec![vec![String::new()]];
    for piece in sequence {
        let options = match piece {
            Piece::Text(text) => vec![vec![(*text).to_owned()]],
            Piece::Group(alternatives) => {
                let mut names = Vec::new();
                for alternative in alternatives {
                    names.extend(write_out(alternative)?);
                }
                if names.iter().flatten().any(String::is_empty) {
                    let mut forms: Vec<String> = names.into_iter().flatten().collect();
                    forms.sort_by_key(|form| !form.is_empty());
                    forms.dedup();
                    vec![forms]
                } else {
                    names
                }
            }
        };
        let count = |names: &[Vec<String>]| names.iter().map(Vec::len).sum::<usize>();
        if count(&done) * count(&options) > MAX_GLOBS {
            return None;
        }
        done = (done.iter())
            .flat_map(|head| options.iter().map(move |tail| joined(head, tail)))
            .collect();
    }
    Some(done)
}

/// The forms of a name followed by a name: each form of `head` followed by each of `tail`.
fn joined(head: &[String], tail: &[String]) -> Vec<String> {
    (head.iter())
        .flat_map(|h| tail.iter().map(move |t| format!("{h}{t}")))
        .collect()
}

/// The patterns without an empty alternative that `pattern` stands for: the pattern itself
/// unless a `{...}` group has one. Text `globset` rejects (an unclosed group or class) is
/// returned as it is, for `globset` to report. `None` when it stands for more than
/// [`MAX_GLOBS`].
fn expand(pattern: &str) -> Option<Vec<String>> {
    if !pattern.contains('{') {
        return Some(vec![pattern.to_owned()]);
    }
    let mut at = 0;
    let Some(alternatives) = parse(pattern, &mut at, false) else {
        return Some(vec![pattern.to_owned()]);
    };
    // Outside a group a comma separates nothing, so the pattern is one sequence.
    let sequence: Vec<Piece<'_>> = alternatives.into_iter().flatten().collect();
    render(&sequence)
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
/// `None` as soon as they are more than [`MAX_GLOBS`].
fn render(sequence: &[Piece<'_>]) -> Option<Vec<String>> {
    let mut done = vec![String::new()];
    for piece in sequence {
        let options = match piece {
            Piece::Text(text) => vec![(*text).to_owned()],
            Piece::Group(alternatives) => group_options(alternatives)?,
        };
        if done.len() * options.len() > MAX_GLOBS {
            return None;
        }
        done = (done.iter())
            .flat_map(|head| options.iter().map(move |option| format!("{head}{option}")))
            .collect();
    }
    Some(done)
}

fn group_options(alternatives: &[Vec<Piece<'_>>]) -> Option<Vec<String>> {
    let mut all = Vec::new();
    for alternative in alternatives {
        all.extend(render(alternative)?);
        if all.len() > MAX_GLOBS {
            return None;
        }
    }
    if all.iter().all(|a| !a.is_empty()) {
        return Some(vec![format!("{{{}}}", all.join(","))]);
    }
    let present: Vec<String> = all.into_iter().filter(|a| !a.is_empty()).collect();
    Some(match present.len() {
        0 => vec![String::new()],
        1 => vec![String::new(), present[0].clone()],
        _ => vec![String::new(), format!("{{{}}}", present.join(","))],
    })
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
            let expanded: Vec<String> = expanded.into_iter().map(str::to_owned).collect();
            assert_eq!(expand(pattern), Some(expanded), "{pattern}");
        }
    }

    #[test]
    fn alternatives_list_names_with_their_extensions() {
        for (pattern, written) in [
            ("pkg.f", vec![vec!["pkg.f"]]),
            (
                "pkg.{a.Base,a.Sub}",
                vec![vec!["pkg.a.Base"], vec!["pkg.a.Sub"]],
            ),
            ("x{a{,b},c}", vec![vec!["xa", "xab"], vec!["xc"]]),
            ("m:f{.*,}", vec![vec!["m:f", "m:f.*"]]),
            ("x{y,{,z}}", vec![vec!["x", "xy", "xz"]]),
            ("a{,}", vec![vec!["a"]]),
            ("{a,b}{,.*}", vec![vec!["a", "a.*"], vec!["b", "b.*"]]),
        ] {
            let written: Vec<Vec<String>> = (written.into_iter())
                .map(|forms| forms.into_iter().map(str::to_owned).collect())
                .collect();
            assert_eq!(alternatives(pattern), Some(written), "{pattern}");
        }
        assert_eq!(alternatives("x{a"), None);
        assert_eq!(alternatives(&"{a,b}".repeat(9)), None);
        assert_eq!(alternatives(&"{,a}".repeat(9)), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn an_escaped_brace_opens_no_group() {
        assert_eq!(expand("a\\{,b"), Some(vec!["a\\{,b".to_owned()]));
        assert_eq!(
            expand("a\\{{,b}"),
            Some(vec!["a\\{".to_owned(), "a\\{b".to_owned()])
        );
    }

    #[test]
    fn unbalanced_patterns_are_left_to_the_glob_parser() {
        for pattern in [
            "a{,b", "a},b", "a{,b}}", "}a{,b}", "a[{,b", "a{b[,c}", "a{,\\",
        ] {
            assert_eq!(expand(pattern), Some(vec![pattern.to_owned()]), "{pattern}");
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

    #[test]
    fn a_pattern_standing_for_too_many_globs_is_refused() {
        let eight = "{,a}".repeat(8);
        assert_eq!(globs(&eight).unwrap().len(), MAX_GLOBS);
        let twenty = "{,a}".repeat(20);
        assert!(matches!(globs(&twenty), Err(Error::TooManyGlobs)));
        let deep = format!("x{{{}}}", vec!["{,a}".repeat(9); 2].join(","));
        assert!(matches!(AnyGlob::new(&deep), Err(Error::TooManyGlobs)));
        // Each alternative stands for 128 globs, the group for 384.
        let wide = format!("x{{{}}}", vec!["{,a}".repeat(7); 3].join(","));
        assert!(matches!(AnyGlob::new(&wide), Err(Error::TooManyGlobs)));
    }
}
