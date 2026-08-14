//! OEH — the Order-Embedded Hierarchy index (ADR-035).
//!
//! One structure that answers **subsumption** *and* **index-resident monoid roll-up** over
//! a subsumption poset, choosing its encoding from a cheap structural probe:
//!
//! - **tree → nested-set**: a DFS `[tin, tout]` interval per node. Subsumption is 2-D
//!   interval containment (two integer comparisons). The descendants of `y` are the
//!   *contiguous* rank range `[tin[y], tout[y]]`, so a roll-up is a range query — a Fenwick
//!   difference in O(log n) for SUM, a sparse-table fold in O(1) for MIN/MAX, and a
//!   subtraction of two integers for COUNT. 2 ints/node.
//! - **low-width DAG → chain decomposition**: each node gets `(chain, pos)`;
//!   `reach[v][c]` is the minimum position on chain `c` reachable from `v`. Descendants on
//!   a chain are the contiguous *suffix* from that position, and because the chains
//!   partition the node set, folding per-chain suffixes visits every descendant exactly
//!   once — **exact and double-count-free** on multi-parent DAGs.
//! - **high-width DAG → declined**: above a `max(64, 8·√n)` chain-width cap the index
//!   refuses to build and says why. A 2-hop index is the right substrate there; see the
//!   honest-scope section of ADR-035.
//!
//! The roll-up being *index-resident* — read out of the structure rather than computed by
//! an engine aggregation the index merely filters — is the property that turns O(subtree)
//! into O(log n), and is what distinguishes this from an index-assisted hierarchy index.

use std::collections::HashMap;

use super::monoid::{Fenwick, RollupOp, RollupValue, SparseTable};
use super::poset::{HierarchyError, HierarchyResult, Poset};

/// Which encoding the structural probe selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Nested-set interval labeling — trees and forests.
    NestedSet,
    /// Jagadish chain decomposition — low-width DAGs.
    Chain,
}

impl Encoding {
    /// Name as it appears in `SHOW HIERARCHY INDEXES` and `EXPLAIN`.
    pub fn name(&self) -> &'static str {
        match self {
            Encoding::NestedSet => "nested-set",
            Encoding::Chain => "chain",
        }
    }
}

/// The verdict of the structural probe, before any encoding work happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Every node has at most one parent.
    Tree,
    /// Multi-parent, but chain width is within the cap.
    LowWidthDag { width: usize },
    /// Chain width exceeds the cap — defer to a 2-hop index.
    HighWidthDag { width: usize, cap: usize },
}

/// The width cap: chain-mode space is O(n·width), so capping width at ~8·√n keeps the
/// index at ~O(n^1.5). Small posets get a floor of 64 so that toy hierarchies and unit
/// tests are never declined for being small.
pub fn width_cap_for(n: usize) -> usize {
    std::cmp::max(64, (8.0 * (n as f64).sqrt()) as usize)
}

#[derive(Debug, Clone)]
enum EncodingData {
    NestedSet {
        /// `tin[i]` = DFS pre-order rank of dense node `i`.
        tin: Vec<u32>,
        /// `tout[i]` = maximum rank in the subtree of `i`.
        tout: Vec<u32>,
        /// `inv[rank]` = dense node index at that rank.
        inv: Vec<u32>,
    },
    Chain {
        /// `chain_of[i]` = `(chain id, position on that chain)`.
        chain_of: Vec<(u32, u32)>,
        /// The chains themselves, as dense node indices in order.
        chains: Vec<Vec<u32>>,
        /// `reach[i]` = sorted `(chain id, min reachable position)` pairs.
        reach: Vec<Vec<(u32, u32)>>,
    },
}

/// Roll-up structures for one monoid, in whichever encoding the index uses.
#[derive(Debug, Clone)]
enum RollupData {
    /// Range structure over nested-set ranks.
    Fenwick(Fenwick),
    /// Range structure over nested-set ranks for a non-invertible monoid.
    Sparse(SparseTable),
    /// Per-chain suffix folds.
    ChainSuffix(Vec<Vec<RollupValue>>),
}

/// The OEH index over one subsumption poset.
#[derive(Debug, Clone)]
pub struct OehIndex {
    poset: Poset,
    encoding: Encoding,
    data: EncodingData,
    /// Per-dense-index measure, as declared.
    measure: Option<Vec<Option<RollupValue>>>,
    /// Built range structures, keyed by monoid.
    rollups: HashMap<RollupOp, RollupData>,
    /// Chain width, when the chain encoding was selected.
    width: Option<usize>,
}

