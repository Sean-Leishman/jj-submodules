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

### 2026-10-17 — Submodule testing plan, first results

- Plan (oracle = invariants, checked after every step): I1 local submodule
  changes never clobbered; I2 submodule files never in superproject
  snapshots; I3 missing clone/commit/network = warning on automatic paths;
  I4 working tree matches `git checkout --recurse-submodules`; I5 undo/op
  restore keep submodule dirs consistent with @.
- Methods, in order: targeted insta tests per gap; Git differential script
  over real histories (scratchpad `diff/differential.sh`, not committed);
  randomized op sequences; failure injection; perf guard (no regression
  without submodules; bat status ≤ ~75 ms dev-opt3).
- Falsifier #1 found a real bug: `jj status --ignore-working-copy`/`--at-op`
  exited 1 in any repo with submodules (dirty check took the wc lock). Fixed
  by skipping the dirty check when the wc is read-only (ponytail).
- I5 + workspaces test passes: undo/op restore repopulate; second workspace
  gets its own checkout; abandon/rebase/delete keep local submodule edits.
- Wart (documented in test, not fixed): a dirty submodule left in place
  differs from @, so the next command snapshots its HEAD as the gitlink
  (Git-like, `git add -A`) and `jj undo` then needs to run twice.
- Unfetchable gitlink (commit gone upstream): clone/fetch/status all warn
  (I3 ✓). Fixed: `fetch_submodule` now returns still-missing hashes and the
  CLI says so; before, the failure was only `tracing::info!` and the only
  hint was "run `jj git submodule fetch`" — the command just run.
- Colocated superproject: `git status --porcelain` clean after jj populates.
- Git differential, jq: 221 commits over all history incl. the 2025
  `modules/` → `vendor/` move: 0 mismatches once the script follows the
  "not cloned; run clone" hint (old commits declare a differently *named*
  submodule; git's `--init` clones on demand, jj checkout stays offline by
  design).
- Git differential, bat (all history): unreliable oracle — git's own
  checkout failed on 44/61 old commits (dead URLs, commits gone). On the
  rest: nested submodules (Dart → sublime_plugin_lib) were left empty
  *silently*. Fixed: warning on checkout + "Submodules:" line in status
  (ponytail: no nested population). Also `build.bat` differs: git applies
  `.gitattributes` eol, jj doesn't — upstream jj, not submodule-related.
- Old gitlinks on no branch aren't fetched by `jj git clone` (only @'s);
  checkout warns with a fetch hint. By design (offline checkout).
- Git differential, bat (last 300 commits, 43 sampled, after the nested
  fix): git's checkout failed on 10 (skipped); on the other 33 every
  submodule file matches and status is clean. Only diff: `build.bat` eol.
- Not done yet from the plan: randomized op sequences, failure injection
  (offline, corrupt nested tree_state, killed mid-populate), perf guard
  re-measure, sparse/type-change/conflict-deletion targeted tests.

### 2026-10-17 — Review pass (code-review + falsifiers), fixes

- Ran `/code-review high` over the whole submodule range plus hand-made
  falsifiers in scratch repos. Real bugs found and fixed, all with tests:
  - `jj status` internal error when a submodule's path is a plain file on
    disk (nested snapshot read_dir failed); now "has local changes".
  - `jj git fetch --ignore-working-copy`/`--at-op` exited 1 with cloned
    submodules (auto-populate took the wc lock); populate is skipped there.
  - `rm -rf sub` then `jj git submodule populate` did nothing (nested state
    still said "populated"); a missing dir now resets the state.
  - A dirty submodule whose gitlink moved had the *old* gitlink written into
    the checkout tree, so the next snapshot recorded a gitlink revert that
    could be pushed. Now only a gitlink that *leaves* (or changes type) is
    kept; a moved one stays as the commit says, like Git's "modified
    content".
  - Leaving a submodule left ignored files (build dirs) behind without the
    gitlink, so the superproject snapshotted them as new files. Now the
    gitlink is kept with a warning; the cleared state counts as unpopulated
    so coming back repopulates around them.
  - Same leak via the "not in .gitmodules" branch (after undoing a kept
    gitlink): now kept too.
  - Malformed `.gitmodules` made `jj git clone`/`fetch` fail; now a warning.
  - Relative URLs with `jj git clone --remote=upstream` resolved against the
    workspace path; now fall back to the only remote.
  - `jj git submodule delete` silently deleted the repo of a dirty
    submodule, orphaning the edits; now refused.
  - `jj sparse set` could clear clean excluded submodules before erroring on
    a dirty one; dirtiness is checked first. (No test; needs two submodules.)
  - `populate` showed the same 5-warning cap its own hint pointed at.
