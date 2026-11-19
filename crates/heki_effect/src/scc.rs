//! Iterative Tarjan strongly connected components.

/// Strongly connected components of the graph `0..n` with successor lists `succ`.
///
/// Components are returned in **reverse topological order** of the condensation: every
/// component appears after all components it has edges into (callees before callers), which
/// is the bottom-up order the effect fixpoint needs. Members of each component are sorted.
pub(crate) fn tarjan_scc(n: usize, succ: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut out = Vec::new();
    // Explicit DFS frames `(node, next successor position)` avoid recursion depth limits.
    let mut frames: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        frames.push((root, 0));
        index[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;

        while let Some(&mut (v, ref mut pos)) = frames.last_mut() {
            if let Some(&w) = succ[v].get(*pos) {
                *pos += 1;
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                lowlink[parent] = lowlink[parent].min(lowlink[v]);
            }
            if lowlink[v] == index[v] {
                let mut component = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                component.sort_unstable();
                out.push(component);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tarjan_scc;

    #[test]
    fn components_come_callees_first() {
        // 0 -> 1 -> 2 -> 1, 2 -> 3, 4 isolated
        let succ = vec![vec![1], vec![2], vec![1, 3], vec![], vec![]];
        let sccs = tarjan_scc(5, &succ);
        let pos = |n: usize| sccs.iter().position(|c| c.contains(&n)).unwrap();
        assert!(sccs.contains(&vec![1, 2]));
        assert!(pos(3) < pos(1));
        assert!(pos(1) < pos(0));
        assert_eq!(sccs.iter().map(Vec::len).sum::<usize>(), 5);
    }

    #[test]
    fn deep_chain_does_not_overflow() {
        let n = 200_000;
        let succ: Vec<Vec<usize>> = (0..n)
            .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
            .collect();
        let sccs = tarjan_scc(n, &succ);
        assert_eq!(sccs.len(), n);
        assert_eq!(sccs[0], vec![n - 1]);
    }
}