impl OehIndex {
    /// Run the structural probe without building anything.
    ///
    /// Cheap for the tree verdict (one pass over parent lists). For the DAG verdicts it
    /// runs the chain decomposition, which is O(n + m) — the expensive part of chain mode
    /// is the reachability fold, and that is exactly what we avoid paying before knowing
    /// whether the poset is in regime.
    pub fn probe(poset: &Poset) -> Probe {
        if poset.is_tree() {
            return Probe::Tree;
        }
        let chains = Self::decompose_chains(poset);
        let width = chains.len();
        let cap = width_cap_for(poset.n());
        if width > cap && poset.n() > 100 {
            Probe::HighWidthDag { width, cap }
        } else {
            Probe::LowWidthDag { width }
        }
    }

    /// Build the index, selecting the encoding from the probe.
    ///
    /// Returns [`HierarchyError::WidthTooHigh`] when the poset is out of regime. That is a
    /// supported outcome: the caller reports the diagnostic and the planner keeps using
    /// variable-length expansion.
    pub fn build(poset: Poset) -> HierarchyResult<Self> {
        match Self::probe(&poset) {
            Probe::Tree => Self::build_nested_set(poset),
            Probe::LowWidthDag { .. } => Self::build_chain(poset),
            Probe::HighWidthDag { width, cap } => Err(HierarchyError::WidthTooHigh {
                width,
                cap,
                nodes: poset.n(),
            }),
        }
    }

    /// Build with a forced encoding, bypassing the probe and the width cap.
    ///
    /// Used by tests and by the benchmark harness to measure the declined regime on
    /// purpose. `NestedSet` on a non-tree is rejected — that one is not a policy choice
    /// but an impossibility.
    pub fn build_forced(poset: Poset, encoding: Encoding) -> HierarchyResult<Self> {
        match encoding {
            Encoding::NestedSet if !poset.is_tree() => Err(HierarchyError::NotATree),
            Encoding::NestedSet => Self::build_nested_set(poset),
            Encoding::Chain => Self::build_chain(poset),
        }
    }

    // ---- nested-set --------------------------------------------------------

    fn build_nested_set(poset: Poset) -> HierarchyResult<Self> {
        let n = poset.n();
        let mut tin = vec![0u32; n];
        let mut tout = vec![0u32; n];
        let mut inv = vec![0u32; n];
        let mut counter: u32 = 0;

        // Iterative DFS. NCBI Taxonomy and calendar hierarchies are deep enough that a
        // recursive walk risks a stack overflow on a default 2 MiB thread stack.
        for root in poset.roots() {
            tin[root as usize] = counter;
            inv[counter as usize] = root;
            counter += 1;
            let mut stack: Vec<(u32, usize)> = vec![(root, 0)];
            while let Some(&mut (node, ref mut next_child)) = stack.last_mut() {
                let kids = poset.children(node);
                if *next_child < kids.len() {
                    let c = kids[*next_child];
                    *next_child += 1;
                    tin[c as usize] = counter;
                    inv[counter as usize] = c;
                    counter += 1;
                    stack.push((c, 0));
                } else {
                    tout[node as usize] = counter - 1;
                    stack.pop();
                }
            }
        }

        debug_assert_eq!(counter as usize, n, "every node gets exactly one rank");
        Ok(OehIndex {
            poset,
            encoding: Encoding::NestedSet,
            data: EncodingData::NestedSet { tin, tout, inv },
            measure: None,
            rollups: HashMap::new(),
            width: None,
        })
    }

    // ---- chain decomposition ----------------------------------------------

    /// Greedy path partition in parent-before-child order.
    fn decompose_chains(poset: &Poset) -> Vec<Vec<u32>> {
        let n = poset.n();
        let mut used = vec![false; n];
        let mut chains: Vec<Vec<u32>> = Vec::new();
        for u in poset.topo_down() {
            if used[u as usize] {
                continue;
            }
            let mut chain = Vec::new();
            let mut t = Some(u);
            while let Some(v) = t {
                if used[v as usize] {
                    break;
                }
                used[v as usize] = true;
                chain.push(v);
                t = poset
                    .children(v)
                    .iter()
                    .copied()
                    .find(|&c| !used[c as usize]);
            }
            chains.push(chain);
        }
        chains
    }

