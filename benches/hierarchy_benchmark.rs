//! Criterion micro-benchmarks for the OEH hierarchy index (ADR-035).
//!
//! Where the HIER corpus (`examples/hier_benchmark.rs`) measures whole Cypher queries, this
//! measures the index itself: build cost per encoding, order-test throughput, roll-up
//! latency as a function of subtree size, and descendant enumeration. The roll-up curve is
//! the one to watch — it should be flat in the subtree size, which is the whole claim.
//!
//! ```bash
//! cargo bench --bench hierarchy_benchmark
//! ```

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use samyama::graph::NodeId;
use samyama::index::hierarchy::{oeh::Encoding, OehIndex, Poset, RollupOp, RollupValue};

/// Balanced tree with `depth` levels of `fanout`. Node ids are dense from 0.
fn balanced_tree(depth: usize, fanout: usize) -> Poset {
    let mut edges = Vec::new();
    let mut frontier = vec![0u64];
    let mut next = 1u64;
    for _ in 0..depth {
        let mut new_frontier = Vec::new();
        for &p in &frontier {
            for _ in 0..fanout {
                edges.push((NodeId(next), NodeId(p)));
                new_frontier.push(next);
                next += 1;
            }
        }
        frontier = new_frontier;
    }
    Poset::from_edges(edges, std::iter::empty()).unwrap()
}

/// Layered multi-parent DAG — the chain-encoding regime.
fn layered_dag(layers: usize, width: usize) -> Poset {
    let mut edges = Vec::new();
    let id = |layer: usize, i: usize| NodeId((layer * width + i) as u64);
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

fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("hierarchy/build");
    for (label, depth, fanout) in [("1k", 4, 6), ("10k", 5, 6), ("100k", 6, 7)] {
        let p = balanced_tree(depth, fanout);
        group.throughput(Throughput::Elements(p.n() as u64));
        group.bench_with_input(BenchmarkId::new("nested-set", label), &p, |b, p| {
            b.iter(|| OehIndex::build(black_box(p.clone())).unwrap())
        });
    }
    let dag = layered_dag(40, 40);
    group.throughput(Throughput::Elements(dag.n() as u64));
    group.bench_with_input(BenchmarkId::new("chain", "1.6k"), &dag, |b, p| {
        b.iter(|| OehIndex::build(black_box(p.clone())).unwrap())
    });
    group.finish();
}

fn bench_subsumption(c: &mut Criterion) {
    let mut group = c.benchmark_group("hierarchy/subsumes");

    let tree = balanced_tree(5, 6);
    let n = tree.n();
    let idx = OehIndex::build(tree).unwrap();
    group.throughput(Throughput::Elements(1000));
    group.bench_function("nested-set", |b| {
        b.iter(|| {
            let mut hits = 0u32;
            for i in 0..1000u32 {
                let x = (i * 7) % n as u32;
                let y = (i * 13) % n as u32;
                if idx.subsumes(black_box(x), black_box(y)) {
                    hits += 1;
                }
            }
            hits
        })
    });

    let dag = layered_dag(40, 40);
    let dn = dag.n();
    let didx = OehIndex::build(dag).unwrap();
    group.bench_function("chain", |b| {
        b.iter(|| {
            let mut hits = 0u32;
            for i in 0..1000u32 {
                let x = (i * 7) % dn as u32;
                let y = (i * 13) % dn as u32;
                if didx.subsumes(black_box(x), black_box(y)) {
                    hits += 1;
                }
            }
            hits
        })
    });
    group.finish();
}

/// The claim under test: roll-up latency is independent of subtree size.
///
/// Each input is the root of a subtree an order of magnitude larger than the last. A flat
/// curve here is what "index-resident" means; an O(subtree) aggregation would climb.
fn bench_rollup_by_subtree_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("hierarchy/rollup_by_subtree");
    let tree = balanced_tree(6, 7);
    let n = tree.n();
    let mut idx = OehIndex::build(tree).unwrap();
    idx.set_measure(unit_measure(n), &[RollupOp::Sum, RollupOp::Max]);

    // Pick one root per distinct subtree size, smallest first.
    let mut seen: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
    for v in 0..n as u32 {
        seen.entry(idx.descendant_count(v)).or_insert(v);
    }
    for (size, root) in seen {
        group.bench_with_input(BenchmarkId::new("sum", size), &root, |b, &root| {
            b.iter(|| idx.rollup(black_box(root), RollupOp::Sum))
        });
        group.bench_with_input(BenchmarkId::new("max", size), &root, |b, &root| {
            b.iter(|| idx.rollup(black_box(root), RollupOp::Max))
        });
    }
    group.finish();
}

/// Enumerating a subtree from the index, for comparison with an expansion.
fn bench_descendants(c: &mut Criterion) {
    let mut group = c.benchmark_group("hierarchy/descendants");
    let tree = balanced_tree(6, 7);
    let idx = OehIndex::build(tree).unwrap();
    for root in [0u32, 1, 8] {
        let size = idx.descendant_count(root);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("nested-set", size), &root, |b, &root| {
            b.iter(|| idx.descendants(black_box(root)).len())
        });
    }
    group.finish();
}

/// Forcing chain mode on a tree measures the two encodings over identical data.
fn bench_encoding_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("hierarchy/encoding");
    let n = balanced_tree(4, 6).n();
    let nested = OehIndex::build_forced(balanced_tree(4, 6), Encoding::NestedSet).unwrap();
    let chain = OehIndex::build_forced(balanced_tree(4, 6), Encoding::Chain).unwrap();
    group.bench_function("nested-set/subsumes", |b| {
        b.iter(|| {
            (0..500u32)
                .filter(|i| nested.subsumes(black_box((i * 7) % n as u32), 0))
                .count()
        })
    });
    group.bench_function("chain/subsumes", |b| {
        b.iter(|| {
            (0..500u32)
                .filter(|i| chain.subsumes(black_box((i * 7) % n as u32), 0))
                .count()
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_subsumption,
    bench_rollup_by_subtree_size,
    bench_descendants,
    bench_encoding_comparison
);
criterion_main!(benches);
