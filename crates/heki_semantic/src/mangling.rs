//! Python's private name mangling. A name written in the body of a class, or in a scope nested
//! in it, that starts with two underscores and does not end with two (`__cache`) is compiled as
//! `_Class__cache`: the class's name without its leading underscores, prefixed with one. A class
//! named with underscores only mangles nothing, and neither does a dotted name (the module of an
//! `import`).
//!
//! A definition keeps its written name in `__qualname__` (`Parser.__run`), while the class holds
//! it under the mangled one (`Parser._Parser__run`): [`member_path`] and [`source_path`] turn
//! one spelling of a dotted path into the other.

use std::borrow::Cow;

/// Whether the compiler mangles `name` written inside a class.
fn is_private(name: &str) -> bool {
    name.starts_with("__") && !name.ends_with("__") && !name.contains('.')
}

/// `name` as the compiler spells it when it is written inside the class named `class`.
pub fn mangle<'n>(class: &str, name: &'n str) -> Cow<'n, str> {
    let stripped = class.trim_start_matches('_');
    if stripped.is_empty() || !is_private(name) {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("_{stripped}{name}"))
    }
}

/// The name written inside the class named `class` that the compiler spells `member`
/// (`__run` of `_Parser__run`), if `member` is such a spelling.
pub fn unmangle<'m>(class: &str, member: &'m str) -> Option<&'m str> {
    let stripped = class.trim_start_matches('_');
    if stripped.is_empty() {
        return None;
    }
    let name = member.strip_prefix('_')?.strip_prefix(stripped)?;
    is_private(name).then_some(name)
}

/// Each way `member` reads as a private name mangled with a class: the class's name without
/// its leading underscores and the name written in the class (`("Parser", "__run")` of
/// `_Parser__run`; a class name may itself hold `__`, so there may be several).
pub fn unmangled(member: &str) -> impl Iterator<Item = (&str, &str)> {
    let rest = member.strip_prefix('_').filter(|r| !r.starts_with('_'));
    rest.into_iter().flat_map(|rest| {
        (1..rest.len())
            .filter(move |&i| rest.is_char_boundary(i) && is_private(&rest[i..]))
            .map(move |i| (&rest[..i], &rest[i..]))
    })
}

/// The name the class holding the definition `qualname` holds it under: its last name, mangled
/// with the class when it is private (`_Parser__run` of `Parser.__run`); a function's own
/// definitions (`f.<locals>.__g`) and module-level ones keep theirs.
pub fn member_name(qualname: &str) -> Cow<'_, str> {
    match qualname.rsplit_once('.') {
        Some((holder, name)) => {
            let class = holder.rsplit('.').next().unwrap_or(holder);
            if class == "<locals>" {
                Cow::Borrowed(name)
            } else {
                mangle(class, name)
            }
        }
        None => Cow::Borrowed(qualname),
    }
}

/// The dotted path `qualname` with each class member spelled as its class holds it
/// (`Parser.__run` is `Parser._Parser__run`); `None` when no member is mangled. A segment
/// followed by one other than `<locals>` is a class.
pub fn member_path(qualname: &str) -> Option<String> {
    respell(qualname, Spelling::Written, |class, segment| {
        Some(mangle(class, segment))
            .filter(|m| matches!(m, Cow::Owned(_)))
            .map(Cow::into_owned)
    })
}

/// The dotted path `path` with each class member spelled as it is written in its class
/// (`Parser._Parser__run` is `Parser.__run`), as the definition's qualname has it; `None` when
/// no member is a mangled name.
pub fn source_path(path: &str) -> Option<String> {
    respell(path, Spelling::Member, |class, segment| {
        unmangle(class, segment).map(str::to_owned)
    })
}

/// How a dotted path spells the members of its classes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spelling {
    Written,
    Member,
}

