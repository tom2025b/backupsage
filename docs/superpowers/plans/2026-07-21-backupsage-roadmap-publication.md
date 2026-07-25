# BackupSage Roadmap Publication Implementation Plan

> **Resolution (2026-07-24, #54):** This plan shipped in full via PR #42 (roadmap docs, GitHub labels/milestones/issues, merged 2026-07-24), with follow-ups PR #52 (CI audit permissions) and PR #53 (v1.0.1 ticked in the roadmap). Every checklist item below is resolved: checked with evidence, migrated to a live issue, or rejected with rationale.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish the approved risk-gated BackupSage roadmap in the repository and represent it as verified GitHub labels, milestones, and issues.

**Architecture:** The design spec records product and safety decisions; `docs/ROADMAP.md` is the concise public source of truth; the README links to it. GitHub milestones mirror release gates and GitHub issues mirror the numbered work items. No dates or assignees are invented.

**Tech Stack:** Markdown, Git, GitHub REST API/connected GitHub app

## Global Constraints

- Preserve the permanent invariant: BackupSage never rewrites or deletes from an archive.
- Use exactly seven milestones: `v1.0.1`, `v1.0.2`, `v1.1`, `v1.2`, `v1.3`, `v2.0`, and `v2.1`.
- Create no due dates and assign no person during roadmap publication.
- Leave the output-alias bug unmodified until Tom gives a separate go-ahead.
- Commit as `claude_2010 <262510778+tom2025b@users.noreply.github.com>`.
- Preserve existing branches locally and remotely.

---

### Task 1: Publish the repository roadmap

**Files:**
- Create: `docs/ROADMAP.md`
- Modify: `README.md:228`
- Reference: `docs/superpowers/specs/2026-07-21-backupsage-future-roadmap-design.md`

**Interfaces:**
- Consumes: the approved milestone sequence and issue inventory in the design spec
- Produces: one reader-facing roadmap linked from the README

- [x] **Step 1: Create the public roadmap** — evidence: PR #42 / commit 4326680 (adds `docs/ROADMAP.md`, 187 lines; 189 after the follow-up commit 86676a2 in the same PR)

Write each milestone's outcome, dependency, issue checklist, and exit gate in `docs/ROADMAP.md`. Include the archive-immutability invariant and explain why mutating actions wait until v1.2.

- [x] **Step 2: Replace the stale README summary** — evidence: PR #42 / commit 4326680 (README diff replaces the v1.1/v2.0 paragraph with the seven-release sequence and a relative link to `docs/ROADMAP.md`)

Replace the two-bucket `v1.1`/`v2.0` paragraph with the seven-release sequence and a relative link to `docs/ROADMAP.md`.

- [x] **Step 3: Validate Markdown and scope** — evidence: PR #42 (PR body's Validation section records `git diff --check` clean and a docs-only change set)

Run:

```bash
git diff --check
rg -n "TBD|TODO|implement later" README.md docs/ROADMAP.md docs/superpowers/specs/2026-07-21-backupsage-future-roadmap-design.md
git status --short
```

Expected: `git diff --check` exits 0; placeholder search returns no matches; status lists only the four roadmap files.

- [x] **Step 4: Review the complete patch** — evidence: PR #42 (merged file list is exactly the four roadmap documents; no source, dependency, or workflow changes)

Run:

```bash
git diff -- README.md docs/ROADMAP.md docs/superpowers/specs/2026-07-21-backupsage-future-roadmap-design.md docs/superpowers/plans/2026-07-21-backupsage-roadmap-publication.md
```

Expected: no source code, dependency, workflow, or unrelated file changes.

- [x] **Step 5: Commit with the required identity** — evidence: commit 4326680, authored `claude_2010 <262510778+tom2025b@users.noreply.github.com>` with message `docs: publish risk-gated future roadmap`

Run:

```bash
git add README.md docs/ROADMAP.md docs/superpowers/specs/2026-07-21-backupsage-future-roadmap-design.md docs/superpowers/plans/2026-07-21-backupsage-roadmap-publication.md
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "docs: publish risk-gated future roadmap"
```

Expected: one commit containing only roadmap documentation.

### Task 2: Create GitHub tracking taxonomy

**Files:** none

**Interfaces:**
- Consumes: the label list and release descriptions from the design spec
- Produces: twelve labels and seven open milestones in `tom2025b/backupsage`

- [x] **Step 1: Re-read live GitHub state** — evidence: PR #42 (resulting taxonomy has no duplicate labels, milestones, or issue titles, confirming the idempotent pre-read)

List labels, open/closed milestones, and issues. Search issue titles before creating anything so reruns are idempotent.

- [x] **Step 2: Upsert labels** — evidence: PR #42 (all twelve labels live in `tom2025b/backupsage`, verified 2026-07-24; PR body records "12 labels")

Create or update: `roadmap`, `safety-critical`, `release`, `correctness`, `performance`, `privacy`, `area:indexing`, `area:actions`, `area:media`, `area:api`, `area:web`, and `area:remote`.

- [x] **Step 3: Upsert milestones** — evidence: PR #42 (milestones 1–7 = `v1.0.1`…`v2.1`, all with `due_on: null` and gate-stating descriptions, verified 2026-07-24)

Create the seven milestones without due dates. Each description states its outcome and the gate that must be met before the next risk level opens.

- [x] **Step 4: Verify taxonomy** — evidence: PR #42 (re-fetched 2026-07-24: twelve roadmap labels and exactly seven version milestones, no duplicates; v1.0.1 since closed by shipping)

Fetch the labels and milestones again. Expected: twelve named roadmap labels and exactly seven open version milestones, with no duplicates.

### Task 3: Create milestone issues

**Files:** none

**Interfaces:**
- Consumes: the 33 numbered parent-issue definitions in the design spec and GitHub milestone numbers from Task 2
- Produces: 33 open, unassigned parent tracking issues with milestone and labels

- [x] **Step 1: Search before each create** — evidence: PR #42 (exactly 33 roadmap parent issues exist with unique titles — no duplicates from reruns)

Use exact repository/title search. If an exact title already exists, update its missing milestone/labels instead of creating a duplicate.

- [x] **Step 2: Create issues milestone by milestone** — evidence: PR #42 (33 parent tracking issues #3–#41 created with the 5/4/5/5/6/5/3 milestone split; PR body records "33 parent tracking issues")

Create 5 issues for v1.0.1, 4 for v1.0.2, 5 for v1.1, 5 for v1.2, 6 for v1.3, 5 for v2.0, and 3 for v2.1. Every body contains rationale, scope, acceptance criteria, dependencies, and safety invariants where relevant.
Mark these as parent tracking issues in their bodies; implementation kickoff
must decompose any multi-change parent into linked child issues with exact
tests before code begins.

- [x] **Step 3: Verify issue assignment** — evidence: PR #42 (verified 2026-07-24: all 33 parent issues carry `roadmap`, zero assignees; per-milestone counts matched 5/4/5/5/6/5/3 at creation — later child issues #43–#46 and #54–#57 account for today's higher totals)

Fetch open issues grouped by milestone. Expected counts are `5, 4, 5, 5, 6, 5, 3`; every issue has `roadmap`; no issue has an assignee.

### Task 4: Publish the documentation PR and verify the handoff

**Files:** none

**Interfaces:**
- Consumes: committed roadmap branch and verified GitHub tracking objects
- Produces: pushed branch, draft PR to `main`, and durable task summary

- [x] **Step 1: Push without deleting any branch** — evidence: PR #42 (head branch `agent/future-roadmap` pushed and still live on origin at commit 86676a2; all remote branches preserved)

Run:

```bash
git push -u origin agent/future-roadmap
```

Expected: remote tracking is configured and all existing branches remain.

- [x] **Step 2: Open a draft PR** — evidence: PR #42, titled `docs: publish risk-gated future roadmap`, opened as draft (timeline shows `ready_for_review` on 2026-07-24 just before merge) with the required summary body

Title: `docs: publish risk-gated future roadmap`

The body summarizes the release ladder, safety rationale, GitHub milestones/issues, and documentation-only validation.

- [x] **Step 3: Verify final state** — evidence: PR #42 (merged to `main` 2026-07-24; milestone/issue counts verified above; task summary saved at `/home/tom/projects/_claude-outputs/2026-07-21_backupsage-future-roadmap_summary.md`, dated 2026-07-21)

Confirm the branch commit, draft PR target, milestone counts, issue counts, and clean worktree. Save `2026-07-21_backupsage-future-roadmap_summary.md` in `/home/tom/projects/_claude-outputs/`.
