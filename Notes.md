# Notes

### 2026-09-09 — Git submodules, Phase 1 (read-only)

Context: upstream jj-vcs/jj#494 has had no owner since 2023; the design docs
(`docs/design/git-submodules.md`, `git-submodule-storage.md`) fix the bounds:
phased roadmap, submodules as full jj repos in the `SubmoduleStore`, no
workspace per submodule, all interaction through the superproject, warn-never-
crash. Implemented the Phase 1 vertical slice within those bounds.

Decisions:
- `.gitmodules` parsed with `gix::submodule::File` (already a transitive dep via
  gix `attributes`). Upstream removed its own parser in `fe9e71b90` for exactly
  this reason; don't reintroduce one.
- Store layout `.jj/repo/submodule_store/<percent-encoded name>`; names are
  untrusted repo content so `/`, `..`, leading `.` are encoded.
- Submodule repos created with `ReadonlyRepo::init` + `GitBackend::init_internal`
  (no `Workspace`), fetched with the existing `GitFetch` pipeline with
  `abandon_unreachable_commits: false` (superproject may reference commits on no
  branch). Remote is always `origin`.
- Working-copy population = one persisted `TreeState` per submodule under
  `.jj/working_copy/submodules/<encoded path>`, rooted at the submodule dir and
  backed by the submodule's store. Keyed by *path*, not name, so clearing needs
  no name lookup. Clear-before / populate-after the superproject checkout is
  essential: a cleared-too-late dir has no `.git` and no `GitSubmodule` file
  state, so its files would be snapshotted as new superproject files.
- `WorkingCopyFactory::{init,load}_working_copy` gained a `submodule_store`
  param (only honest way to reach the store from the working copy). Two new
  default-no-op `LockedWorkingCopy` methods: `populate_submodules`,
  `clear_submodule` (used by `jj git submodule populate` and `delete`).
- Misses (not cloned, commit not fetched) go to `CheckoutStats::skipped_submodules`
  and print as warnings; the old `eprintln!("ignoring git submodule")` is gone.
- `DefaultSubmoduleStore::load_repo` uses `default_backend_factories()` only
  (ponytail: custom backends hosting submodules are hypothetical).

Dead ends / gotchas:
- `LockedWorkingCopy` is `#[async_trait]` (Send futures); holding `&self` of
  `LockedLocalWorkingCopy` across an await fails because `OnceCell<TreeState>`
  isn't Sync. Helpers that await take `&Store` instead.
- `description('x')` in revsets is no longer a substring match; use bookmarks in
  tests.
- Populate must `create_dir_all` the submodule dir itself; `create_parent_dirs`
  only makes parents of entries.

Smoke test on real repos (jq, bat with 93 submodules), same day:
- Found and fixed: local edits inside a populated submodule were clobbered on
  `jj new`, and untracked files left in a cleared submodule dir leaked into the
  next superproject snapshot. Fix: pre-pass snapshots each affected nested
  state (throwaway, compares tree ids, not the "state changed" flag which fires
  on mtime bumps); dirty submodules are kept and their *old gitlink stays in the
  checked-out tree*, so the wc commit shows `A sub`/modified gitlink. Excluding
  the path from the superproject update instead violated the debug invariant
  `file_states paths == tree paths`.