    fn build_chain(poset: Poset) -> HierarchyResult<Self> {
        let n = poset.n();
        let chains = Self::decompose_chains(&poset);
        let width = chains.len();

        let mut chain_of = vec![(0u32, 0u32); n];
        for (cid, chain) in chains.iter().enumerate() {
            for (pos, &v) in chain.iter().enumerate() {
                chain_of[v as usize] = (cid as u32, pos as u32);
            }
        }

        // reach[v][c] = min position on chain c reachable from v (v included).
        // Folded children-before-parents, so a node's children are complete when it is
        // visited. This is the O(n·width) part, which is why the cap is checked first.
        let mut reach_maps: Vec<HashMap<u32, u32>> = vec![HashMap::new(); n];
        for &v in poset.topo_up() {
            let (cid, pos) = chain_of[v as usize];
            let mut acc: HashMap<u32, u32> = HashMap::new();
            acc.insert(cid, pos);
            for &c in poset.children(v) {
                for (&cc, &mp) in reach_maps[c as usize].iter() {
                    acc.entry(cc)
                        .and_modify(|e| *e = (*e).min(mp))
                        .or_insert(mp);
                }
            }
            reach_maps[v as usize] = acc;
        }

        // Compact each map into a sorted vector: smaller, cache-friendlier, and a chain
        // probe becomes a binary search over a contiguous slice.
        let reach: Vec<Vec<(u32, u32)>> = reach_maps
            .into_iter()
            .map(|m| {
                let mut v: Vec<(u32, u32)> = m.into_iter().collect();
                v.sort_unstable_by_key(|&(c, _)| c);
                v
            })
            .collect();

        Ok(OehIndex {
            poset,
            encoding: Encoding::Chain,
            data: EncodingData::Chain {
                chain_of,
                chains,
                reach,
            },
            measure: None,
            rollups: HashMap::new(),
            width: Some(width),
        })
    }

    // ---- accessors ---------------------------------------------------------

    /// The underlying poset.
    pub fn poset(&self) -> &Poset {
        &self.poset
    }

    /// Which encoding was selected.
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Chain width, if the chain encoding was selected.
    pub fn width(&self) -> Option<usize> {
        self.width
    }

    // ---- subsumption -------------------------------------------------------

    /// `x ⊑ y`? Dense indices. Reflexive.
    pub fn subsumes(&self, x: u32, y: u32) -> bool {
        match &self.data {
            EncodingData::NestedSet { tin, tout, .. } => {
                tin[y as usize] <= tin[x as usize] && tout[x as usize] <= tout[y as usize]
            }
            EncodingData::Chain {
                chain_of, reach, ..
            } => {
                let (cid, pos) = chain_of[x as usize];
                let ry = &reach[y as usize];
                match ry.binary_search_by_key(&cid, |&(c, _)| c) {
                    Ok(i) => ry[i].1 <= pos,
                    Err(_) => false,
                }
            }
        }
    }

    /// `x ⊑ y` by graph node id. Returns `None` if either node is outside the hierarchy —
    /// which is a different answer from `false` and the planner must not conflate them.
    pub fn subsumes_ids(
        &self,
        x: crate::graph::types::NodeId,
        y: crate::graph::types::NodeId,
    ) -> Option<bool> {
        let (xi, yi) = (self.poset.idx(x)?, self.poset.idx(y)?);
        Some(self.subsumes(xi, yi))
    }

    // ---- descendants -------------------------------------------------------

    /// Enumerate `{y} ∪ descendants(y)` from the index, without touching adjacency lists.
    ///
    /// In nested-set mode this is a contiguous slice of the rank→node inverse. In chain
    /// mode it is one suffix per reachable chain; because chains partition the nodes,
    /// the result contains no duplicates.
    pub fn descendants(&self, y: u32) -> Vec<u32> {
        match &self.data {
            EncodingData::NestedSet { tin, tout, inv } => {
                let (lo, hi) = (tin[y as usize] as usize, tout[y as usize] as usize);
                inv[lo..=hi].to_vec()
            }
            EncodingData::Chain { chains, reach, .. } => {
                let mut out = Vec::new();
                for &(cid, mp) in &reach[y as usize] {
                    out.extend_from_slice(&chains[cid as usize][mp as usize..]);
                }
                out
            }
        }
    }

    /// Size of `{y} ∪ descendants(y)`, answered structurally — no measure needed.
    ///
    /// COUNT never requires a declared measure: in nested-set mode it is a subtraction of
    /// two ranks, in chain mode a sum of suffix lengths.
    pub fn descendant_count(&self, y: u32) -> usize {
        match &self.data {
            EncodingData::NestedSet { tin, tout, .. } => {
                (tout[y as usize] - tin[y as usize]) as usize + 1
            }
            EncodingData::Chain { chains, reach, .. } => reach[y as usize]
                .iter()
                .map(|&(cid, mp)| chains[cid as usize].len() - mp as usize)
                .sum(),
        }
    }

    // ---- roll-up -----------------------------------------------------------

