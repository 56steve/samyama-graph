//! Semantic checks the grammar cannot express (openCypher TCK negative cases).
//!
//! A parser that accepts everything is not lenient, it is wrong: the TCK has
//! 55 scenarios whose entire assertion is *"this query must be rejected"*, and
//! an engine that runs them anyway gives an answer to a question that was
//! never well-formed. Two of those answers are actively dangerous —
//! `RETURN a AS x, b AS x` silently drops a column, and `CREATE (a:Foo)` over
//! a variable already bound by `MATCH` reads as "add a label" when Cypher
//! says it is an error.
//!
//! Only rules that are **unambiguous and local** live here. Scope analysis
//! ("is this variable defined?") is deliberately absent: getting it slightly
//! wrong would reject valid queries, which is a far worse failure than
//! accepting an invalid one, and this engine's own benchmarks and loaders
//! would be the first casualties.

use std::collections::HashSet;

use crate::query::ast::{Expression, Query};

/// Why a query was rejected. Carries the offending name so the message can
/// say which one rather than that something, somewhere, was wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationError {
    DuplicateColumn(String),
    UnionColumnMismatch { left: Vec<String>, right: Vec<String> },
    MixedUnionAndUnionAll,
    CreateOnBoundVariable(String),
    CreateRelationshipWithoutType,
    CreateUndirectedRelationship,
    CreateVariableLengthRelationship,
    CreateOnBoundRelationship(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateColumn(name) => write!(
                f,
                "Multiple result columns with the same name are not supported: `{name}`"
            ),
            Self::UnionColumnMismatch { left, right } => write!(
                f,
                "All sub queries in an UNION must have the same column names: {left:?} vs {right:?}"
            ),
            Self::MixedUnionAndUnionAll => write!(
                f,
                "Cannot mix UNION and UNION ALL in the same query"
            ),
            Self::CreateOnBoundVariable(name) => write!(
                f,
                "Variable `{name}` already declared; CREATE cannot add labels or properties to it"
            ),
            Self::CreateRelationshipWithoutType => write!(
                f,
                "Exactly one relationship type must be specified for CREATE"
            ),
            Self::CreateUndirectedRelationship => write!(
                f,
                "Only directed relationships are supported in CREATE"
            ),
            Self::CreateVariableLengthRelationship => write!(
                f,
                "Variable length relationships cannot be created"
            ),
            Self::CreateOnBoundRelationship(name) => write!(
                f,
                "Variable `{name}` already declared; CREATE cannot rebind a relationship"
            ),
        }
    }
}

/// The column name a return item produces, if it has one that can collide.
///
/// An unaliased expression like `count(*)` has a generated name that Cypher
/// does not treat as a user-visible column for collision purposes, so only
/// aliases and bare variables are considered.
fn column_name(item: &crate::query::ast::ReturnItem) -> Option<String> {
    if let Some(alias) = &item.alias {
        return Some(alias.clone());
    }
    match &item.expression {
        Expression::Variable(v) => Some(v.clone()),
        _ => None,
    }
}

fn columns(query: &Query) -> Vec<String> {
    query
        .return_clause
        .as_ref()
        .map(|rc| rc.items.iter().filter_map(column_name).collect())
        .unwrap_or_default()
}

/// Variables a MATCH clause binds — the ones a later CREATE must not redeclare.
fn matched_variables(query: &Query) -> HashSet<String> {
    let mut out = HashSet::new();
    for mc in &query.match_clauses {
        for path in &mc.pattern.paths {
            if let Some(v) = &path.start.variable {
                out.insert(v.clone());
            }
            for seg in &path.segments {
                if let Some(v) = &seg.edge.variable {
                    out.insert(v.clone());
                }
                if let Some(v) = &seg.node.variable {
                    out.insert(v.clone());
                }
            }
        }
    }
    out
}

