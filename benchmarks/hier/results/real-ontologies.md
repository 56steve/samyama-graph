# OEH on real ontologies

First run of the structural probe against ontologies **as published**, rather than against
the generated HIER dataset. Part of #353.

- **Date:** 2026-08-14
- **Host:** Vultr `voc-g-16c-64gb-320s-amd` (16 vCPU, 62 GB, Mumbai) — see *Right-sizing* below
- **Engine:** `samyama-graph` @ `57f0b03`, `cargo run --release --example ontology_loader`
- **Data:** downloaded at run time; nothing committed here. NCBI Taxonomy (public domain),
  GeoNames (CC BY 4.0), Gene Ontology / MONDO / HPO (CC BY 4.0).

## Results

| Ontology | Nodes | Covering edges | Verdict | Chain width vs cap | Order embedding | Build |
|---|---:|---:|---|---:|---:|---:|
| **NCBI Taxonomy** (`nodes.dmp`) | 2,944,492 | 2,944,491 | **nested-set** (tree) | n/a | **12.00 B/node** | **6.81 s** |
| GeoNames (`hierarchy.txt`, ADM) | 544,093 | 512,678 | declined | 477,763 vs 5,901 | — | 1.19 s |
| Gene Ontology (`go-basic.obo`) | 38,092 | 63,726 | declined | 24,135 vs 1,561 | — | 42 ms |
| HPO (`hp.obo`) | 19,836 | 24,378 | declined | 13,979 vs 1,126 | — | 19 ms |
| MONDO (`mondo.obo`) | 58,656 | 81,474 | **error: cycle** | — | — | — |

Peak RSS for the NCBI build was 3.5 GB — the whole 2.9M-node graph plus index.

## What this changes

**1. The tree case holds at scale.** NCBI Taxonomy is 2.9M nodes — more than double the 1.3M
the paper cites — and builds in under 7 seconds at exactly the predicted 12 bytes per node.
That is the claim, on real data, at scale.

**2. Gene Ontology declines, as predicted.** Width 24,135 against a cap of 1,561. ADR-035
and the paper both say a high-width DAG belongs on a 2-hop index and that OEH should refuse
rather than build something quadratic. Confirmed on the real file, not a synthetic
stand-in.

**3. GeoNames declines too — and that is a genuine gap, not a confirmation.**

This is the finding worth acting on. GeoNames' administrative hierarchy is **98.7% a tree**:

| Parents per child | Children |
|---:|---:|
| 1 | 498,385 |
| 2 | 5,714 |
| 3 | 679 |
| 4 | 113 |
| 5 | 34 |

6,540 nodes out of 504,925 have more than one parent. `is_tree()` requires *every* node to
have at most one, so those 6,540 exceptions disqualify the nested-set encoding for the whole
poset; chain decomposition then takes over and its width blows out to roughly the leaf count.

So a poset that is a tree apart from 1.3% of its nodes is handled as though it were not a
tree at all. The probe is not wrong — chain decomposition really is unaffordable here — but
there is **no encoding between "perfect tree" and "give up"**, and real geography lands
squarely in that gap. Geography is one of the three axes the paper unifies, which makes this
the most consequential result in the table.

**4. MONDO contains an `is_a` cycle.** The build correctly refuses: *"covering relation has a
cycle: only 58,633/58,647 nodes could be ordered"* — 14 nodes. It is not an artifact of
merging `is_a` with `part_of`: MONDO has only 3 `part_of` lines, and stripping them leaves
the cycle intact. The validator behaving this way is the design working — a cycle would make
every roll-up over it wrong — but the diagnostic should name the offending nodes so a user
can fix their data rather than just learn that 14 of 58,647 are bad.

## Honest summary

Of five real ontologies: one builds, three decline, one is rejected as cyclic. The paper's
"genuinely low-width multi-parent DAGs are rare in practice" is *understated* — in this
sample there were none at all. Every multi-parent ontology tested was high-width.

That does not invalidate the index; it sharpens where it applies. Strict hierarchies —
taxonomies, calendars, ATC-style coded classifications, administrative geography *if the
exceptions are handled* — are exactly its regime, and NCBI Taxonomy shows the payoff at 2.9M
nodes. But the HIER benchmark's generated DAG axis is more favourable than reality, and the
results should be read with that in mind.

## Right-sizing

The utilization sampler recorded **peak 4 GB of 62 GB** and **peak load 10.5 of 16 cores**,
almost all of it the Rust build rather than the workload. A 4-core / 8 GB instance would
have run this comfortably. The 16-core box was chosen for compile speed; next time, build
once and snapshot.

## Not covered

- **ATC** — WHO licences the classification; no redistributable source file. Needs the
  licensed download, gated behind `--i-have-a-licence`.
- **MITRE ATT&CK / CWE** — loadable today via `--format prefix` / `--format edgelist`,
  not yet run.
- Roll-up timings on this data: the loader declares the index but does not run the HIER
  query classes against real ontologies. That is the next step of #353.
