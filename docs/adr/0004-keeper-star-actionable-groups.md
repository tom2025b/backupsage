# ADR 0004 — Keeper-star actionable classification for near-duplicate groups

Date: 2026-08-02 · Status: accepted · Milestone: v1.0.2 (issue #9)

## Context

Near-duplicate groups are the connected components of the verified
within-threshold pair graph: MIH finds every pair within hamming distance
`--threshold` (recall proven against a brute-force scan), and union-find
merges chains of such pairs into one group. Transitivity is the point — a
burst of edits A→B→C belongs together for *review* — but it silently
implies a guarantee nobody measured: A and C can sit in one group while
their pairwise distance exceeds the threshold. Every non-keeper member was
counted as a reclaimable duplicate regardless, and `hamming_to_keep` was
display-only. The v1.2 contract already commits to "near duplicates are
never selected automatically"; v1.1's plan generation needs a
representation that makes the safe subset explicit, per file, before any
action code exists. That is issue #9's mandate, and the future-roadmap
design named the two candidate shapes: "explicit edges or keeper-star-safe
groups."

## Decision

- **Review groups stay exactly as they are.** Union-find components remain
  the group shape — no re-partitioning, no group splits. Their honest
  meaning ("connected by a chain of verified pairwise matches") is now
  documented on `near_components`, the extracted grouping function.
- **Keeper-star classification, per member, additive.** A member is
  `actionable` only when its distance to the keeper was *directly
  measured* and is within the run's threshold — exact members by content
  identity, near members by `hamming_to_keep <= threshold`; keepers,
  shadowed rows and hardlinks never. One pure function
  (`member_is_actionable`, fail-closed) is the single source of the rule,
  echoed in-band as `params.actionable_rule`.
- **Accounting counts only what the rule admits.** `reclaimable_bytes` and
  `duplicate_files` now cover actionable members only; transitive-only
  members land in new additive counters (`Group.review_only_bytes`,
  `Summary.transitive_only_files`/`review_only_bytes`) and carry a
  `[review-only]` tag in the terminal rendering. This is a value-level
  correctness fix to match the fields' documented meaning, shipped inside
  report `version: 1` under the additive-only policy; the fixture re-bless
  diff shows only added keys and unchanged existing values.
- **The oracle is the acceptance bar.** A grouping-level brute-force
  oracle (`grouping_matches_brute_force_oracle_at_every_threshold`) proves
  at every threshold 0..=3 that pipeline components equal the components
  of the brute-force pair graph, and that classification admits exactly
  the members measured within threshold of *any* keeper choice — with an
  anti-vacuity assertion that the corpus really produces transitive-only
  members. A real-image chain (A 640×480 keeper, B at distance 2, C at
  distance 4 via B) exercises the same guarantee end-to-end and is frozen
  non-degenerately in `tests/fixtures/contract/dedup_chain.json`.

## Alternatives considered

- **Explicit pairwise edge lists in the JSON** — the other shape the
  roadmap design named. Rejected: O(n²) payload on large groups,
  overspecifies what v1.1 planning needs, and the safety question a plan
  must answer is only ever "is this file measured-safe relative to the
  keeper being retained" — exactly the star, not the full graph. Edges can
  be added later additively if a UI wants them.
- **Re-partitioning clusters so every group is pairwise-within-threshold**
  (e.g. splitting components until diameter ≤ threshold) — rejected:
  destroys the review affordance (edit chains scatter across groups),
  partition choice is unstable under member insertion, and it still needs
  a keeper rule afterwards; safety comes from classification, not from
  reshaping groups.
- **Client-side derivation (`hamming_to_keep` vs `params.threshold`)
  instead of a field** — rejected: every consumer would re-implement the
  rule, including its shadowed/hardlink/keeper exclusions, and one of them
  would get it wrong; the fail-closed rule belongs in exactly one place.
- **Overloading exit code 2 or `has_skips()` to flag transitive-only
  members** — rejected: exit 2 is documented as archive
  reachability/completeness; a routine, correct grouping outcome is not a
  skip condition.

## Consequences

- v1.1 plan generation gets its hard selection floor for free: only
  `actionable: true` members may ever be auto-proposed, and the v1.2
  "explicit per-file selection" contract for near dups has a typed,
  fixture-frozen representation to hang selections on.
- `max_distance` keeps its documented keeper-relative meaning and now has
  company: a group whose `max_distance` exceeds the threshold visibly
  carries review-only members instead of silently inflating reclaimable
  space. Group ordering also gained a deterministic tie-break (smallest
  member identity), closing the documented HashMap-order fragility noted
  in the contract corpus.
- Every dedup fixture re-bless in this change is additive-only;
  `dedup_chain.json` joins the frozen set (8 fixtures) so the new fields
  are pinned at non-degenerate values per ADR 0003's residual-gaps note.
- Reported reclaimable numbers shrink on corpora with transitive chains —
  by exactly the bytes that were never measured safe. README and
  compatibility docs state the fix; scripts consuming `reclaimable_bytes`
  see honest values, not a shape change.
- The oracle pattern extends: v1.3's benchmarked MIH work (thresholds
  above 3) inherits a grouping-level oracle it must keep green, not just
  the pairwise recall test.