- 93 uncloned submodules printed 93 warnings on clone; capped at 5 + summary.
- bat: clone --all 1m42s; `jj new` populating 7860 files across 92 nested
  states ~5s, clearing ~1s. Gitlink pointing at a commit on no branch warns
  correctly (Git would fetch by sha; we don't).
- jq history walk incl. gitlink bump and vendor→modules path move worked; the
  moved submodule has a new name so it needs its own clone, same as Git.

Not done (next slice): auto-clone on `jj git clone`, auto-fetch on `jj git
fetch`, `jj git submodule sync`, `jj status` integration, Phase 2 snapshotting
(hook = nested `TreeState::snapshot`), gitlink conflict handling (#7806, #7386
panic in builtin merge tool still open).

### 2026-10-06 — submodules: auto-clone on clone, auto-fetch on fetch, gitlinks by hash

- `jj git clone` clones + populates submodules of the checked-out commit;
  `jj git fetch` fetches stored submodules then populates. Both best effort,
  gated by `git.clone-submodules` / `git.fetch-submodules` (default true).
- `fetch_submodule` takes `wanted` commit ids: after branches/tags, any still
  missing are fetched by hash, one refspec at a time (git aborts the whole
  fetch on one refused sha), into `refs/jj/gitlink/<hex>` so import_refs
  ignores them but they stay alive. Refusal is logged, not an error.
- Gotcha: wanting only `@`'s gitlinks is wrong for `jj git fetch` — the new
  gitlink is in the just-fetched remote bookmark, not `@`. Fetch now also
  scans `changed_remote_bookmarks` new targets.
- Tests need `uploadpack.allowAnySHA1InWant=true` on the file:// submodule
  remote to serve a no-branch commit.

Still not done: `jj git submodule sync`, `jj status` integration, Phase 2
snapshotting, gitlink conflict handling.

### 2026-10-06 — submodules: relative URLs, URL sync on fetch

- Bug found: relative `.gitmodules` URLs (`../x.git`) were passed through
  raw. Now resolved like Git via `resolve_submodule_url` against the
  superproject's `origin` fetch URL (fallback: workspace root). Handles
  scp-like `host:path`. Ponytail: always `origin`; Git prefers the current
  branch's upstream remote.
- No `jj git submodule sync` command: fetch compares the submodule's stored
  origin URL with the (resolved) URL from `@`'s `.gitmodules` and updates it.
  Must re-`load_repo` afterwards; `load_at_head` on the old loader keeps the
  stale gix config.
- Auto-fetch from `jj git fetch` is now best effort (dead submodule URL was
  failing the whole fetch after the superproject fetch succeeded); explicit
  `jj git submodule fetch` still errors.
- Known wrinkle: URLs come from `@`, so after upstream moves a submodule the
  first `jj git fetch` warns; it syncs once the new commit is checked out.
- Test gotcha: `git commit -a` in the superproject deletes the gitlink because
  the test removes git's submodule checkout; `git add .gitmodules` instead.

### 2026-10-07 — submodules: `jj status` integration

- `jj status` prints a `Submodules:` section for submodules of `@` that are
  not cloned, whose gitlink commit isn't fetched, or that have local changes.
- Dirty check needs the nested tree state, so: new default-no-op
  `LockedWorkingCopy::dirty_submodules()` (local impl = `load_nested` +
  `nested_is_dirty`). Status takes the wc lock for it and drops it unfinished
  (nothing changes). Repos without `.gitmodules` return before the lock.
- Cost: one throwaway nested snapshot per populated submodule per `jj status`
  (bat-sized repo: ~92 nested snapshots). Fine for status; would be too much
  on every command, which is the Phase 2 snapshotting problem anyway.
- Ponytail: the status path filter doesn't apply to the submodule section.
- Cleaned up the `--no-default-features` build: 10 warnings from the earlier
  submodule commits (whole trait methods now `#[cfg(feature = "git")]`, relying
  on the trait's default no-ops). 3 remaining CLI warnings are upstream.
- Gotcha: `cargo insta test` without `--accept` reports "ok" with pending
  snapshots; a mismatching snapshot does not fail the run.

### 2026-10-07 — falsifier for Phase 2 (snapshot submodules every command): timing on bat

Release build, warm cache, tmpfs, hyperfine 15 runs. bat: superproject 1007
files, 93 populated submodules, 7865 files total (via the new auto-clone on
`jj git clone`: 92s, no failures).
- `jj log -r @` (superproject snapshot only): 17 ms
- `jj status`, nested states hidden (93 `load_repo` + `get_commit`): 54 ms
- `jj status` full (+93 throwaway nested snapshots): 168 ms
- Control: same 7772 files flat in one plain repo, `jj log -r @`: 30 ms

So per-file cost is ~2 µs (cheap); the ~115 ms of nested snapshots is ~1.2 ms
fixed cost per submodule (gix repo open + store + `TreeState::load`, run
serially), and repo loading alone is ~0.4 ms each.
Verdict: Phase 2 is NOT killed, but naive per-command nested snapshots would
add ~150 ms on a 93-submodule repo. Needed first: skip submodules whose
directory is unchanged (cheap stat of the nested state / dir mtimes before
loading anything), load stores lazily, and run nested snapshots in parallel.

### 2026-10-08 — submodules: skip unchanged submodules before loading them

- `nested_maybe_dirty(state_path, dir)`: walks the submodule dir and returns
  false only if every entry's max(mtime, ctime) is older than the nested
  `tree_state` file's mtime. ctime catches chmod/renames that keep mtime.
  Equal timestamps, unreadable entries, missing state → "maybe dirty" (full
  check). Windows: mtime only.
- Wired into `dirty_submodules` (jj status) only; it's also the gate Phase 2's
  per-command nested snapshot will need.
- Limitation: walks ignored files too; a build dir inside a submodule keeps it
  "maybe dirty". Also the throwaway dirty check never saves state, so a
  submodule edited then reverted stays "maybe dirty" until repopulated.
- Unit test moves the state file's mtime ±1h (ctime can't be set).
- Benchmark on bat NOT rerun yet: release rebuild was killed by the harness
  for low system memory. Expected ~168 ms → ~65 ms for `jj status`.
- Follow-up same day, measured on bat (dev profile at opt-level 3, since a
  release build gets killed for low memory): the skip first did NOTHING
  (172 ms both ways). Cause: Linux file timestamps come from a coarse clock,
  so populate's last files and the saved `tree_state` share an mtime to the
  ns; the conservative `>=` flagged every submodule, and the throwaway check
  never re-saved. Fix (racy-git style): when the full check finds a submodule
  clean, `dirty_submodules` saves its state (same tree, refreshed file
  states), moving the mtime past the tick. Result: `jj status` 172 → 71.5 ms
  (`jj log -r @` 21 ms). Edit inside a submodule still detected; revert
  clears it.
- A/B trick without two builds: `touch` every submodule dir forces the full
  path in the same binary.

### 2026-10-10 — back to the Phase 1 plan; `jj git submodule set`

- Checked against docs/design/git-submodules.md: Phase 1 outcomes still open
  were "update gitlink to an existing submodule commit", "resolve gitlink
  conflicts", "view submodule history". The status/perf work was drift.
  Design-doc "Design" sections are our decisions, not maintainer-approved.
- Known deviation from git-submodule-storage.md ("commands only interact with
  the submodule repo as an entire unit, never its commit backend directly"):
  the nested working copy reads the submodule store directly. Raise upstream.
- Dropped: lazy repo loading in `jj status` (~36 ms on bat; not on the plan).
- `jj git submodule set <name> <SUBMODULE_REVSET> [-r REV]`: revset parsed in
  the superproject (its aliases), resolved+evaluated in the submodule repo
  (same trick as `jj op diff`), must be exactly one commit. Rewrites the
  gitlink with MergedTreeBuilder; normal checkout repopulates. Writing a plain
  value also resolves a conflicted gitlink, covering that outcome from the CLI.
- UX wart: submodule repos have no local bookmarks (fetch auto-tracks
  nothing), so it's `next@origin`, not `next`. The error hint says so.
- Fixed stale design-doc line (auto clone/fetch) and CHANGELOG entry.
- Next Phase 1 item: view submodule history from the superproject.

### 2026-10-10 — `jj git submodule log` (last Phase 1 outcome)

- Chose a dedicated read-only subcommand over a global `--submodule` flag
  (cheapest way to meet "submodule history and branches can be viewed"; the
  flag is a global CLI change for maintainers to weigh in on).
- Revsets parsed in the superproject, evaluated in the submodule repo (same
  as `set`). Default is self-contained: `ancestors(present(<gitlink of @>) |
  remote_bookmarks(), 10)`. `revsets.log`/`trunk()` would name superproject
  bookmarks.
- Gotcha found by test: templates' `immutable` evaluates the superproject's
  `immutable_heads()` in the submodule → "Revision master@origin doesn't
  exist" after `jj git clone` sets trunk(). Fix: new
  `WorkspaceCommandEnvironment::commit_template_language_with_immutable`;
  submodule log passes `all()` (read-only in Phase 1).
- Graph loop is a trimmed copy of cmd_log's (no prioritization, elided nodes,
  patches); marked ponytail.
- Not done: marking which commit the superproject points at in the graph.
- With this, all six Phase 1 outcomes have a CLI path. The conflict one is
  CLI-only (`set`); the builtin merge tool panic (#7386) is still open.