    /// Attach a per-node measure and build range structures for `ops`.
    ///
    /// `measure` is indexed by dense node index; `None` means the node carries no value
    /// and contributes the monoid identity.
    pub fn set_measure(&mut self, measure: Vec<Option<RollupValue>>, ops: &[RollupOp]) {
        assert_eq!(
            measure.len(),
            self.poset.n(),
            "measure must cover the poset"
        );
        self.rollups.clear();
        for &op in ops {
            if op == RollupOp::Count {
                continue; // answered structurally
            }
            let data = match &self.data {
                EncodingData::NestedSet { tin, .. } => {
                    // reorder the measure into rank order so ranges are contiguous
                    let mut by_rank = vec![RollupValue::Null; self.poset.n()];
                    for (i, m) in measure.iter().enumerate() {
                        let r = tin[i] as usize;
                        by_rank[r] = m.unwrap_or(match op {
                            RollupOp::Sum => RollupValue::Int(0),
                            _ => RollupValue::Null,
                        });
                    }
                    if op.is_invertible() {
                        RollupData::Fenwick(Fenwick::build(&by_rank))
                    } else {
                        RollupData::Sparse(SparseTable::build(&by_rank, op))
                    }
                }
                EncodingData::Chain { chains, .. } => {
                    // Per-chain suffix folds are correct for any commutative monoid; no
                    // inverse is needed because we never subtract a prefix.
                    let mut suffixes: Vec<Vec<RollupValue>> = Vec::with_capacity(chains.len());
                    for chain in chains {
                        let mut suf = vec![op.identity(); chain.len() + 1];
                        for i in (0..chain.len()).rev() {
                            let v = measure[chain[i] as usize].unwrap_or(op.identity());
                            suf[i] = op.combine(v, suf[i + 1]);
                        }
                        suffixes.push(suf);
                    }
                    RollupData::ChainSuffix(suffixes)
                }
            };
            self.rollups.insert(op, data);
        }
        self.measure = Some(measure);
    }

    /// Whether a roll-up for `op` can be answered from the index right now.
    pub fn has_rollup(&self, op: RollupOp) -> bool {
        op == RollupOp::Count || self.rollups.contains_key(&op)
    }

    /// Aggregate the declared measure over `{y} ∪ descendants(y)`, from the index.
    ///
    /// Returns `None` when no structure for `op` was built — the caller then falls back to
    /// the engine aggregation rather than guessing.
    pub fn rollup(&self, y: u32, op: RollupOp) -> Option<RollupValue> {
        if op == RollupOp::Count {
            return Some(RollupValue::Int(self.descendant_count(y) as i128));
        }
        match (self.rollups.get(&op)?, &self.data) {
            (RollupData::Fenwick(f), EncodingData::NestedSet { tin, tout, .. }) => {
                let (lo, hi) = (tin[y as usize] as usize, tout[y as usize] as usize);
                Some(f.range(lo, hi))
            }
            (RollupData::Sparse(st), EncodingData::NestedSet { tin, tout, .. }) => {
                let (lo, hi) = (tin[y as usize] as usize, tout[y as usize] as usize);
                Some(st.range(lo, hi))
            }
            (RollupData::ChainSuffix(suffixes), EncodingData::Chain { reach, .. }) => {
                let mut acc = op.identity();
                for &(cid, mp) in &reach[y as usize] {
                    acc = op.combine(acc, suffixes[cid as usize][mp as usize]);
                }
                Some(acc)
            }
            _ => None,
        }
    }

    /// Roll-up by graph node id.
    pub fn rollup_id(&self, y: crate::graph::types::NodeId, op: RollupOp) -> Option<RollupValue> {
        let yi = self.poset.idx(y)?;
        self.rollup(yi, op)
    }

    // ---- lowest common ancestors -------------------------------------------

    /// Minimal elements of `ancestors(x) ∩ ancestors(y)` — the LCA set.
    ///
    /// On a tree this is the single classical LCA and the interval encoding answers it
    /// directly: walk up from `x` to the first ancestor whose interval also contains `y`.
    /// On a DAG there can be several incomparable lowest common ancestors, so the answer is
    /// a set; chain mode computes it by intersecting reachability and dropping any element
    /// that subsumes another. Both paths use the index — neither walks the graph.
    pub fn lowest_common_ancestors(&self, x: u32, y: u32) -> Vec<u32> {
        match &self.data {
            EncodingData::NestedSet { .. } => {
                let mut cur = x;
                loop {
                    if self.subsumes(y, cur) {
                        return vec![cur];
                    }
                    match self.poset.parents(cur).first() {
                        Some(&p) => cur = p,
                        // Disjoint trees in a forest have no common ancestor.
                        None => return Vec::new(),
                    }
                }
            }
            EncodingData::Chain { .. } => {
                let common: Vec<u32> = (0..self.poset.n() as u32)
                    .filter(|&c| self.subsumes(x, c) && self.subsumes(y, c))
                    .collect();
                let mut minimal: Vec<u32> = common
                    .iter()
                    .copied()
                    .filter(|&c| !common.iter().any(|&d| d != c && self.subsumes(d, c)))
                    .collect();
                minimal.sort_unstable();
                minimal
            }
        }
    }

    /// LCA set by graph node id.
    pub fn lowest_common_ancestors_ids(
        &self,
        x: crate::graph::types::NodeId,
        y: crate::graph::types::NodeId,
    ) -> Option<Vec<crate::graph::types::NodeId>> {
        let (xi, yi) = (self.poset.idx(x)?, self.poset.idx(y)?);
        Some(
            self.lowest_common_ancestors(xi, yi)
                .into_iter()
                .map(|i| self.poset.node_at(i))
                .collect(),
        )
    }