/// `path`, spelled `input`, with each member of a class segment respelled by `respell`, which
/// takes the class's written name (what the compiler mangles with) and the member; `None` when
/// it respells nothing.
fn respell(
    path: &str,
    input: Spelling,
    respell: impl Fn(&str, &str) -> Option<String>,
) -> Option<String> {
    if !path.contains("__") {
        return None;
    }
    let segments: Vec<&str> = path.split('.').collect();
    let mut out: Vec<String> = Vec::with_capacity(segments.len());
    let mut changed = false;
    for (i, &segment) in segments.iter().enumerate() {
        let class = i
            .checked_sub(1)
            .filter(|&p| segments[p] != "<locals>" && segment != "<locals>");
        let spelled = class.and_then(|p| match input {
            Spelling::Written => respell(segments[p], segment),
            Spelling::Member => respell(&out[p], segment),
        });
        changed |= spelled.is_some();
        out.push(spelled.unwrap_or_else(|| segment.to_owned()));
    }
    changed.then(|| out.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangles_private_names_only() {
        for (class, name, mangled) in [
            ("Parser", "__run", "_Parser__run"),
            ("_Parser", "__run", "_Parser__run"),
            ("__Parser", "___run", "_Parser___run"),
            ("Parser", "__init__", "__init__"),
            ("Parser", "__", "__"),
            ("Parser", "_run", "_run"),
            ("Parser", "__a.b", "__a.b"),
            ("___", "__run", "__run"),
        ] {
            assert_eq!(mangle(class, name), mangled, "{class} {name}");
        }
    }

    #[test]
    fn unmangles_its_own_spellings() {
        for (class, member, written) in [
            ("Parser", "_Parser__run", Some("__run")),
            ("__Parser", "_Parser__run", Some("__run")),
            ("Parser", "_Other__run", None),
            ("Parser", "_Parser__init__", None),
            ("Parser", "__run", None),
            ("Parser", "run", None),
            ("___", "___run", None),
            ("A__B", "_A__B__run", Some("__run")),
        ] {
            assert_eq!(unmangle(class, member), written, "{class} {member}");
        }
    }

    #[test]
    fn reads_every_class_a_member_may_be_mangled_with() {
        for (member, readings) in [
            ("_Parser__run", vec![("Parser", "__run")]),
            ("_A__B__run", vec![("A", "__B__run"), ("A__B", "__run")]),
            ("_A___run", vec![("A", "___run"), ("A_", "__run")]),
            ("__run", vec![]),
            ("_run", vec![]),
            ("_Parser__init__", vec![]),
            ("run", vec![]),
        ] {
            assert_eq!(unmangled(member).collect::<Vec<_>>(), readings, "{member}");
        }
    }

    #[test]
    fn names_a_definition_as_its_class_holds_it() {
        for (qualname, member) in [
            ("Parser.__run", "_Parser__run"),
            ("Outer.__Inner.__run", "_Inner__run"),
            ("Parser.run", "run"),
            ("f.<locals>.__g", "__g"),
            ("__g", "__g"),
        ] {
            assert_eq!(member_name(qualname), member, "{qualname}");
        }
    }

    #[test]
    fn respells_class_members_of_dotted_paths() {
        for (qualname, member) in [
            ("Parser.__run", Some("Parser._Parser__run")),
            ("Parser.run", None),
            ("Parser.__init__", None),
            ("__run", None),
            ("f.<locals>.__run", None),
            ("f.<locals>.C.__run", Some("f.<locals>.C._C__run")),
            (
                "Outer.__Inner.__run",
                Some("Outer._Outer__Inner._Inner__run"),
            ),
            ("C.m.<locals>.__g", None),
        ] {
            assert_eq!(member_path(qualname).as_deref(), member, "{qualname}");
            if let Some(member) = member {
                assert_eq!(source_path(member).as_deref(), Some(qualname), "{member}");
            }
        }
        assert_eq!(source_path("Parser.run"), None);
        assert_eq!(source_path("Parser._Other__run"), None);
    }
}
