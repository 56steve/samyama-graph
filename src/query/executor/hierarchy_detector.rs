//! Detector for the hierarchy patterns the OEH index can answer (ADR-035 §8).
//!
//! Recognizes queries of the form
//!
//! ```text
//! MATCH (d)-[:IS_A*0..]->(r:Class {code: "C0"}) RETURN sum(d.units)   -- roll-up
//! MATCH (d)-[:IS_A*0..]->(r:Class {code: "C0"}) RETURN d              -- descendant scan
//! ```
//!
//! and their reversed spelling `MATCH (r:Class {code: "C0"})<-[:IS_A*0..]-(d)`.
//!
//! Like [`super::adjacency_agg_detector`], this is deliberately conservative: any
//! constraint that fails returns `None`, which means "use the standard plan" and is never
//! an error. The rewrite must be invisible except in cost.
//!
//! ## Why `*0..` specifically
//!
//! A roll-up folds the measure over `{y} ∪ descendants(y)` — the *reflexive* descendant
//! set. `*0..` is the reflexive spelling; the default `*` (`*1..`) excludes the root, and
//! `*1..3` excludes both the root and everything deeper than three hops. The index answers
//! the reflexive unbounded question and nothing else, so those other spellings are left to
//! the standard planner rather than answered approximately. For SUM we could subtract the
//! root's own measure to recover the strict form, but MIN and MAX have no inverse — a
//! rewrite that worked for two monoids and silently misanswered the other two would be
//! worse than no rewrite at all.

use crate::graph::{GraphStore, Label, NodeId, PropertyValue};
use crate::index::hierarchy::RollupOp;
use crate::query::ast::{Direction, Expression, NodePattern, Query};

/// What the detector found.
#[derive(Debug, Clone, PartialEq)]
pub enum HierarchyRewrite {
    /// Answer a subtree aggregate from the index in one step.
    Rollup {
        /// Index that answers it.
        index_name: String,
        /// Pinned subtree root.
        root: NodeId,
        /// Monoid.
        op: RollupOp,
        /// Output column name.
        alias: String,
    },
    /// Enumerate the reflexive descendant set from the index.
    DescendantScan {
        /// Index that answers it.
        index_name: String,
        /// Pinned subtree root.
        root: NodeId,
        /// Variable each descendant binds to.
        var: String,
    },
}

/// The shape common to both rewrites, extracted once.
struct HierarchyPattern {
    index_name: String,
    root: NodeId,
    /// Variable bound to the descendant side.
    descendant_var: String,
    /// Measure declared on the index, if any.
    measure: Option<String>,
}

/// Try to rewrite `query` into a hierarchy-index plan.
pub fn detect(query: &Query, store: &GraphStore) -> Option<HierarchyRewrite> {
    if store.hierarchy_index.is_empty() {
        return None;
    }
    let pattern = match_pattern(query, store)?;
    let ret = query.return_clause.as_ref()?;

    // --- roll-up: exactly one aggregate over the descendant variable -------------
    if ret.items.len() == 1 && !ret.distinct {
        if let Expression::Function {
            name,
            args,
            distinct,
        } = &ret.items[0].expression
        {
            if *distinct {
                return None;
            }
            let op = RollupOp::parse(name)?;
            let measured_property = match args.as_slice() {
                // sum(d.units) / min(d.units) / max(d.units)
                [Expression::Property { variable, property }]
                    if *variable == pattern.descendant_var =>
                {
                    Some(property.clone())
                }
                // count(d) — needs no measure at all, the index answers it structurally
                [Expression::Variable(v)] if *v == pattern.descendant_var => None,
                _ => return None,
            };

            match (op, &measured_property) {
                // COUNT is answered from the interval arithmetic; a counted property would
                // mean "count non-null", which is a different question.
                (RollupOp::Count, None) => {}
                (RollupOp::Count, Some(_)) => return None,
                // Every other monoid needs the aggregated property to be *the* declared
                // measure. Aggregating some other property would need a different index.
                (_, Some(p)) => {
                    if pattern.measure.as_deref() != Some(p.as_str()) {
                        return None;
                    }
                }
                (_, None) => return None,
            }

            // The index must actually carry a structure for this monoid.
            let entry = store.hierarchy_index.usable_named(&pattern.index_name)?;
            let has = {
                let g = entry.read().unwrap();
                g.index.as_ref().is_some_and(|i| i.has_rollup(op))
            };
            if !has {
                return None;
            }

            let alias = ret.items[0]
                .alias
                .clone()
                .unwrap_or_else(|| match &measured_property {
                    Some(p) => format!("{}({}.{})", op.name(), pattern.descendant_var, p),
                    None => format!("{}({})", op.name(), pattern.descendant_var),
                });
            return Some(HierarchyRewrite::Rollup {
                index_name: pattern.index_name,
                root: pattern.root,
                op,
                alias,
            });
        }
    }

    // --- descendant scan: RETURN the descendant variable itself ------------------
    if ret.items.len() == 1 && !ret.distinct && query.order_by.is_none() {
        if let Expression::Variable(v) = &ret.items[0].expression {
            if *v == pattern.descendant_var && ret.items[0].alias.is_none() {
                return Some(HierarchyRewrite::DescendantScan {
                    index_name: pattern.index_name,
                    root: pattern.root,
                    var: pattern.descendant_var,
                });
            }
        }
    }

    None
}