    // ---- reporting ---------------------------------------------------------

    /// Bytes held by the **order-embedding itself** — intervals or chains, no measures.
    ///
    /// This is the number that compares like-for-like against a 2-hop index: PLL answers
    /// subsumption and nothing else, so putting its space next to a structure that also
    /// carries four monoids' worth of range tables would flatter the wrong side. The
    /// roll-up structures are reported separately by [`Self::rollup_bytes`].
    pub fn structural_bytes(&self) -> usize {
        match &self.data {
            EncodingData::NestedSet { tin, tout, inv } => {
                (tin.len() + tout.len() + inv.len()) * std::mem::size_of::<u32>()
            }
            EncodingData::Chain {
                chain_of,
                chains,
                reach,
            } => {
                chain_of.len() * std::mem::size_of::<(u32, u32)>()
                    + chains.iter().map(|c| c.len()).sum::<usize>() * std::mem::size_of::<u32>()
                    + reach.iter().map(|r| r.len()).sum::<usize>()
                        * std::mem::size_of::<(u32, u32)>()
            }
        }
    }

    /// Bytes held by the roll-up range structures.
    ///
    /// Dominated by MIN/MAX: a sparse table is O(n log n) where a Fenwick tree is O(n).
    /// That is the space-for-time trade ADR-035 §5 takes on purpose, and it should be
    /// visible in the numbers rather than folded into a single total.
    pub fn rollup_bytes(&self) -> usize {
        self.rollups
            .values()
            .map(|r| match r {
                RollupData::Fenwick(f) => f.size_bytes(),
                RollupData::Sparse(s) => s.size_bytes(),
                RollupData::ChainSuffix(s) => {
                    s.iter().map(|c| c.len()).sum::<usize>() * std::mem::size_of::<RollupValue>()
                }
            })
            .sum()
    }

    /// Approximate resident size in bytes, for `SHOW HIERARCHY INDEXES` and the benchmark
    /// space column.
    pub fn size_bytes(&self) -> usize {
        let structural = match &self.data {
            EncodingData::NestedSet { tin, tout, inv } => {
                (tin.len() + tout.len() + inv.len()) * std::mem::size_of::<u32>()
            }
            EncodingData::Chain {
                chain_of,
                chains,
                reach,
            } => {
                chain_of.len() * std::mem::size_of::<(u32, u32)>()
                    + chains.iter().map(|c| c.len()).sum::<usize>() * std::mem::size_of::<u32>()
                    + reach.iter().map(|r| r.len()).sum::<usize>()
                        * std::mem::size_of::<(u32, u32)>()
            }
        };
        let rollups: usize = self
            .rollups
            .values()
            .map(|r| match r {
                RollupData::Fenwick(f) => f.size_bytes(),
                RollupData::Sparse(s) => s.size_bytes(),
                RollupData::ChainSuffix(s) => {
                    s.iter().map(|c| c.len()).sum::<usize>() * std::mem::size_of::<RollupValue>()
                }
            })
            .sum();
        structural + rollups
    }

