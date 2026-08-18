//! Queries that must be rejected (openCypher TCK negative scenarios).
//!
//! 55 TCK scenarios assert only that a query is refused. An engine that runs
//! them anyway answers a question that was never well-formed, and two of those
//! answers are actively harmful:
//!
//! * `RETURN a AS x, b AS x` — one column has to win, and the caller silently
//!   loses the other;
//! * `MATCH (a) CREATE (a:Foo)` — reads as "add a label", which is what `SET`
//!   is for. Cypher makes it an error precisely because the intent is
//!   ambiguous.
//!
//! The tests come in pairs on purpose. A validation rule is only worth having
//! if it rejects the invalid form *and* leaves the valid one alone, and the
//! expensive failure here is the second half: rejecting a legal query would
//! break every loader and benchmark in this repo. That is also why scope
//! analysis ("is this variable defined?") is deliberately not implemented —
//! several TCK scenarios want it, and getting it slightly wrong costs more
//! than the scenarios are worth.

use samyama::query::parser::parse_query;

#[track_caller]
fn rejected(cypher: &str) {
    assert!(
        parse_query(cypher).is_err(),
        "should have been rejected, but parsed: {cypher}"
    );
}

#[track_caller]
fn accepted(cypher: &str) {
    assert!(
        parse_query(cypher).is_ok(),
        "should have been accepted, but was rejected: {cypher}"
    );
}

#[test]
fn two_result_columns_may_not_share_a_name() {
    rejected("MATCH (a), (b) RETURN a AS x, b AS x");
    rejected("MATCH (a) RETURN a, a");
    accepted("MATCH (a), (b) RETURN a AS x, b AS y");
}

#[test]
fn two_with_items_may_not_share_a_name() {
    rejected("MATCH (a) WITH a AS x, a AS x RETURN x");
    accepted("MATCH (a) WITH a AS x, a AS y RETURN x, y");
}

#[test]
fn unaliased_expressions_do_not_collide_with_each_other() {
    // Only aliases and bare variables produce a column name that can clash;
    // two `count(*)` items are given generated names. Rejecting these would
    // be over-reach.
    accepted("MATCH (a) RETURN count(*), count(*)");
}

#[test]
fn union_branches_must_have_the_same_columns() {
    rejected("MATCH (a) RETURN a AS x UNION MATCH (b) RETURN b AS y");
    accepted("MATCH (a) RETURN a AS x UNION MATCH (b) RETURN b AS x");
}

#[test]
fn union_and_union_all_may_not_be_mixed() {
    rejected(
        "MATCH (a) RETURN a AS x UNION MATCH (b) RETURN b AS x \
         UNION ALL MATCH (c) RETURN c AS x",
    );
    accepted("MATCH (a) RETURN a AS x UNION ALL MATCH (b) RETURN b AS x");
}

#[test]
fn create_may_not_add_labels_to_a_variable_match_already_bound() {
    rejected("MATCH (a) CREATE (a:Foo)");
    rejected("MATCH (a) CREATE (a {x: 1})");
}

#[test]
fn a_bare_re_mention_of_a_matched_variable_stays_legal() {
    // This is how you write an edge between two matched nodes, and it is the
    // single most common write query in this codebase. If the rule above ever
    // starts rejecting it, every loader breaks.
    accepted("MATCH (a) CREATE (a)");
    accepted("MATCH (a), (b) CREATE (a)-[:R]->(b)");
    accepted("MATCH (a:Person) CREATE (a)-[:OWNS]->(:Thing)");
}

#[test]
fn reusing_a_variable_inside_one_create_stays_legal() {
    // Bound by the CREATE itself rather than by a MATCH — legal, and the
    // subject of its own test file.
    accepted("CREATE (a), (b), (a)-[:R]->(b)");
    accepted("CREATE (a:A), (b:B), (a)-[:R]->(b)");
}

#[test]
fn ordinary_queries_are_unaffected() {
    // A spot check across the shapes this repo actually runs, because the
    // cost of a false rejection is much higher than the benefit of a true one.
    for q in [
        "MATCH (n) RETURN n",
        "MATCH (a)-[r]->(b) RETURN *",
        "MATCH (p:Person) WHERE p.age > 30 RETURN p.name AS name ORDER BY name LIMIT 10",
        "UNWIND [1, 2] AS a UNWIND [3, 4] AS b RETURN a, b",
        "MATCH (a) WITH a.x AS v, count(*) AS c WHERE c > 1 RETURN v, c",
        "MERGE (a:L) ON CREATE SET a.n = 1 ON MATCH SET a.n = 2",
        "MATCH (a) RETURN a.name AS name UNION ALL MATCH (b) RETURN b.name AS name",
    ] {
        accepted(q);
    }
}