/// Match `MATCH (d)-[:T*0..]->(root {pinned})` (or its reversed spelling) against a usable
/// hierarchy index, and resolve the pinned root to exactly one node.
fn match_pattern(query: &Query, store: &GraphStore) -> Option<HierarchyPattern> {
    // The rewrite replaces the whole plan, so anything else in the query disqualifies it.
    if query.match_clauses.len() != 1
        || query.where_clause.is_some()
        || query.with_clause.is_some()
        || query.create_clause.is_some()
        || query.delete_clause.is_some()
        || query.call_clause.is_some()
        || query.call_subquery.is_some()
        || query.unwind_clause.is_some()
        || query.merge_clause.is_some()
        || query.foreach_clause.is_some()
        || !query.set_clauses.is_empty()
        || !query.remove_clauses.is_empty()
        || !query.union_queries.is_empty()
        || query.limit.is_some()
        || query.skip.is_some()
    {
        return None;
    }

    let clause = &query.match_clauses[0];
    if clause.optional || clause.pattern.paths.len() != 1 {
        return None;
    }
    let path = &clause.pattern.paths[0];
    if path.segments.len() != 1 || path.path_variable.is_some() {
        return None;
    }
    let segment = &path.segments[0];
    let edge = &segment.edge;

    // Exactly one covering edge type, reflexive and unbounded.
    if edge.types.len() != 1 || edge.variable.is_some() || edge.properties.is_some() {
        return None;
    }
    let length = edge.length.as_ref()?;
    if length.min != Some(0) || length.max.is_some() {
        return None;
    }

    // Orientation decides which endpoint is the root.
    let (descendant, root_pattern) = match edge.direction {
        Direction::Outgoing => (&path.start, &segment.node), // (d)-[:T*0..]->(root)
        Direction::Incoming => (&segment.node, &path.start), // (root)<-[:T*0..]-(d)
        Direction::Both => return None, // an undirected hierarchy walk is not subsumption
    };

    let descendant_var = descendant.variable.clone()?;
    // A label or property constraint on the descendant side would filter the subtree, and
    // the index enumerates it unfiltered — leave those to the standard planner.
    if !descendant.labels.is_empty() || descendant.properties.is_some() {
        return None;
    }

    let entry = store.hierarchy_index.usable_for_edge_type(&edge.types[0])?;
    let (index_name, measure, poset_has_root) = {
        let g = entry.read().unwrap();
        let root = resolve_pinned_node(store, root_pattern)?;
        let has = g
            .index
            .as_ref()
            .is_some_and(|i| i.poset().idx(root).is_some());
        (
            g.spec.name.clone(),
            g.spec.measure.as_ref().map(|m| m.property.clone()),
            (root, has),
        )
    };
    let (root, in_poset) = poset_has_root;
    if !in_poset {
        // The pinned node is not part of this hierarchy — the index cannot answer, and
        // pretending its subtree is empty would be wrong.
        return None;
    }

    Some(HierarchyPattern {
        index_name,
        root,
        descendant_var,
        measure,
    })
}