    /// Bytes per node — the space column the paper compares against 2-hop labeling.
    pub fn bytes_per_node(&self) -> f64 {
        if self.poset.n() == 0 {
            0.0
        } else {
            self.size_bytes() as f64 / self.poset.n() as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::NodeId;
    use crate::index::hierarchy::oracle;

    fn nid(i: u64) -> NodeId {
        NodeId(i)
    }

    /// Balanced tree of `depth` levels with `fanout` children per node.
    fn balanced_tree(depth: usize, fanout: usize) -> Poset {
        let mut edges = Vec::new();
        let mut frontier = vec![0u64];
        let mut next = 1u64;
        for _ in 0..depth {
            let mut new_frontier = Vec::new();
            for &p in &frontier {
                for _ in 0..fanout {
                    edges.push((nid(next), nid(p)));
                    new_frontier.push(next);
                    next += 1;
                }
            }
            frontier = new_frontier;
        }
        Poset::from_edges(edges, std::iter::empty()).unwrap()
    }

    /// Deterministic low-width DAG: `layers` layers of `width` nodes, each node linked to
    /// two parents in the layer above. Every node above the bottom is reachable along many
    /// paths — the shape that punishes a naive path-sum roll-up.
    fn layered_dag(layers: usize, width: usize) -> Poset {
        let mut edges = Vec::new();
        let id = |layer: usize, i: usize| nid((layer * width + i) as u64);
        for layer in 1..layers {
            for i in 0..width {
                edges.push((id(layer, i), id(layer - 1, i)));
                edges.push((id(layer, i), id(layer - 1, (i + 1) % width)));
            }
        }
        Poset::from_edges(edges, std::iter::empty()).unwrap()
    }

    fn unit_measure(n: usize) -> Vec<Option<RollupValue>> {
        vec![Some(RollupValue::Int(1)); n]
    }

    fn ramp_measure(n: usize) -> Vec<Option<RollupValue>> {
        (0..n)
            .map(|i| Some(RollupValue::Int(i as i128 + 1)))
            .collect()
    }

    // ---- probe -------------------------------------------------------------

    #[test]
    fn probe_selects_nested_set_for_a_tree() {
        let p = balanced_tree(3, 3);
        assert_eq!(OehIndex::probe(&p), Probe::Tree);
        let idx = OehIndex::build(p).unwrap();
        assert_eq!(idx.encoding(), Encoding::NestedSet);
    }

    #[test]
    fn probe_selects_chain_for_a_low_width_dag() {
        let p = layered_dag(6, 4);
        assert!(matches!(OehIndex::probe(&p), Probe::LowWidthDag { .. }));
        let idx = OehIndex::build(p).unwrap();
        assert_eq!(idx.encoding(), Encoding::Chain);
    }

    #[test]
    fn probe_declines_a_high_width_dag() {
        // A wide bipartite DAG: 400 leaves each under 2 of 3 roots. Width ≈ #leaves,
        // which is the Gene Ontology regime — the index must decline, not build.
        let mut edges = Vec::new();
        for i in 0..400u64 {
            edges.push((nid(1000 + i), nid(i % 3)));
            edges.push((nid(1000 + i), nid((i + 1) % 3)));
        }
        let p = Poset::from_edges(edges, std::iter::empty()).unwrap();
        match OehIndex::probe(&p) {
            Probe::HighWidthDag { width, cap } => assert!(width > cap),
            other => panic!("expected a decline, got {other:?}"),
        }
        let err = OehIndex::build(p).unwrap_err();
        assert!(matches!(err, HierarchyError::WidthTooHigh { .. }));
    }

    #[test]
    fn width_cap_is_eight_root_n_with_a_floor() {
        assert_eq!(width_cap_for(1), 64);
        assert_eq!(width_cap_for(10_000), 800);
        assert_eq!(width_cap_for(1_000_000), 8_000);
    }

    #[test]
    fn forced_nested_set_on_a_dag_is_rejected() {
        let p = layered_dag(4, 3);
        assert!(matches!(
            OehIndex::build_forced(p, Encoding::NestedSet),
            Err(HierarchyError::NotATree)
        ));
    }

    // ---- subsumption: all pairs vs oracle ----------------------------------

    fn assert_subsumption_matches_oracle(p: Poset, forced: Option<Encoding>) {
        let n = p.n();
        let oracle_poset = p.clone();
        let idx = match forced {
            Some(e) => OehIndex::build_forced(p, e).unwrap(),
            None => OehIndex::build(p).unwrap(),
        };
        for x in 0..n as u32 {
            for y in 0..n as u32 {
                assert_eq!(
                    idx.subsumes(x, y),
                    oracle::subsumes(&oracle_poset, x, y),
                    "subsumes({x}, {y}) in {:?} mode",
                    idx.encoding()
                );
            }
        }
    }

    #[test]
    fn nested_set_subsumption_equals_oracle_on_all_pairs() {
        assert_subsumption_matches_oracle(balanced_tree(4, 3), None);
    }

    #[test]
    fn nested_set_subsumption_equals_oracle_on_a_chain() {
        let edges: Vec<_> = (1..60u64).map(|i| (nid(i), nid(i - 1))).collect();
        assert_subsumption_matches_oracle(
            Poset::from_edges(edges, std::iter::empty()).unwrap(),
            None,
        );
    }

    #[test]
    fn nested_set_subsumption_equals_oracle_on_a_forest() {
        let mut edges = Vec::new();
        for root in 0..3u64 {
            for c in 1..5u64 {
                edges.push((nid(root * 100 + c), nid(root * 100)));
            }
        }
        assert_subsumption_matches_oracle(
            Poset::from_edges(edges, std::iter::empty()).unwrap(),
            None,
        );
    }

    #[test]
    fn chain_subsumption_equals_oracle_on_all_pairs() {
        assert_subsumption_matches_oracle(layered_dag(6, 4), None);
    }

    #[test]
    fn chain_subsumption_equals_oracle_on_a_diamond() {
        let p = Poset::from_edges(
            vec![
                (nid(3), nid(1)),
                (nid(3), nid(2)),
                (nid(1), nid(0)),
                (nid(2), nid(0)),
            ],
            std::iter::empty(),
        )
        .unwrap();
        assert_subsumption_matches_oracle(p, None);
    }

    #[test]
    fn chain_mode_on_a_tree_agrees_with_nested_set() {
        // Forcing chain mode on a tree exercises both encodings over identical data —
        // any disagreement is an encoding bug rather than a data artifact.
        let p = balanced_tree(3, 3);
        assert_subsumption_matches_oracle(p, Some(Encoding::Chain));
    }

    // ---- descendants -------------------------------------------------------

    #[test]
    fn descendants_match_the_oracle_set() {
        for (p, label) in [(balanced_tree(3, 3), "tree"), (layered_dag(5, 4), "dag")] {
            let oracle_poset = p.clone();
            let idx = OehIndex::build(p).unwrap();
            for y in 0..oracle_poset.n() as u32 {
                let mut got = idx.descendants(y);
                got.sort_unstable();
                let mut want: Vec<u32> = oracle::descendants(&oracle_poset, y, true)
                    .into_iter()
                    .collect();
                want.sort_unstable();
                assert_eq!(got, want, "descendants({y}) on {label}");
            }
        }
    }

    #[test]
    fn descendants_are_duplicate_free_on_a_dag() {
        // The double-count trap in set form: on a DAG a node is reachable along many
        // paths, and the enumeration must still list it once.
        let p = layered_dag(6, 4);
        let idx = OehIndex::build(p).unwrap();
        for y in 0..idx.poset().n() as u32 {
            let d = idx.descendants(y);
            let uniq: std::collections::HashSet<u32> = d.iter().copied().collect();
            assert_eq!(d.len(), uniq.len(), "descendants({y}) contained duplicates");
        }
    }

    #[test]
    fn descendant_count_is_structural_and_needs_no_measure() {
        for p in [balanced_tree(3, 3), layered_dag(5, 4)] {
            let oracle_poset = p.clone();
            let idx = OehIndex::build(p).unwrap();
            for y in 0..oracle_poset.n() as u32 {
                assert_eq!(
                    idx.descendant_count(y),
                    oracle::descendants(&oracle_poset, y, true).len()
                );
                assert_eq!(
                    idx.rollup(y, RollupOp::Count),
                    Some(RollupValue::Int(
                        oracle::descendants(&oracle_poset, y, true).len() as i128
                    ))
                );
            }
        }
    }

    // ---- roll-up: every node, every monoid, vs oracle -----------------------

    fn assert_rollup_matches_oracle(p: Poset, measure: Vec<Option<RollupValue>>) {
        let oracle_poset = p.clone();
        let mut idx = OehIndex::build(p).unwrap();
        let ops = [RollupOp::Sum, RollupOp::Min, RollupOp::Max, RollupOp::Count];
        idx.set_measure(measure.clone(), &ops);
        for op in ops {
            for y in 0..oracle_poset.n() as u32 {
                let want = if op == RollupOp::Count {
                    RollupValue::Int(oracle::descendants(&oracle_poset, y, true).len() as i128)
                } else {
                    oracle::rollup(&oracle_poset, y, &measure, op)
                };
                assert_eq!(
                    idx.rollup(y, op),
                    Some(want),
                    "{op:?} roll-up at {y} in {:?} mode",
                    idx.encoding()
                );
            }
        }
    }

    #[test]
    fn nested_set_rollup_equals_oracle() {
        let p = balanced_tree(4, 3);
        let n = p.n();
        assert_rollup_matches_oracle(p, ramp_measure(n));
    }

    #[test]
    fn chain_rollup_equals_oracle() {
        let p = layered_dag(6, 4);
        let n = p.n();
        assert_rollup_matches_oracle(p, ramp_measure(n));
    }

    #[test]
    fn rollup_with_sparse_measures_treats_missing_as_identity() {
        let p = balanced_tree(3, 3);
        let n = p.n();
        let measure: Vec<Option<RollupValue>> = (0..n)
            .map(|i| {
                if i % 3 == 0 {
                    Some(RollupValue::Int(5))
                } else {
                    None
                }
            })
            .collect();
        assert_rollup_matches_oracle(p, measure);
    }

    // ---- the double-count class --------------------------------------------

    #[test]
    fn dag_rollup_counts_each_descendant_once_where_naive_path_sum_overcounts() {
        let p = layered_dag(6, 4);
        let n = p.n();
        let oracle_poset = p.clone();
        let measure = unit_measure(n);
        let mut idx = OehIndex::build(p).unwrap();
        idx.set_measure(measure.clone(), &[RollupOp::Sum]);

        // A naive sum that follows every path instead of the descendant set. On this DAG
        // it is strictly larger for at least one node — which is exactly the bug the chain
        // encoding exists to avoid, so the test asserts the trap is live before asserting
        // the index steps around it.
        fn naive_path_sum(p: &Poset, y: u32, measure: &[Option<RollupValue>]) -> i128 {
            let own = match measure[y as usize] {
                Some(RollupValue::Int(v)) => v,
                _ => 0,
            };
            own + p
                .children(y)
                .iter()
                .map(|&c| naive_path_sum(p, c, measure))
                .sum::<i128>()
        }

        let mut trap_fired = false;
        for y in 0..n as u32 {
            let exact = oracle::rollup(&oracle_poset, y, &measure, RollupOp::Sum);
            let naive = RollupValue::Int(naive_path_sum(&oracle_poset, y, &measure));
            if naive != exact {
                trap_fired = true;
            }
            assert_eq!(idx.rollup(y, RollupOp::Sum), Some(exact), "exact at {y}");
        }
        assert!(
            trap_fired,
            "fixture must actually over-count under a path sum, else the test proves nothing"
        );
    }

    #[test]
    fn diamond_rollup_does_not_double_count() {
        let p = Poset::from_edges(
            vec![
                (nid(3), nid(1)),
                (nid(3), nid(2)),
                (nid(1), nid(0)),
                (nid(2), nid(0)),
            ],
            std::iter::empty(),
        )
        .unwrap();
        let root = p.idx(nid(0)).unwrap();
        let n = p.n();
        let mut idx = OehIndex::build(p).unwrap();
        idx.set_measure(unit_measure(n), &[RollupOp::Sum]);
        // 4 nodes, 1 each — a path sum would report 5 because node 3 is reached twice.
        assert_eq!(idx.rollup(root, RollupOp::Sum), Some(RollupValue::Int(4)));
    }

    // ---- misc --------------------------------------------------------------

    #[test]
    fn lca_equals_the_oracle_in_both_encodings() {
        for p in [balanced_tree(3, 3), layered_dag(5, 4)] {
            let oracle_poset = p.clone();
            let idx = OehIndex::build(p).unwrap();
            for x in 0..oracle_poset.n() as u32 {
                for y in 0..oracle_poset.n() as u32 {
                    let mut got = idx.lowest_common_ancestors(x, y);
                    got.sort_unstable();
                    let want = oracle::lowest_common_ancestors(&oracle_poset, x, y);
                    assert_eq!(got, want, "lca({x}, {y}) in {:?} mode", idx.encoding());
                }
            }
        }
    }

    #[test]
    fn lca_across_disjoint_trees_is_empty() {
        let p = Poset::from_edges(vec![(nid(1), nid(0)), (nid(3), nid(2))], std::iter::empty())
            .unwrap();
        let (a, b) = (p.idx(nid(1)).unwrap(), p.idx(nid(3)).unwrap());
        let idx = OehIndex::build(p).unwrap();
        assert!(idx.lowest_common_ancestors(a, b).is_empty());
    }

    #[test]
    fn rollup_without_a_built_structure_returns_none() {
        let p = balanced_tree(2, 2);
        let n = p.n();
        let mut idx = OehIndex::build(p).unwrap();
        idx.set_measure(unit_measure(n), &[RollupOp::Sum]);
        assert!(idx.rollup(0, RollupOp::Max).is_none());
        assert!(!idx.has_rollup(RollupOp::Max));
        assert!(idx.has_rollup(RollupOp::Sum));
        assert!(idx.has_rollup(RollupOp::Count), "count is always available");
    }

    #[test]
    fn subsumes_by_node_id_reports_unknown_nodes_as_none() {
        let p = balanced_tree(2, 2);
        let idx = OehIndex::build(p).unwrap();
        assert_eq!(idx.subsumes_ids(nid(1), nid(0)), Some(true));
        assert_eq!(idx.subsumes_ids(nid(0), nid(1)), Some(false));
        assert_eq!(idx.subsumes_ids(nid(9999), nid(0)), None);
    }

    #[test]
    fn nested_set_costs_about_twelve_bytes_per_node_before_measures() {
        // 2 interval arrays + the rank inverse, all u32.
        let idx = OehIndex::build(balanced_tree(5, 3)).unwrap();
        assert_eq!(idx.bytes_per_node(), 12.0);
        assert_eq!(
            idx.rollup_bytes(),
            0,
            "no measure declared, no roll-up structures"
        );
    }

    #[test]
    fn min_max_cost_far_more_space_than_sum_and_the_split_shows_it() {
        // The space-for-time trade made explicit: a Fenwick tree is O(n), a sparse table
        // is O(n log n). Reporting one total would hide which monoid you are paying for.
        let p = balanced_tree(5, 3);
        let n = p.n();
        let mut sum_only = OehIndex::build(p).unwrap();
        sum_only.set_measure(unit_measure(n), &[RollupOp::Sum]);

        let p2 = balanced_tree(5, 3);
        let mut with_max = OehIndex::build(p2).unwrap();
        with_max.set_measure(unit_measure(n), &[RollupOp::Sum, RollupOp::Max]);

        assert_eq!(sum_only.structural_bytes(), with_max.structural_bytes());
        assert!(
            with_max.rollup_bytes() > 4 * sum_only.rollup_bytes(),
            "sparse table {} should dwarf the Fenwick tree {}",
            with_max.rollup_bytes(),
            sum_only.rollup_bytes()
        );
    }
}
