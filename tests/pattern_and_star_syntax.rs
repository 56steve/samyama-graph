//! Bare relationship arrows and `RETURN *` / `WITH *` (openCypher TCK).
//!
//! Both were found by running the TCK and grouping its failures by cause:
//! 228 of 560 failures were the parser rejecting the query outright, and these
//! two constructs accounted for the largest clusters.
//!
//! `MATCH (a)-->(b)` — about the simplest pattern in Cypher — was a **syntax
//! error**, because the grammar required the bracket in `-[...]->`. That is
//! invisible in a codebase whose own queries always name a relationship type,
//! which is why every query in `benches/`, `examples/` and the existing tests
//! passed while the engine could not parse the form every tutorial opens with.
//!
//! The tests below therefore assert on *results*, not on parsing: a pattern
//! that parses and then matches the wrong thing is no better than one that
//! does not parse.

use samyama::graph::{GraphStore, PropertyValue};
use samyama::query::executor::{MutQueryExecutor, QueryExecutor, Value};
use samyama::query::parser::parse_query;

fn write(store: &mut GraphStore, cypher: &str) {
    let q = parse_query(cypher).expect("setup should parse");
    MutQueryExecutor::new(store, "default".to_string())
        .execute(&q)
        .expect("setup should run");
}

fn count(store: &GraphStore, cypher: &str) -> i64 {
    let q = parse_query(cypher).expect("query should parse");
    let batch = QueryExecutor::new(store).execute(&q).expect("query should run");
    match batch.records[0].get("c") {
        Some(Value::Property(PropertyValue::Integer(n))) => *n,
        other => panic!("expected an integer count, got {other:?}"),
    }
}

fn rows(store: &GraphStore, cypher: &str) -> usize {
    let q = parse_query(cypher).expect("query should parse");
    QueryExecutor::new(store).execute(&q).expect("query should run").records.len()
}