- Verified-but-upstream (not ours): checkout doesn't apply `.gitattributes`
  eol; a path with a GitSubmodule file state is invisible to snapshot
  whatever is on disk (so type-change commits in tests use git plumbing).
- Learned: when the oracle is a reference implementation, assert the
  reference succeeded (git failed on 72% of old bat commits); `jj file list`
  lists gitlinks too, which fooled an experiment into testing nothing.
- jq differential re-run after the behaviour changes: 126 commits, 0
  mismatches.

### 2026-10-17 — Remaining plan items: randomized sequences, failure injection, sparse

- `test_git_submodule_random_sequences`: xorshift seeds 1..4 × 16 steps of
  edit/untracked/discard/new/undo/populate/set; after each, local edits
  intact, `sub/sub`+`sub/untracked` never in status/diff, no internal error.
  Passed first time once the earlier fixes were in (~10 s; bump seeds if it
  gets cheaper).
- Failure injection found two more I2 leaks, fixed: a submodule whose repo
  vanished from the store, or that was cleared leaving ignored files, lost
  its gitlink on checkout and its files were snapshotted as new superproject
  files. `prepare_submodules` now keeps the gitlink whenever it can't load
  the nested state and the gitlink leaves. `stays` also tolerates a missing
  *old* `.gitmodules` (a kept gitlink has none), else coming back to such a
  submodule looked like leaving it.
- Corrupt nested `tree_state` is read as empty (cleared) and self-heals on
  populate; a state dir without `tree_state` likewise. Populating over files
  already on disk never overwrites them ("skipped" warning, upstream text).
- Bad submodule URL: auto-clone warns, explicit clone errors.
- Two-submodule sparse test: refusing on one dirty submodule leaves the clean
  one untouched; now a user error (`CheckoutError::DirtySubmodule`), not
  "Internal error".
- bat differential re-run (43 commits, last 300): only `build.bat` eol diffs,
  same as before today's changes.
- Filed into the Nordorn vault (Draft notes, to be judged): "Assert the
  Oracle Ran", "Keep the Marker When You Can't Tell", "Racy Timestamps", plus
  a conversation source note. Left here as too jj-specific: one refspec per
  hash; parse-here-evaluate-there revsets.
- Not re-run: the perf guard on bat (heavy build). `refs/original/` backup
  from the trailer-stripping filter-branch is still around.

### 2026-10-20 — Rebased onto upstream main (256 commits, 7 Sep → 20 Oct)

- Conflicts were small: changelog/lockfile/one factory call in the first
  commit, then `git fetch` and `status` API drift, then changelog-only ones
  (resolved by a six-line "head.replace(base, theirs)" script).
- Upstream MSRV is now 1.97.1; installed with `rustup toolchain install
  1.97.1` next to the 1.96 default, build with `cargo +1.97.1`.
- API drift fixed in one follow-up commit: `signer_from_settings`,
  `ReadonlyRepo::init` takes a workspace-store initializer, `GitFetch::fetch`
  takes the ref expression itself, `MergedTreeBuilder::set_or_remove` takes
  `Option<TreeValue>` (kept gitlinks now go through `set_or_remove_merge`).
- Upstream `jj git fetch` no longer writes `refs/remotes/` in non-colocated
  repos (refs land in `refs/jj/remote-bookmarks/`); only the test helper that
  read the submodule's git log cared. New "Fetching from Git remotes" line in
  fetch snapshots.