/// Resolve a node pattern pinned by label + inline properties to exactly one node.
///
/// Requires a label (so the search is a label scan, not a full scan) and at least one
/// property. Ambiguity is a rejection, not a coin flip: if two nodes match the pin, the
/// query means something the index cannot express in one roll-up.
fn resolve_pinned_node(store: &GraphStore, pattern: &NodePattern) -> Option<NodeId> {
    let props = pattern.properties.as_ref()?;
    if props.is_empty() || pattern.labels.is_empty() {
        return None;
    }
    let label: &Label = &pattern.labels[0];

    // Prefer a property index when one covers the pin. Without this the rewrite pays an
    // O(label size) scan *per execution* to find the subtree root — which on a 9k-node
    // ontology costs more than the O(log n) roll-up it is setting up, and turns a
    // thousand-fold win into a fifty-fold one. The index lookup is the same one an
    // ordinary `MATCH (r:Term {code: ...})` would use.
    let candidates: Vec<NodeId> = props
        .iter()
        .find(|(k, _)| store.property_index.has_index(label, k))
        .and_then(|(k, v)| {
            store
                .property_index
                .get_index(label, k)
                .map(|idx| idx.read().unwrap().get(v))
        })
        .unwrap_or_else(|| store.node_ids_by_label(label, None));

    let mut found: Option<NodeId> = None;
    for id in candidates {
        if props
            .iter()
            .all(|(k, v)| node_property_equals(store, id, k, v))
        {
            if found.is_some() {
                return None; // ambiguous pin
            }
            found = Some(id);
        }
    }
    found
}

