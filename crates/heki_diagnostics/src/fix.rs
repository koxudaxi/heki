//! Applying fixes (`heki check --fix`) and previewing them as unified diffs (`--diff`)
//! (docs/fixes.md).
//!
//! Every fix is a group of [`TextEdit`]s applied together or not at all. Groups are accepted in
//! source order; a group that overlaps an accepted edit is skipped (safety first: a later run
//! re-proves it on the new text), and the accepted edits are applied back to front.

use crate::TextEdit;
use std::fmt::Write as _;

fn conflicts(a: &TextEdit, b: &TextEdit) -> bool {
    a.span.start == b.span.start || (a.span.start < b.span.end && b.span.start < a.span.end)
}

fn valid(source: &str, e: &TextEdit) -> bool {
    let (s, t) = (e.span.start as usize, e.span.end as usize);
    s <= t && t <= source.len() && source.is_char_boundary(s) && source.is_char_boundary(t)
}

/// Apply the fix groups that do not overlap; returns the new text and the indices of the
/// applied groups.
pub fn apply_fixes(source: &str, groups: &[&[TextEdit]]) -> (String, Vec<usize>) {
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&i| {
        groups[i]
            .iter()
            .map(|e| e.span.start)
            .min()
            .unwrap_or(u32::MAX)
    });
    let mut accepted: Vec<&TextEdit> = Vec::new();
    let mut applied = Vec::new();
    for i in order {
        let g = groups[i];
        let self_overlap = g
            .iter()
            .enumerate()
            .any(|(j, a)| g.iter().skip(j + 1).any(|b| conflicts(a, b)));
        if g.is_empty()
            || self_overlap
            || g.iter().any(|e| !valid(source, e))
            || g.iter().any(|e| accepted.iter().any(|a| conflicts(a, e)))
        {
            continue;
        }
        accepted.extend(g.iter());
        applied.push(i);
    }
    accepted.sort_by_key(|e| std::cmp::Reverse(e.span.start));
    let mut out = source.to_owned();
    for e in accepted {
        out.replace_range(e.span.start as usize..e.span.end as usize, &e.replacement);
    }
    applied.sort_unstable();
    (out, applied)
}

/// One `--fix` pass over a Python file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonFixPass {
    pub text: String,
    pub applied: Vec<usize>,
    /// Set when the fixes together would not parse: a fix must never break a file, so nothing
    /// is applied (the message names the file and the first syntax error).
    pub refused: Option<String>,
}