- Verified with 1.97.1: workspace check clean, 16 submodule tests, 94
  working-copy tests, fmt. Old tip kept as branch `main-pre-rebase`.
- Drafted a comment for jj-vcs/jj#494 asking whether direct store access
  from the nested working copy is acceptable (scratchpad `upstream-post.md`,
  not posted; needs a public fork to link).

### 2026-11-15 — Folded the post-rebase drift fixes into the commits they fix

- `jj absorb` (system jj 0.43, in a `git clone --shared` scratch copy with
  `immutable_heads()` pinned to the upstream base) placed all hunks of the two
  drift-fix commits into 7 earlier commits; only the Cargo.lock line and one
  import removal needed hands. The import hunk conflicted because the removed
  line sat next to a line a later commit added; resolving it in the first
  commit left rebase-artifact conflicts in every descendant, cleared by
  restoring each commit's file from its own `at_operation(<absorb op>, X)`
  version bottom-up. Each commit then differed from its post-absorb tree only
  by the Cargo.lock line; head tree identical to the old head (ee7a17a38).
- Series is 19 commits on upstream 4eb41bedc; first commit type-checks alone
  (`cargo check -p jj-cli`). Other intermediate commits not checked yet.
- Old head kept as `main-pre-absorb`; `main-pre-rebase` and
  `refs/original/refs/heads/main` still exist too. Upstream main has moved
  again (4eb41bedc → 632115a5e); rebase again just before splitting into PRs.
- Lesson for scripted jj: `jj squash` opens `$EDITOR` to merge descriptions;
  set `JJ_EDITOR=true` or pass `--use-destination-message`. zsh does not
  word-split `$CMD`; use a shell function.

### 2026-11-15 — falsifier for Phase 2 (snapshot dirty submodules): passed

- `test_git_submodule_snapshot_by_hand` does the snapshot steps by hand: load
  the nested `TreeState` against the submodule repo's store, snapshot and
  save it, write a commit in a submodule-repo transaction (child of the
  gitlink target; the second time rewriting the first), then `jj git
  submodule set` to move the gitlink in `@`.
- All three kill checks pass with no change to the op log, the backend trait
  or any non-test code: (1) `jj new @-` / `jj edit` reproduces the files byte
  for byte (incl. a non-UTF-8 CRLF file) and the superproject diff is only
  `M sub`; (2) `git cat-file -p` reads the commit from the submodule's git
  dir, parent = old gitlink; (3) `jj undo` after the amending snapshot checks
  out the first snapshot commit, which the submodule repo no longer shows as
  visible, with no clobber warning; `jj redo` brings the second back.
- So Phase 2 is ours to build: wire these steps into the superproject
  snapshot path (behind `nested_maybe_dirty`), then `jj git submodule push`.
- Found while reading, not tested: undo works on a hidden submodule commit
  only because nothing gcs submodule repos. Superproject commits/ops are the
  only thing referencing amended-away snapshots, so a future submodule gc
  must treat superproject gitlinks as roots.
- Not run: clippy (not installed for 1.97.1).

### 2026-11-15 — falsifier for updating a snapshotted submodule (rebase): passed

- Question: once local submodule edits are a snapshot commit `B` on the old
  gitlink `A`, how do you move to a fetched upstream `C`? `set sub
  main@origin` would just check out `C` (edits stay in `B`, off the working
  copy). What's wanted is `B` rebased onto `C` and the gitlink on the result.
- `test_git_submodule_rebase_by_hand` does that through the library
  (`rewrite::rebase_commit` in a submodule-repo transaction, then `submodule
  set`). Clean case: edit kept, upstream file appears, undo/redo move both.
  Conflict case, the one expected to fail, also passes with no non-test
  change: the conflicted commit sits behind the gitlink, Git reads it, jj
  materializes the markers into the submodule file, the unchanged markers do
  NOT read as "local changes", undo returns the pre-rebase file, and writing
  a resolution + snapshot (amend) yields an unconflicted commit.