pub fn validate(query: &Query) -> Result<(), ValidationError> {
    // ---- Duplicate result columns.
    //
    // `RETURN a AS x, b AS x` cannot be answered: one of the two columns has
    // to win, and whichever it is, the caller silently loses data.
    let cols = columns(query);
    let mut seen: HashSet<&str> = HashSet::new();
    for c in &cols {
        if !seen.insert(c.as_str()) {
            return Err(ValidationError::DuplicateColumn(c.clone()));
        }
    }
    if let Some(wc) = &query.with_clause {
        let mut seen: HashSet<String> = HashSet::new();
        for item in &wc.items {
            if let Some(name) = column_name(item) {
                if !seen.insert(name.clone()) {
                    return Err(ValidationError::DuplicateColumn(name));
                }
            }
        }
    }

    // ---- UNION: same columns on both sides, and one flavour throughout.
    if !query.union_queries.is_empty() {
        let flavours: HashSet<bool> = query.union_queries.iter().map(|(_, all)| *all).collect();
        if flavours.len() > 1 {
            return Err(ValidationError::MixedUnionAndUnionAll);
        }
        for (sub, _) in &query.union_queries {
            let right = columns(sub);
            // An empty column list means the sub-query had no RETURN this
            // check can read; leaving it alone keeps the rule to what it can
            // actually see.
            if !cols.is_empty() && !right.is_empty() && cols != right {
                return Err(ValidationError::UnionColumnMismatch {
                    left: cols.clone(),
                    right,
                });
            }
            validate(sub)?;
        }
    }

    // ---- CREATE over a variable a MATCH already bound.
    //
    // `MATCH (a) CREATE (a:Foo)` looks like "add a label" and is not: Cypher
    // requires SET for that, and the CREATE form is an error. A *bare*
    // re-mention — `MATCH (a), (b) CREATE (a)-[:R]->(b)` — is how you write
    // an edge between matched nodes and stays legal.
    if let Some(create) = &query.create_clause {
        let bound = matched_variables(query);

        // A relationship being *created* has to say exactly what it is. These
        // are ambiguous rather than merely unsupported: `CREATE (a)-->(b)`
        // does not say what kind of edge to make, `CREATE (a)-[:R]-(b)` does
        // not say which way it points, and `CREATE (a)-[:R*2]->(b)` does not
        // say what the intermediate node is. Cypher rejects all three, and
        // accepting them means inventing an answer.
        //
        // Note this is only in CREATE. The same patterns are perfectly good in
        // MATCH, where they mean "any type", "either direction" and "two
        // hops" — which is why the check lives here and not in the grammar.
        for path in &create.pattern.paths {
            for seg in &path.segments {
                if seg.edge.length.is_some() {
                    return Err(ValidationError::CreateVariableLengthRelationship);
                }
                if seg.edge.types.len() != 1 {
                    return Err(ValidationError::CreateRelationshipWithoutType);
                }
                if matches!(seg.edge.direction, crate::query::ast::Direction::Both) {
                    return Err(ValidationError::CreateUndirectedRelationship);
                }
                if let Some(v) = &seg.edge.variable {
                    if bound.contains(v) {
                        return Err(ValidationError::CreateOnBoundRelationship(v.clone()));
                    }
                }
            }
        }
        for path in &create.pattern.paths {
            let mut check = |np: &crate::query::ast::NodePattern| -> Result<(), ValidationError> {
                if let Some(v) = &np.variable {
                    let adds_something = !np.labels.is_empty()
                        || np.properties.as_ref().is_some_and(|p| !p.is_empty())
                        || np.property_exprs.as_ref().is_some_and(|p| !p.is_empty());
                    if bound.contains(v) && adds_something {
                        return Err(ValidationError::CreateOnBoundVariable(v.clone()));
                    }
                }
                Ok(())
            };
            check(&path.start)?;
            for seg in &path.segments {
                check(&seg.node)?;
            }
        }
    }

    Ok(())
}