/// Compare a node property, reading the columnar store first (ADR-021): an imported graph
/// has nothing in the sparse map, and a pin that silently never matched would turn the
/// rewrite off exactly where it matters most.
fn node_property_equals(store: &GraphStore, id: NodeId, key: &str, want: &PropertyValue) -> bool {
    let columnar = store.node_columns.get_property(id.as_u64() as usize, key);
    let actual = match columnar {
        PropertyValue::Null => match store.get_node(id).and_then(|n| n.get_property(key)) {
            Some(v) => v.clone(),
            None => return false,
        },
        v => v,
    };
    &actual == want
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeType;
    use crate::index::hierarchy::{HierarchySpec, RollupOp};
    use crate::query::parse_query;

    fn store_with_index() -> GraphStore {
        let mut store = GraphStore::new();
        let root = store.create_node("Class");
        store.set_column_property(root, "code", PropertyValue::String("ROOT".into()));
        let mut n = 1i64;
        for c in 0..3 {
            let mid = store.create_node("Class");
            store.set_column_property(mid, "code", PropertyValue::String(format!("C{c}")));
            store.create_edge(mid, root, "IS_A").unwrap();
            for _ in 0..3 {
                let leaf = store.create_node("Drug");
                store.create_edge(leaf, mid, "IS_A").unwrap();
                store.set_column_property(leaf, "units", PropertyValue::Integer(n));
                n += 1;
            }
        }
        let mgr = std::sync::Arc::clone(&store.hierarchy_index);
        mgr.create(
            &store,
            HierarchySpec::new("atc", vec![EdgeType::new("IS_A")]).with_measure(
                None,
                "units",
                vec![RollupOp::Sum, RollupOp::Max, RollupOp::Count],
            ),
        )
        .unwrap();
        store
    }

    fn detect_str(q: &str, store: &GraphStore) -> Option<HierarchyRewrite> {
        let query = parse_query(q).unwrap();
        detect(&query, store)
    }

    #[test]
    fn detects_a_reflexive_rollup() {
        let store = store_with_index();
        let r = detect_str(
            "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units) AS total",
            &store,
        );
        match r {
            Some(HierarchyRewrite::Rollup { op, alias, .. }) => {
                assert_eq!(op, RollupOp::Sum);
                assert_eq!(alias, "total");
            }
            other => panic!("expected a roll-up rewrite, got {other:?}"),
        }
    }

    #[test]
    fn detects_the_reversed_spelling() {
        let store = store_with_index();
        assert!(matches!(
            detect_str(
                "MATCH (r:Class {code: \"C0\"})<-[:IS_A*0..]-(d) RETURN sum(d.units)",
                &store
            ),
            Some(HierarchyRewrite::Rollup { .. })
        ));
    }

    #[test]
    fn detects_count_without_a_measure_argument() {
        let store = store_with_index();
        assert!(matches!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN count(d)",
                &store
            ),
            Some(HierarchyRewrite::Rollup {
                op: RollupOp::Count,
                ..
            })
        ));
    }

    #[test]
    fn detects_a_descendant_scan() {
        let store = store_with_index();
        assert!(matches!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"ROOT\"}) RETURN d",
                &store
            ),
            Some(HierarchyRewrite::DescendantScan { .. })
        ));
    }

    // --- the conservative rejections ---------------------------------------

    #[test]
    fn rejects_the_strict_default_star() {
        // `*` is `*1..` — it excludes the root, which is a different set from the one the
        // index folds. Answering it with a reflexive roll-up would be wrong.
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_a_bounded_depth() {
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..2]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_an_undeclared_edge_type() {
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:TREATS*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_aggregating_a_property_that_is_not_the_measure() {
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.price)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_a_monoid_with_no_built_structure() {
        // The index declared sum/max/count; min has no range structure, so the rewrite
        // must decline rather than return the identity.
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN min(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn a_property_index_on_the_pin_finds_the_same_root_as_a_scan() {
        // Same rewrite either way — the property index only changes how the root is
        // located, never which node it is.
        let store = store_with_index();
        let without = detect_str(
            "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
            &store,
        );
        store
            .property_index
            .create_index(Label::new("Class"), "code".to_string());
        for id in store.node_ids_by_label(&Label::new("Class"), None) {
            let v = store
                .node_columns
                .get_property(id.as_u64() as usize, "code");
            store
                .property_index
                .index_insert(&Label::new("Class"), "code", v, id);
        }
        let with = detect_str(
            "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
            &store,
        );
        assert!(with.is_some());
        assert_eq!(with, without);
    }

    #[test]
    fn rejects_an_ambiguous_root_pin() {
        // Two classes share a code: the query means something the index cannot answer with
        // a single roll-up.
        let mut store = store_with_index();
        let dup = store.create_node("Class");
        store.set_column_property(dup, "code", PropertyValue::String("C0".into()));
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_an_unpinned_root() {
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_a_root_outside_the_hierarchy() {
        let mut store = store_with_index();
        let orphan = store.create_node("Class");
        store.set_column_property(orphan, "code", PropertyValue::String("ORPHAN".into()));
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"ORPHAN\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_a_filtered_descendant_side() {
        // A label on the descendant filters the subtree; the index enumerates it whole.
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d:Drug)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_when_a_where_clause_is_present() {
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) WHERE d.units > 1 RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_a_stale_index() {
        let mut store = store_with_index();
        let extra = store.create_node("Drug");
        let root = store.node_ids_by_label(&Label::new("Class"), Some(1))[0];
        store.create_edge(extra, root, "IS_A").unwrap();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None,
            "a stale index must fall back to the standard plan"
        );
    }

    #[test]
    fn rejects_when_no_hierarchy_is_declared() {
        let mut store = GraphStore::new();
        let root = store.create_node("Class");
        store.set_column_property(root, "code", PropertyValue::String("C0".into()));
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"C0\"}) RETURN sum(d.units)",
                &store
            ),
            None
        );
    }

    #[test]
    fn rejects_distinct_and_ordered_forms() {
        let store = store_with_index();
        assert_eq!(
            detect_str(
                "MATCH (d)-[:IS_A*0..]->(r:Class {code: \"ROOT\"}) RETURN DISTINCT d",
                &store
            ),
            None
        );
    }
}