/// Column names of the first row, sorted.
fn columns(store: &GraphStore, cypher: &str) -> Vec<String> {
    let q = parse_query(cypher).expect("query should parse");
    let batch = QueryExecutor::new(store).execute(&q).expect("query should run");
    let mut cols: Vec<String> = batch
        .records
        .first()
        .map(|r| r.bindings().iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default();
    cols.sort();
    cols
}

/// `A -R-> B -S-> C`: two edges, three nodes, no cycles.
fn chain() -> GraphStore {
    let mut store = GraphStore::new();
    write(&mut store, "CREATE (:A {n: 'a'})-[:R]->(:B {n: 'b'})-[:S]->(:C {n: 'c'})");
    store
}

// ---------------------------------------------------------------- bare arrows

#[test]
fn a_bare_outgoing_arrow_matches_each_edge_once() {
    assert_eq!(count(&chain(), "MATCH (a)-->(b) RETURN count(*) AS c"), 2);
}

#[test]
fn a_bare_incoming_arrow_matches_each_edge_once() {
    assert_eq!(count(&chain(), "MATCH (a)<--(b) RETURN count(*) AS c"), 2);
}

#[test]
fn a_bare_undirected_edge_matches_each_edge_from_both_ends() {
    // The rule that makes the count fast-path unsafe: an undirected pattern
    // binds (a=A,b=B) *and* (a=B,b=A).
    assert_eq!(count(&chain(), "MATCH (a)--(b) RETURN count(*) AS c"), 4);
    assert_eq!(rows(&chain(), "MATCH (a)--(b) RETURN a"), 4);
}

#[test]
fn a_count_over_an_undirected_pattern_agrees_with_the_rows_it_counts() {
    // `count(*)` had an O(1) fast path reading the edge count off the store,
    // which counts each edge once. It disagreed with the query's own row count
    // — 2 against 4 — and no test compared the two.
    let store = chain();
    for pattern in ["(a)--(b)", "()--()", "(a)-[:R]-(b)", "(a)-[r]-(b)"] {
        let counted = count(&store, &format!("MATCH {pattern} RETURN count(*) AS c"));
        let listed = rows(&store, &format!("MATCH {pattern} RETURN 1 AS x")) as i64;
        assert_eq!(counted, listed, "count(*) disagreed with the rows for {pattern}");
    }
}

#[test]
fn a_count_over_a_directed_pattern_still_uses_the_fast_path_correctly() {
    let store = chain();
    for pattern in ["(a)-->(b)", "()-->()",  "(a)-[:R]->(b)"] {
        let counted = count(&store, &format!("MATCH {pattern} RETURN count(*) AS c"));
        let listed = rows(&store, &format!("MATCH {pattern} RETURN 1 AS x")) as i64;
        assert_eq!(counted, listed, "{pattern}");
    }
}

#[test]
fn an_anonymous_undirected_chain_parses_and_traverses() {
    // `(:A)-->()--()`: forward one hop, then either direction.
    assert!(count(&chain(), "MATCH (:A)-->()--() RETURN count(*) AS c") > 0);
}

#[test]
fn a_bare_arrow_binds_the_same_endpoints_as_the_bracketed_form() {
    // The equivalence that makes the grammar change safe: `-->` is
    // `-[]->` with no type filter.
    let store = chain();
    assert_eq!(
        rows(&store, "MATCH (a)-->(b) RETURN a, b"),
        rows(&store, "MATCH (a)-[]->(b) RETURN a, b"),
    );
    assert_eq!(
        rows(&store, "MATCH (a)--(b) RETURN a, b"),
        rows(&store, "MATCH (a)-[]-(b) RETURN a, b"),
    );
}

#[test]
fn a_typed_edge_still_filters_by_type() {
    // Guard against the optional bracket making the type filter optional too.
    assert_eq!(count(&chain(), "MATCH (a)-[:R]->(b) RETURN count(*) AS c"), 1);
    assert_eq!(count(&chain(), "MATCH (a)-[:NOPE]->(b) RETURN count(*) AS c"), 0);
}

// ------------------------------------------------------------- RETURN */WITH *

#[test]
fn return_star_projects_every_bound_variable_including_edges() {
    // Forgetting the edge variable is the easy mistake, and it shows up as a
    // column silently missing rather than as an error.
    assert_eq!(
        columns(&chain(), "MATCH (a)-[r]->(b) RETURN *"),
        vec!["a".to_string(), "b".to_string(), "r".to_string()]
    );
}

#[test]
fn return_star_includes_a_named_path() {
    assert_eq!(
        columns(&chain(), "MATCH p = (a)-[r]->(b) RETURN *"),
        vec!["a".to_string(), "b".to_string(), "p".to_string(), "r".to_string()]
    );
}

#[test]
fn return_star_includes_an_unwind_variable() {
    assert_eq!(columns(&GraphStore::new(), "UNWIND [1, 2] AS u RETURN *"), vec!["u".to_string()]);
}

#[test]
fn return_star_alongside_an_explicit_item_does_not_duplicate_it() {
    let store = chain();
    assert_eq!(columns(&store, "MATCH (a:A) RETURN *, a"), vec!["a".to_string()]);
    // An aliased expression is a new column, so both survive.
    assert_eq!(
        columns(&store, "MATCH (a:A) RETURN *, a.n AS name"),
        vec!["a".to_string(), "name".to_string()]
    );
}

#[test]
fn with_star_passes_every_variable_through() {
    // This returned zero columns and then failed at runtime with "Variable not
    // found", because two copies of the item parser had drifted and the one
    // used by `WITH` dropped item kinds it did not recognise — silently.
    assert_eq!(
        columns(&chain(), "MATCH (a)-[r]->(b) WITH * RETURN a, r, b"),
        vec!["a".to_string(), "b".to_string(), "r".to_string()]
    );
}

#[test]
fn a_with_narrows_what_a_later_star_expands_to() {
    // Scope, not "everything ever bound": after `WITH a.n AS name` only
    // `name` exists.
    assert_eq!(
        columns(&chain(), "MATCH (a:A) WITH a.n AS name RETURN *"),
        vec!["name".to_string()]
    );
}

#[test]
fn return_star_survives_order_by() {
    let store = chain();
    assert_eq!(rows(&store, "MATCH (n) RETURN * ORDER BY n.n"), 3);
}