- Phase 2 plan gains `jj git submodule rebase <name> -d <revset>` (rebase the
  snapshot commit, move the gitlink). No general "run jj in the submodule"
  passthrough.
- Gaps seen, not built: the superproject shows nothing for a gitlink whose
  submodule commit is conflicted (`jj status` just says `M sub`; it should
  say so, and `submodule push` must refuse it); `jj git submodule log` has
  no `--no-graph`.
- Test hygiene: `hide_ids` only hid 12-40 hex chars, so the 128-char op id in
  "redo: restore to operation ..." leaked into a snapshot; widened to 128.
  Commits written from the test process get wall-clock timestamps, so their
  ids must never reach a snapshot.

### 2026-11-15 — Phase 2, step 1: submodule changes are snapshotted

- New default-no-op `LockedWorkingCopy::snapshot_submodules(wc_commit)`,
  called by the CLI's `snapshot_working_copy` right after the superproject
  snapshot. Local impl: per populated submodule that `nested_maybe_dirty`
  flags, snapshot the nested tree state, commit the tree in a submodule-repo
  transaction, move the gitlink in the superproject tree state. The other
  snapshot call sites (`file track/untrack`, `run`, `workspace remove`) don't
  call it; submodule edits just wait for the next ordinary command.
- Amend-or-child rule: the submodule repo's own view records the snapshot
  commit as the wc commit of the superproject's workspace name. Rewrite it
  iff the gitlink still equals that pointer AND no parent of the superproject
  `@` has that gitlink; else new child. So upstream commits and commits the
  superproject history refers to are never rewritten. Rejected: inferring
  "mutable" from remote bookmarks (commits fetched by hash are on no
  bookmark and would have been amended). Bonus: `submodule log` shows `@`.
- Only snapshot when the nested state's tree equals the gitlink commit's
  tree; otherwise (gitlink moved while the dir couldn't follow) the diff
  isn't just local changes, so it stays unsnapshotted and protected.
- Nested states of snapshotted submodules are saved in `finish()` with the
  superproject's, so an unfinished superproject snapshot is redone (cost: an
  orphan submodule commit).
- The per-submodule function is sync with `block_on`: `Transaction` isn't
  `Send` and the trait is `#[async_trait]`.
- Config `git.snapshot-submodules` (default true). Besides being an opt-out,
  it is how the Phase 1 "never clobber unsnapshotted changes" tests keep
  reaching that state: five of them set it to false and only changed wording.
  The random-sequence test runs in both modes; with snapshots on the
  invariant is "on disk or in some commit of the submodule repo".
- `jj git submodule delete` refuses when the repo has visible heads on no
  remote (the snapshot commits exist nowhere else); `--force` overrides and
  skips loading the repo.
- Known gaps: global ignores / auto-track / max-new-file-size don't apply in
  submodules; snapshot commits the superproject dropped stay visible heads
  forever (no gc, and they keep `delete` asking for `--force`); editing after
  an undo to an amended-away commit creates a child of that hidden commit;
  a submodule repo that fails to load makes every snapshotting command fail
  (escape: `--config git.snapshot-submodules=false`); in a colocated repo
  `git status` shows nothing for the moved gitlink.
- Verified on 1.97.1: 1161 CLI tests, 95 lib working-copy tests,
  `--no-default-features` check (only the 3 upstream warnings). NOT measured:
  per-command cost on bat (the gate walks every submodule dir each command).
  Clippy still not installed.
- Build gotcha: rustc ICE "unstable fingerprints for evaluate_obligation" in
  incremental `cargo check` of jj-lib, twice. `rm -rf
  target/debug/incremental/jj_lib-*` fixes it; `cargo clean -p jj-lib` also
  does but deleted 26 GB across all profiles.

### 2026-11-15 — Phase 2, step 2: per-command cost on bat; `jj git submodule rebase`

