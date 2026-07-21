# BackupSage Roadmap Publication Implementation Plan

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

- [ ] **Step 1: Create the public roadmap**

Write each milestone's outcome, dependency, issue checklist, and exit gate in `docs/ROADMAP.md`. Include the archive-immutability invariant and explain why mutating actions wait until v1.2.

- [ ] **Step 2: Replace the stale README summary**

Replace the two-bucket `v1.1`/`v2.0` paragraph with the seven-release sequence and a relative link to `docs/ROADMAP.md`.

- [ ] **Step 3: Validate Markdown and scope**

Run:

```bash
git diff --check
rg -n "TBD|TODO|implement later" README.md docs/ROADMAP.md docs/superpowers/specs/2026-07-21-backupsage-future-roadmap-design.md
git status --short
```

Expected: `git diff --check` exits 0; placeholder search returns no matches; status lists only the four roadmap files.

- [ ] **Step 4: Review the complete patch**

Run:

```bash
git diff -- README.md docs/ROADMAP.md docs/superpowers/specs/2026-07-21-backupsage-future-roadmap-design.md docs/superpowers/plans/2026-07-21-backupsage-roadmap-publication.md
```

Expected: no source code, dependency, workflow, or unrelated file changes.

- [ ] **Step 5: Commit with the required identity**

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

- [ ] **Step 1: Re-read live GitHub state**

List labels, open/closed milestones, and issues. Search issue titles before creating anything so reruns are idempotent.

- [ ] **Step 2: Upsert labels**

Create or update: `roadmap`, `safety-critical`, `release`, `correctness`, `performance`, `privacy`, `area:indexing`, `area:actions`, `area:media`, `area:api`, `area:web`, and `area:remote`.

- [ ] **Step 3: Upsert milestones**

Create the seven milestones without due dates. Each description states its outcome and the gate that must be met before the next risk level opens.

- [ ] **Step 4: Verify taxonomy**

Fetch the labels and milestones again. Expected: twelve named roadmap labels and exactly seven open version milestones, with no duplicates.

### Task 3: Create milestone issues

**Files:** none

**Interfaces:**
- Consumes: the 33 numbered parent-issue definitions in the design spec and GitHub milestone numbers from Task 2
- Produces: 33 open, unassigned parent tracking issues with milestone and labels

- [ ] **Step 1: Search before each create**

Use exact repository/title search. If an exact title already exists, update its missing milestone/labels instead of creating a duplicate.

- [ ] **Step 2: Create issues milestone by milestone**

Create 5 issues for v1.0.1, 4 for v1.0.2, 5 for v1.1, 5 for v1.2, 6 for v1.3, 5 for v2.0, and 3 for v2.1. Every body contains rationale, scope, acceptance criteria, dependencies, and safety invariants where relevant.
Mark these as parent tracking issues in their bodies; implementation kickoff
must decompose any multi-change parent into linked child issues with exact
tests before code begins.

- [ ] **Step 3: Verify issue assignment**

Fetch open issues grouped by milestone. Expected counts are `5, 4, 5, 5, 6, 5, 3`; every issue has `roadmap`; no issue has an assignee.

### Task 4: Publish the documentation PR and verify the handoff

**Files:** none

**Interfaces:**
- Consumes: committed roadmap branch and verified GitHub tracking objects
- Produces: pushed branch, draft PR to `main`, and durable task summary

- [ ] **Step 1: Push without deleting any branch**

Run:

```bash
git push -u origin agent/future-roadmap
```

Expected: remote tracking is configured and all existing branches remain.

- [ ] **Step 2: Open a draft PR**

Title: `docs: publish risk-gated future roadmap`

The body summarizes the release ladder, safety rationale, GitHub milestones/issues, and documentation-only validation.

- [ ] **Step 3: Verify final state**

Confirm the branch commit, draft PR target, milestone counts, issue counts, and clean worktree. Save `2026-07-21_backupsage-future-roadmap_summary.md` in `/home/tom/projects/_claude-outputs/`.
