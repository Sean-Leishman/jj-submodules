# Git submodules: status

Last checked 2026-11-22 at `124f101c8` + the help fix, toolchain 1.97.1.
Decisions and history are in `Notes.md`; the design is in
`docs/design/git-submodules.md`.

How to re-check everything in the "Done" list:

    cargo +1.97.1 insta test -j2 -p jj-cli --test runner -- test_git_submodule

"Test" below means a test in `cli/tests/test_git_submodule.rs` (22 tests, all
passing). "Hand" means I also ran it by hand on 2026-11-22 against real Git
repos, and checked the result with `git` where that applies.

## Done

### Phase 1: read submodules

- [x] `jj git clone` clones submodules, `jj git fetch` fetches them, checkout
      populates their directories. Test `auto_clone_and_fetch`,
      `clone_fetch_delete`; hand.
- [x] `jj git submodule clone|fetch|list|delete|populate|print-gitmodules`.
      Test `clone_fetch_delete`, `print_gitmodules`,
      `delete_dirty_and_ignored_leftovers`.
- [x] Relative URLs, and URLs that change upstream. Test
      `relative_and_changed_url`.
- [x] `jj git submodule set` points a gitlink at another commit and resolves a
      conflicted gitlink. Test `set`.
- [x] `jj git submodule log`. Test `log`; hand.
- [x] `jj status` lists submodules that need attention. Test
      `type_changes_and_missing_dir`; hand.
- [x] Unsnapshotted changes in a submodule directory are never overwritten
      (with `git.snapshot-submodules = false`). Test
      `delete_dirty_and_ignored_leftovers`, `random_sequences`; hand.
- [x] Odd cases: unfetchable gitlink, file/dir/submodule type changes, bad
      `.gitmodules`, sparse checkouts, offline, corrupt state, undo, several
      workspaces. Tests `unfetchable_gitlink`, `type_changes_and_missing_dir`,
      `bad_gitmodules`, `sparse_two_submodules`, `failure_injection`,
      `undo_workspaces_and_rewrites`.

### Phase 2: write submodules

- [x] Editing files in a submodule directory is snapshotted into a submodule
      commit, and `@` points at it. Git can read the result. Test `snapshot`;
      hand (pushed, then `git clone --recurse-submodules` showed the edits).
- [x] `jj undo` / `jj redo` move the directory back and forth. Test
      `snapshot`; hand.
- [x] `jj git submodule rebase <name> --onto <rev>` moves local submodule
      commits onto fetched upstream ones. Test `rebase`; hand.
- [x] `jj git submodule describe <name> -m`. Test `push`; hand.
- [x] `jj git submodule push <name> [-b bookmark]`; refuses conflicted commits
      and empty descriptions. Test `push`; hand.
- [x] Every mutable superproject commit follows a submodule rewrite, not only
      `@`. Test `follow_rewrites`.
- [x] Only submodule commits made in this repo are rewritten (read from the
      submodule's operation log). Test `follow_rewrites`, `push`.
- [x] A submodule commit that immutable superproject history depends on is not
      rewritten: snapshots add a child, `rebase` and `describe` refuse. Test
      `immutable_references`; hand.
- [x] `jj edit <earlier>` then editing submodule files amends that commit's
      submodule commit; later commits follow in both repos. Test
      `edit_earlier`; hand.
- [x] A conflict in an earlier commit's submodule commit is resolved by
      `jj edit`-ing that commit and fixing the files. Test `rebase`.
- [x] A failed superproject snapshot leaves the submodule repo untouched
      (submodule operation is published second). Covered only indirectly by
      `random_sequences_snapshotted`; no test kills the process in between.

## To do

Ordered by how much it would bother a user.

### Wrong or misleading today

- [ ] `jj git submodule log` draws every commit except the checked-out one as
      immutable (`◆`), including local unpushed ones. Leftover from Phase 1
      (`submodule.rs`, "Submodules are read-only"). Seen by hand. Fix: mark
      only commits not made here, or protected.
- [ ] After undo-then-edit the submodule repo has two commits with the same
      change id (`(divergent)` in `submodule log`). Seen by hand. Nothing
      depends on submodule change ids, so it is cosmetic. Fix: fresh change id
      when rewriting a hidden commit.
- [ ] A submodule left dirty while snapshotting was off stays "has local
      changes that aren't snapshotted" after turning it back on, until you
      check out the commit the directory was populated from. Seen by hand.
      Needs at least a hint in `jj status`.
- [ ] `jj status` reports a conflicted submodule commit only for `@`. Earlier
      ones are mentioned once, by the `rebase` warning.

### Missing

- [ ] Diverging gitlinks in a superproject rebase or merge are a plain gitlink
      conflict, resolved with `jj git submodule set`. Merging the submodule
      commits is Phase 3 and the open question for jj-vcs/jj#494.
- [ ] Nested submodules (a submodule inside a submodule) are not populated;
      there is a warning. Test `nested` pins this.
- [ ] `jj file track`, `jj file untrack`, `jj run` and `jj workspace remove`
      snapshot without snapshotting submodules. Checked in the code: only
      `cli_util.rs` calls `snapshot_submodules()`.
- [ ] Global ignores, auto-track and `max-new-file-size` don't apply inside
      submodules. From notes, not rechecked.
- [ ] Snapshot commits the superproject dropped are never garbage collected,
      and keep `submodule delete` asking for `--force`. From notes.
- [ ] `jj git submodule log --no-graph`.
- [ ] A submodule pointer that moved sideways (not forward) in immutable
      history is not protected from rewrites.
- [ ] A submodule at a different path in older commits is missed when
      following rewrites.
- [ ] In a colocated repo `git status` shows nothing for a moved gitlink.
      From notes, not rechecked.

### Performance

- [ ] Re-measure per-command cost on bat (94 submodules). Last number, before
      steps C-E: +36 ms with nothing changed.
- [ ] The first edit on top of an upstream commit walks the submodule's whole
      operation log once.

### Before sending anything upstream

- [ ] Rebase onto upstream `main`: 373 commits behind as of 2026-11-22.
- [ ] `cargo clippy` has never run on this work (not installed for 1.97.1).
- [ ] Redraft and post the comment on jj-vcs/jj#494 (needs Sean's go-ahead).
- [ ] Raise the deviation from `git-submodule-storage.md`: the nested working
      copy reads the submodule store directly.
- [ ] Turn Dependabot off on the fork; it keeps merging into `fork/main`.