/// [`apply_fixes`] to a Python source, refusing every fix of the pass when the result would
/// not parse. `shown_path` names the file in the refusal.
pub fn fix_python_source(shown_path: &str, source: &str, groups: &[&[TextEdit]]) -> PythonFixPass {
    let (text, applied) = apply_fixes(source, groups);
    match heki_ast::parse_module(&text) {
        Ok(_) => PythonFixPass {
            text,
            applied,
            refused: None,
        },
        Err(errors) => PythonFixPass {
            text: source.to_owned(),
            applied: Vec::new(),
            refused: Some(format!(
                "{shown_path}: the fixes would make the file invalid Python ({}); it was left unchanged. This is a heki bug: please report it",
                errors
                    .first()
                    .map_or("syntax error", |e| e.message.as_str())
            )),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Line diff of `a` and `b`: Myers' O((N+M)D) algorithm in linear space (common prefix and
/// suffix stripped, then divided at a point of the middle snake, as diff-match-patch's
/// `diff_bisect` does). The trace of the greedy variant takes O((N+M)D) memory: gigabytes for a
/// generated file with thousands of fixes. Within each run of changed lines, deletions come
/// before insertions.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_range(a, b, (0, a.len()), (0, b.len()), &mut ops);
    let mut start = 0;
    while start < ops.len() {
        let end = ops[start..]
            .iter()
            .position(|o| matches!(o, Op::Equal(..)))
            .map_or(ops.len(), |i| start + i);
        ops[start..end].sort_by_key(|o| matches!(o, Op::Insert(_)));
        start = end + 1;
    }
    ops
}

/// [`diff_ops`] of `a[ra.0..ra.1]` and `b[rb.0..rb.1]`, appended to `ops`.
fn diff_range(
    a: &[&str],
    b: &[&str],
    (mut a0, mut a1): (usize, usize),
    (mut b0, mut b1): (usize, usize),
    ops: &mut Vec<Op>,
) {
    while a0 < a1 && b0 < b1 && a[a0] == b[b0] {
        ops.push(Op::Equal(a0, b0));
        a0 += 1;
        b0 += 1;
    }
    let mut common_suffix = 0;
    while a0 < a1 && b0 < b1 && a[a1 - 1] == b[b1 - 1] {
        a1 -= 1;
        b1 -= 1;
        common_suffix += 1;
    }
    if a0 == a1 {
        ops.extend((b0..b1).map(Op::Insert));
    } else if b0 == b1 {
        ops.extend((a0..a1).map(Op::Delete));
    } else {
        let (n, m) = (a1 - a0, b1 - b0);
        match middle_split(&a[a0..a1], &b[b0..b1]).filter(|&p| p != (0, 0) && p != (n, m)) {
            Some((x, y)) => {
                diff_range(a, b, (a0, a0 + x), (b0, b0 + y), ops);
                diff_range(a, b, (a0 + x, a1), (b0 + y, b1), ops);
            }
            None => {
                ops.extend((a0..a1).map(Op::Delete));
                ops.extend((b0..b1).map(Op::Insert));
            }
        }
    }
    ops.extend((0..common_suffix).map(|i| Op::Equal(a1 + i, b1 + i)));
}

/// A point `(x, y)` strictly inside the edit graph of `a` and `b` (both non-empty, without a
/// common first or last line) that an optimal path passes: where the furthest-reaching paths
/// searched from both ends meet. `None` only if they never meet, which Myers' bound rules out.
fn middle_split(a: &[&str], b: &[&str]) -> Option<(usize, usize)> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max_d = (n + m + 1) / 2;
    let offset = max_d;
    let len = 2 * max_d + 2;
    // The furthest x reached on each diagonal `k` (index `offset + k`); -1: not reached yet.
    let mut forward = vec![-1isize; len as usize];
    let mut backward = vec![-1isize; len as usize];
    forward[(offset + 1) as usize] = 0;
    backward[(offset + 1) as usize] = 0;
    let delta = n - m;
    // With an odd delta the paths meet in a forward step, else in a backward one.
    let meets_forward = delta % 2 != 0;
    // Diagonals that ran off the graph (right, bottom) are skipped from then on.
    let (mut f_start, mut f_end, mut b_start, mut b_end) = (0, 0, 0, 0);
    let at = |k: isize| (offset + k) as usize;
    let inside = |i: isize| (0..len).contains(&i);
    for d in 0..max_d {
        let mut k = -d + f_start;
        while k <= d - f_end {
            let mut x = if k == -d || (k != d && forward[at(k - 1)] < forward[at(k + 1)]) {
                forward[at(k + 1)]
            } else {
                forward[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            forward[at(k)] = x;
            if x > n {
                f_end += 2;
            } else if y > m {
                f_start += 2;
            } else if meets_forward {
                let kb = offset + delta - k;
                if inside(kb) && backward[kb as usize] != -1 && x >= n - backward[kb as usize] {
                    return Some((x as usize, y as usize));
                }
            }
            k += 2;
        }
        let mut k = -d + b_start;
        while k <= d - b_end {
            let mut x = if k == -d || (k != d && backward[at(k - 1)] < backward[at(k + 1)]) {
                backward[at(k + 1)]
            } else {
                backward[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[(n - x - 1) as usize] == b[(m - y - 1) as usize] {
                x += 1;
                y += 1;
            }
            backward[at(k)] = x;
            if x > n {
                b_end += 2;
            } else if y > m {
                b_start += 2;
            } else if !meets_forward {
                let kf = offset + delta - k;
                if inside(kf) && forward[kf as usize] != -1 {
                    let fx = forward[kf as usize];
                    let fy = offset + fx - kf;
                    if fx >= n - x {
                        return Some((fx as usize, fy as usize));
                    }
                }
            }
            k += 2;
        }
    }
    None
}

fn push_line(out: &mut String, sign: char, line: &str) {
    out.push(sign);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
}

/// `prefix` + `path` as a diff header names a file: quoted with C escapes, as `git diff`
/// writes it, when the name holds a `"`, a `\\`, or a control character, so a patch tool reads
/// the name it holds.
fn header_path(prefix: &str, path: &str) -> String {
    let name = format!("{prefix}{path}");
    if !name
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return name;
    }
    let mut out = String::from('"');
    for c in name.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let mut buf = [0; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    let _ = write!(out, "\\{b:03o}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Unified diff (`--- a/<path>` / `+++ b/<path>`, 3 lines of context); empty if equal.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = diff_ops(&a, &b);
    const CONTEXT: usize = 3;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| !matches!(o, Op::Equal(..)))
        .map(|(i, _)| i)
        .collect();
    let mut out = format!(
        "--- {}\n+++ {}\n",
        header_path("a/", path),
        header_path("b/", path)
    );
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CONTEXT);
        let mut end = changed[i];
        while i < changed.len() && changed[i] <= end + 2 * CONTEXT + 1 {
            end = changed[i];
            i += 1;
        }
        let end = (end + CONTEXT + 1).min(ops.len());
        let hunk = &ops[start..end];
        // Hunk header positions (1-based; 0 when a side has no lines).
        let (mut a_start, mut b_start) = (None, None);
        let (mut a_len, mut b_len) = (0, 0);
        for op in hunk {
            match *op {
                Op::Equal(x, y) => {
                    a_start.get_or_insert(x);
                    b_start.get_or_insert(y);
                    a_len += 1;
                    b_len += 1;
                }
                Op::Delete(x) => {
                    a_start.get_or_insert(x);
                    a_len += 1;
                }
                Op::Insert(y) => {
                    b_start.get_or_insert(y);
                    b_len += 1;
                }
            }
        }
        // 1-based start lines. Hunks carry their context lines, so a side without lines in a
        // hunk is an empty file, reported at line 0.
        let start = |s: Option<usize>| s.map_or(0, |s| s + 1);
        let _ = writeln!(
            out,
            "@@ -{},{a_len} +{},{b_len} @@",
            start(a_start),
            start(b_start),
        );
        for op in hunk {
            match *op {
                Op::Equal(x, _) => push_line(&mut out, ' ', a[x]),
                Op::Delete(x) => push_line(&mut out, '-', a[x]),
                Op::Insert(y) => push_line(&mut out, '+', b[y]),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use heki_ast::Span;

    fn edit(start: u32, end: u32, r: &str) -> TextEdit {
        TextEdit {
            span: Span { start, end },
            replacement: r.into(),
        }
    }

    #[test]
    fn overlapping_groups_are_skipped_and_groups_are_atomic() {
        let src = "abcdef";
        let g1 = [edit(0, 2, "X")];
        let g2 = [edit(1, 3, "Y")];
        let g3 = [edit(4, 4, "+"), edit(5, 6, "Z")];
        let bad = [edit(5, 9, "!")];
        let (out, applied) = apply_fixes(src, &[&g1, &g2, &g3, &bad]);
        assert_eq!(out, "Xcd+eZ");
        assert_eq!(applied, vec![0, 2]);
        // Two insertions at one position are ambiguous: the later group waits for a rerun.
        let (out, applied) = apply_fixes("ab", &[&[edit(1, 1, "1")], &[edit(1, 1, "2")]]);
        assert_eq!((out.as_str(), applied), ("a1b", vec![0]));
    }

    /// The lines of `b` rebuilt from `a` by `ops`, and the number of changed lines.
    fn replay<'s>(a: &[&'s str], b: &[&'s str], ops: &[Op]) -> (Vec<&'s str>, usize) {
        let mut out = Vec::new();
        let mut changes = 0;
        for op in ops {
            match *op {
                Op::Equal(x, y) => {
                    assert_eq!(a[x], b[y]);
                    out.push(a[x]);
                }
                Op::Insert(y) => {
                    out.push(b[y]);
                    changes += 1;
                }
                Op::Delete(_) => changes += 1,
            }
        }
        (out, changes)
    }

    /// The fewest changed lines between `a` and `b` (dynamic programming on the LCS).
    fn fewest_changes(a: &[&str], b: &[&str]) -> usize {
        let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        a.len() + b.len() - 2 * lcs[0][0]
    }

    #[test]
    fn line_diffs_are_minimal_and_rebuild_the_new_text() {
        // Deterministic pseudo-random line sequences over a small alphabet (many matches).
        let mut seed = 7u64;
        let mut next = move |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % bound
        };
        let words = ["a", "b", "c", "d", "e"];
        for _ in 0..300 {
            let a: Vec<&str> = (0..next(12)).map(|_| words[next(5) as usize]).collect();
            let b: Vec<&str> = (0..next(12)).map(|_| words[next(5) as usize]).collect();
            let ops = diff_ops(&a, &b);
            let (rebuilt, changes) = replay(&a, &b, &ops);
            assert_eq!(rebuilt, b, "{a:?} -> {b:?}: {ops:?}");
            assert_eq!(changes, fewest_changes(&a, &b), "{a:?} -> {b:?}: {ops:?}");
        }
    }

    #[test]
    fn scattered_changes_of_a_large_file_diff_in_linear_space() {
        // 25,000 lines with every fifth one replaced: the greedy trace would hold 10,000 copies
        // of a 50,003-entry frontier.
        let old: String = (0..25_000).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..25_000)
            .map(|i| {
                if i % 5 == 0 {
                    format!("fixed {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        let d = unified_diff("big.py", &old, &new);
        assert_eq!(d.lines().filter(|l| l.starts_with('-')).count(), 5_001);
        assert_eq!(d.lines().filter(|l| l.starts_with('+')).count(), 5_001);
    }

    #[test]
    fn fixes_that_break_the_syntax_are_refused_together() {
        let src = "x = 1\ny = 2\n";
        let good = [edit(4, 5, "10")];
        let broken = [edit(10, 11, "(")];
        let pass = fix_python_source("m.py", src, &[&good]);
        assert_eq!(
            (pass.text.as_str(), pass.applied, pass.refused),
            ("x = 10\ny = 2\n", vec![0], None)
        );
        let pass = fix_python_source("m.py", src, &[&good, &broken]);
        assert_eq!((pass.text.as_str(), pass.applied), (src, vec![]));
        let why = pass.refused.unwrap();
        assert!(
            why.starts_with("m.py: the fixes would make the file invalid Python ("),
            "{why}"
        );
    }

    #[test]
    fn unified_diff_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk";
        let d = unified_diff("m.py", old, new);
        assert_eq!(
            d,
            "--- a/m.py\n+++ b/m.py\n@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n d\n e\n@@ -8,3 +8,4 @@\n h\n i\n j\n+k\n\\ No newline at end of file\n"
        );
        assert_eq!(unified_diff("m.py", old, old), "");
        let ins = unified_diff("m.py", "x\n", "y\nx\n");
        assert_eq!(ins, "--- a/m.py\n+++ b/m.py\n@@ -1,1 +1,2 @@\n+y\n x\n");
        let all = unified_diff("m.py", "", "z\n");
        assert_eq!(all, "--- a/m.py\n+++ b/m.py\n@@ -0,0 +1,1 @@\n+z\n");
        // Names a patch tool would misread are quoted the way `git diff` quotes them.
        let quoted = unified_diff("x\\\"y\tz\u{1}.py", "a\n", "b\n");
        assert!(
            quoted.starts_with(
                "--- \"a/x\\\\\\\"y\\tz\\001.py\"\n+++ \"b/x\\\\\\\"y\\tz\\001.py\"\n"
            ),
            "{quoted}"
        );
    }
}
