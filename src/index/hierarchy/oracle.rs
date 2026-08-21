//! Brute-force ground truth for subsumption and hierarchical roll-up.
//!
//! These are the **correctness oracles**: deliberately simple BFS and fold, written so
//! they are obviously right rather than fast. Every OEH encoding is validated against them
//! — all pairs on fixture and generated posets (ADR-035 §7).
//!
//! They ship in the library rather than in `tests/` on purpose: the benchmark harness
//! validates its own answers against the oracle at load time, and a `VALIDATE HIERARCHY`
//! path needs them at runtime.

use std::collections::{HashSet, VecDeque};

use super::monoid::{RollupOp, RollupValue};
use super::poset::Poset;

/// All `y` with `x ⊑ y`. With `reflexive`, includes `x` itself.
pub fn ancestors(p: &Poset, x: u32, reflexive: bool) -> HashSet<u32> {
    let mut seen: HashSet<u32> = HashSet::new();
    let mut q: VecDeque<u32> = p.parents(x).iter().copied().collect();
    while let Some(u) = q.pop_front() {
        if !seen.insert(u) {
            continue;
        }
        q.extend(p.parents(u).iter().copied());
    }
    if reflexive {
        seen.insert(x);
    }
    seen
}

/// All `z` with `z ⊑ x`. With `reflexive`, includes `x` itself.
pub fn descendants(p: &Poset, x: u32, reflexive: bool) -> HashSet<u32> {
    let mut seen: HashSet<u32> = HashSet::new();
    let mut q: VecDeque<u32> = p.children(x).iter().copied().collect();
    while let Some(u) = q.pop_front() {
        if !seen.insert(u) {
            continue;
        }
        q.extend(p.children(u).iter().copied());
    }
    if reflexive {
        seen.insert(x);
    }
    seen
}

/// `x ⊑ y`? Reflexive: `subsumes(x, x)` is true. BFS upward from `x` looking for `y`.
pub fn subsumes(p: &Poset, x: u32, y: u32) -> bool {
    if x == y {
        return true;
    }
    let mut seen: HashSet<u32> = HashSet::new();
    let mut q: VecDeque<u32> = p.parents(x).iter().copied().collect();
    while let Some(u) = q.pop_front() {
        if u == y {
            return true;
        }
        if !seen.insert(u) {
            continue;
        }
        q.extend(p.parents(u).iter().copied());
    }
    false
}

/// Fold a per-node measure over `{x} ∪ descendants(x)` with a monoid.
///
/// This is OLAP roll-up computed the slow honest way: materialize the descendant **set**
/// (so each node counts once no matter how many paths reach it) and fold it.
pub fn rollup(p: &Poset, x: u32, measure: &[Option<RollupValue>], op: RollupOp) -> RollupValue {
    let nodes = descendants(p, x, true);
    let mut acc = op.identity();
    for z in nodes {
        if let Some(v) = measure.get(z as usize).and_then(|m| m.as_ref()) {
            acc = op.combine(acc, *v);
        }
    }
    acc
}

/// Lowest common ancestors of `x` and `y`: the minimal elements of the intersection of
/// their reflexive ancestor sets. On a tree this is the single classical LCA; on a DAG
/// there may be several, which is why this returns a set.
pub fn lowest_common_ancestors(p: &Poset, x: u32, y: u32) -> Vec<u32> {
    let ax = ancestors(p, x, true);
    let ay = ancestors(p, y, true);
    let common: HashSet<u32> = ax.intersection(&ay).copied().collect();
    let mut minimal: Vec<u32> = common
        .iter()
        .copied()
        .filter(|&c| {
            // c is minimal in `common` iff no other member of common is strictly below it
            !common.iter().any(|&d| d != c && subsumes(p, d, c))
        })
        .collect();
    minimal.sort_unstable();
    minimal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::NodeId;

    fn nid(i: u64) -> NodeId {
        NodeId(i)
    }

    /// Diamond: 3 ⊑ {1,2} ⊑ 0.
    fn diamond() -> Poset {
        Poset::from_edges(
            vec![
                (nid(3), nid(1)),
                (nid(3), nid(2)),
                (nid(1), nid(0)),
                (nid(2), nid(0)),
            ],
            std::iter::empty(),
        )
        .unwrap()
    }

    #[test]
    fn subsumption_is_reflexive_and_transitive() {
        let p = diamond();
        let (r, a, _b, d) = (
            p.idx(nid(0)).unwrap(),
            p.idx(nid(1)).unwrap(),
            p.idx(nid(2)).unwrap(),
            p.idx(nid(3)).unwrap(),
        );
        assert!(subsumes(&p, d, d));
        assert!(subsumes(&p, d, a));
        assert!(subsumes(&p, d, r), "transitive through both paths");
        assert!(!subsumes(&p, r, d), "antisymmetric");
    }

    #[test]
    fn descendants_of_root_is_everything() {
        let p = diamond();
        let r = p.idx(nid(0)).unwrap();
        assert_eq!(descendants(&p, r, true).len(), 4);
        assert_eq!(descendants(&p, r, false).len(), 3);
    }

    #[test]
    fn rollup_counts_a_diamond_node_once() {
        // This is the double-count trap stated as an oracle property: node 3 is reachable
        // from 0 along two paths but must contribute to the sum exactly once.
        let p = diamond();
        let r = p.idx(nid(0)).unwrap();
        let mut measure = vec![None; p.n()];
        for i in 0..p.n() as u32 {
            measure[i as usize] = Some(RollupValue::Int(1));
        }
        assert_eq!(rollup(&p, r, &measure, RollupOp::Sum), RollupValue::Int(4));
    }

    #[test]
    fn lca_on_a_tree_is_unique() {
        // 3,4 ⊑ 1 ⊑ 0 ; 5 ⊑ 2 ⊑ 0
        let p = Poset::from_edges(
            vec![
                (nid(3), nid(1)),
                (nid(4), nid(1)),
                (nid(1), nid(0)),
                (nid(5), nid(2)),
                (nid(2), nid(0)),
            ],
            std::iter::empty(),
        )
        .unwrap();
        let lca = lowest_common_ancestors(&p, p.idx(nid(3)).unwrap(), p.idx(nid(4)).unwrap());
        assert_eq!(lca, vec![p.idx(nid(1)).unwrap()]);
        let lca2 = lowest_common_ancestors(&p, p.idx(nid(3)).unwrap(), p.idx(nid(5)).unwrap());
        assert_eq!(lca2, vec![p.idx(nid(0)).unwrap()]);
    }

    #[test]
    fn lca_on_a_dag_can_be_a_set() {
        // 2 ⊑ {0,1}; 0 and 1 are unrelated roots — the LCA of 2 with itself is 2,
        // and the common ancestors of two nodes both under {0,1} are both roots.
        let p = Poset::from_edges(
            vec![
                (nid(2), nid(0)),
                (nid(2), nid(1)),
                (nid(3), nid(0)),
                (nid(3), nid(1)),
            ],
            std::iter::empty(),
        )
        .unwrap();
        let lca = lowest_common_ancestors(&p, p.idx(nid(2)).unwrap(), p.idx(nid(3)).unwrap());
        assert_eq!(lca.len(), 2, "two incomparable minimal common ancestors");
    }
}