- Timing on bat (94 populated submodules, 8304 files, tmpfs, dev profile at
  opt-level 3, `jj log -r @`, A/B via `--config git.snapshot-submodules=`):
  with the `nested_maybe_dirty` walk serial, snapshots on cost +36 ms per
  command when nothing changed (52 → 89 ms mean). Running the walk with
  rayon across submodules brings that to roughly +10 ms (min 40-44 ms off vs
  51-53 ms on). The machine was loaded (load avg 5-6) for the second
  measurement, so medians overlap and only the minimums are trustworthy;
  re-measure on a quiet machine before quoting it upstream. Touching every
  submodule dir (forces a full nested snapshot of all 94, serial): ~+50-60
  ms. One dirty submodule (sub commit + superproject snapshot op): not
  distinguishable from clean. Verdict: not killed.
- `jj git submodule rebase <name> --onto/-d <submodule revset>`: rebases
  `<onto>..<gitlink of @>` (the local commits) with `move_commits` in a
  submodule transaction, then moves `@`'s gitlink (shared `set_gitlink` with
  `set`). No local commits → the gitlink just moves (fast-forward). Refuses
  to rebase anything that is an ancestor of a remote bookmark. The submodule
  repo's wc pointer follows the rewrite, so later edits still amend.
- Hard limit found while writing it: only `@`'s gitlink follows a submodule
  rewrite. Earlier superproject commits keep the pre-rebase gitlinks, and
  only the tip submodule commit is ever checked out, so a conflict in an
  earlier local submodule commit could never be resolved. The rebase is
  refused in that case rather than creating it. The real fix is the open
  "merging/rebasing with submodules" design: superproject commits that refer
  to rewritten submodule commits must follow, and diverging gitlinks should
  merge in the submodule. That one probably does belong in the #494 thread.
- `jj status` lists a submodule whose gitlinked commit is conflicted ("has
  conflicts"); rebase warns when its result is conflicted.
- `move_commits` must be followed by `rebase_descendants()` or
  `Transaction::commit` panics ("Descendants have not been rebased").
- By-hand falsifier tests replaced by `test_git_submodule_snapshot` and
  `test_git_submodule_rebase`; no library calls left in the CLI test file.
- Verified on 1.97.1: 1161 CLI tests, 95 lib working-copy tests. Optimized
  bench binary kept in `target/opt` (build: `CARGO_PROFILE_DEV_OPT_LEVEL=3
  CARGO_PROFILE_DEV_DEBUG=0 cargo build -p jj-cli --target-dir target/opt
  -j4`, 9 min cold).

### 2026-11-18 — Phase 2, step 3: `jj git submodule push`

- Falsifier first (by hand, then replaced by the real test): plain `git push`
  of a snapshot commit from the submodule's store is accepted; fetching it
  back gives the same id, now on a remote (delete guard lifts); a fresh clone
  of the superproject populates the submodule at it. Passed, nothing to
  change.
- `jj git submodule push <name> [-b <bookmark>] [--allow-empty-description]`
  pushes `@`'s gitlink commit to `origin` with `git::push_refs` (so the same
  lease check as `jj git push`). No superproject operation.
- Decision: the pushed bookmark becomes a *tracked local bookmark in the
  submodule repo*. That one piece of state answers three questions: which
  bookmark to push next time (local bookmarks at the gitlink; jj moves them
  along rewrites), what may be rewritten (rebase guard is now
  `::untracked_remote_bookmarks()`, mirroring jj's `immutable_heads()`), and
  what must not be clobbered (a remote bookmark with no local one, e.g.
  `main`, only moves forward; no override flag).
- Refuses conflicted commits, and commits without a description unless
  `--allow-empty-description`.
- Gap this exposes: snapshot commits never have a description and nothing
  sets one, so every real push needs the override. Next: `jj git submodule
  describe <name> -m` for the tip (rewrite + move gitlink, same shape as
  rebase; `rewrite_commit` keeps it through later snapshots). Earlier local
  commits stay undescribable for the same reason their conflicts are
  unresolvable (only `@`'s gitlink follows rewrites).
- `working_copy_gitlink()` now shared by rebase and push.
- Verified on 1.97.1: 1162 CLI tests. Upstream main is 322 commits ahead.
