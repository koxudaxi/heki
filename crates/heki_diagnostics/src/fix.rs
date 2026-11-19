//! Applying fixes (`heki check --fix`) and previewing them as unified diffs (`--diff`)
//! (docs/DETAILED_SPEC_M3_M7.md section 4.6).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Myers' O((N+M)D) line diff of `a` and `b`.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let max = n + m;
    let off = max as isize + 1;
    let idx = |k: isize| (k + off) as usize;
    let mut v = vec![0usize; 2 * max + 3];
    let mut trace: Vec<Vec<usize>> = Vec::new();
    'search: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) {
                v[idx(k + 1)]
            } else {
                v[idx(k - 1)] + 1
            };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[idx(k)] = x;
            if x >= n && y >= m {
                break 'search;
            }
            k += 2;
        }
    }
    // Backtrack from (n, m) through the saved frontiers.
    let mut ops = Vec::new();
    let (mut x, mut y) = (n as isize, m as isize);
    for (d, vd) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let prev_k = if k == -d || (k != d && vd[idx(k - 1)] < vd[idx(k + 1)]) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = vd[idx(prev_k)] as isize;
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(Op::Equal(x as usize, y as usize));
        }
        if d > 0 {
            if x == prev_x {
                ops.push(Op::Insert(prev_y as usize));
            } else {
                ops.push(Op::Delete(prev_x as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

fn push_line(out: &mut String, sign: char, line: &str) {
    out.push(sign);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
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
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
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
    }
}
