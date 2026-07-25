# Samyama Graph — LDBC SNB Interactive Benchmark

Samyama Graph's own results on the [LDBC Social Network Benchmark (SNB) Interactive](https://ldbcouncil.org/benchmarks/snb/) read workload (IS1–IS7 short reads, IC1–IC14 complex reads), at two scale factors. In-process (embedded) timing, 1 warm-up + 3 timed runs, median latency. Provenance: commit `31a7e77`, id-indexes built on all anchor labels.

| Scale | Nodes | Edges | Load |
|---|---|---|---|
| **SF1** | 3,181,724 | 17,256,038 | 78.5 s |
| **SF10** | 29,987,835 | 176,623,433 | 575 s |

## Short reads (IS1–IS7)

| Query | Name | SF1 | SF10 |
|---|---|---|---|
| IS1 | Person Profile | 0.04 ms | 0.02 ms |
| IS2 | Recent Posts by Person | 1.00 ms | 1.10 ms |
| IS3 | Friends of Person | 0.24 ms | 1.80 ms |
| IS4 | Post Content | 0.03 ms | 0.01 ms |
| IS5 | Post Creator | 0.03 ms | 0.02 ms |
| IS6 | Forum of Post | 0.06 ms | 0.06 ms |
| IS7 | Replies to Post | 0.54 ms | 11.50 ms |

## Complex reads (IC1–IC14)

| Query | Name | SF1 | SF10 |
|---|---|---|---|
| IC1 | Transitive Friends by Name | 319 ms | 14.0 s |
| IC2 | Recent Friend Posts | 18.20 ms | 306 ms |
| IC3 | Friends in Countries | 1.3 s | 15.7 s |
| IC4 | Popular Tags in Period | 37.50 ms | 527 ms |
| IC5 | New Forum Members | 1.4 s | 31.1 s |
| IC6 | Tag Co-occurrence | 1.5 s | 31.5 s |
| IC7 | Recent Likers | 0.52 ms | 1.70 ms |
| IC8 | Recent Replies | 0.63 ms | 4.00 ms |
| IC9 | Recent FoF Posts | 2.5 s | 26.3 s |
| IC10 | Friend Recommendation | 199 ms | 2.3 s |
| IC11 | Job Referral | 133 ms | 4.5 s |
| IC12 | Expert Reply | 224 ms | 3.2 s |
| IC13 | Single Shortest Path | 7.10 ms | 37.00 ms |
| IC14 | Trusted Connection Paths | 34.70 ms | 696 ms |

## Notes

- **Samyama is extremely fast on point and short reads** — IS1/IS4/IS5 are sub-0.1 ms at both scales (in-process index-free adjacency).
- **Complex multi-hop reads at scale are a known optimization area.** Several deep-traversal queries (IC1/IC3/IC5/IC6/IC9) grow super-linearly from SF1 to SF10 and are the focus of active planner/executor work — tracked in [issue #296](https://github.com/samyama-ai/samyama-graph/issues/296).
- Queries are LDBC-SNB-inspired Cypher adaptations; the runnable benchmark is `benches/ldbc_benchmark.rs` (`cargo bench --bench ldbc_benchmark -- --params-file <params.json> --data-dir <dataset>`).
- SF1 measured on a macOS i9-9980HK (32 GB); SF10 on a single 192 GB cloud VM.

_These are Samyama's own numbers, published for transparency. We're actively improving the complex-read path._
