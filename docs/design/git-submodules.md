# Git submodules

This is an aspirational document that describes how jj _will_ support Git
submodules. Readers are assumed to have some familiarity with Git and Git
submodules.

This document is a work in progress; submodules are a big feature, and relevant
details will be filled in incrementally.

## Objective

This proposal aims to replicate the workflows users are used to with Git
submodules, e.g.:

- Cloning submodules
- Making new submodule commits and updating the superproject
- Fetching and pushing updates to the submodule's remote
- Viewing submodule history

When it is convenient, this proposal will also aim to make submodules easier to
use than Git's implementation.

### Non-goals

- Non-Git 'submodules' (e.g. native jj submodules, other VCSes)
- Non-Git backends (e.g. Google internal backend)
- Changing how Git submodules are implemented in Git

## Background

We mainly want to support Git submodules for feature parity, since Git
submodules are a standard feature in Git and are popular enough that we have
received user requests for them. Secondarily (and distantly so), Git submodules
are notoriously difficult to use, so there is an opportunity to improve the UX
over Git's implementation.

### Intro to Git Submodules

[Git submodules](https://git-scm.com/docs/gitsubmodules) are a feature of Git
that allow a repository (submodule) to be embedded inside another repository
(the superproject). Notably, a submodule is a full repository, complete with its
own index, object store and ref store. It can be interacted with like any other
repository, regardless of the superproject.

In a superproject commit, submodule information is captured in two places:

- A `gitlink` entry in the commit's tree, where the value of the `gitlink` entry
  is the submodule commit id. This tells Git what to populate in the working
  tree.

- A top level `.gitmodules` file. This file is in Git's config syntax and
  entries take the form `submodule.<submodule-name>.*`. These include many
  settings about the submodules, but most importantly:

  - `submodule<submodule-name>.path` contains the path from the root of the tree
    to the `gitlink` being described.

  - `submodule<submodule-name>.url` contains the url to clone the submodule
    from.

In the working tree, Git notices the presence of a submodule by the `.git` entry
(signifying the root of a Git repository working tree). This is either the
submodule's actual Git directory (an "old-form" submodule), or a `.git` file
pointing to `<superproject-git-directory>/modules/<submodule-name>`. The latter
is sometimes called the "absorbed form", and is Git's preferred mode of
operation.

## Roadmap

Git submodules should be implemented in an order that supports an increasing set
of workflows, with the goal of getting feedback early and often. When support is
incomplete, jj should not crash, but instead provide fallback behavior and warn
the user where needed.

The goal is to land good support for pure Jujutsu workspaces, while colocated
workspaces will be supported when convenient.

This section should be treated as a set of guidelines, not a strict order of
work.

### Phase 1: Readonly submodules

This includes work that inspects submodule contents but does not create new
objects in the submodule. This requires a way to store submodules in a jj
repository that supports readonly operations.

#### Outcomes

- Submodules can be cloned anew
- New submodule commits can be fetched
- Submodule history and branches can be viewed
- Submodule contents are populated in the working copy
- Superproject gitlink can be updated to an existing submodule commit
- Conflicts in the superproject gitlink can be resolved to an existing submodule
  commit

### Phase 2: Snapshotting new changes

This allows a user to write new contents to a submodule and its remote.

#### Outcomes

- Changes in the working copy can be recorded in a submodule commit
- Submodule branches can be modified
- Submodules and their branches can be pushed to their remote

### Phase 3: Merging/rebasing/conflicts

This allows merging and rebasing of superproject commits in a content-aware way
(in contrast to Git, where only the gitlink commit ids are compared), as well as
workflows that make resolving conflicts easy and sensible.

This can be done in tandem with Phase 2, but will likely require a significant
amount of design work on its own.

#### Outcomes

- Merged/rebased submodules result in merged/rebased working copy content
- Merged/rebased working copy content can be committed, possibly by creating
  sensible merged/rebased submodule commits
- Merge/rebase between submodule and non-submodule gives a sensible result
- Merge/rebase between submodule A and submodule B gives a sensible result

### Phase ?: An ideal world

I.e. outcomes we would like to see if there were no constraints whatsoever.

- Rewriting submodule commits rewrites descendants correctly and updates
  superproject gitlinks.
- Submodule conflicts automatically resolve to the 'correct' submodule commits,
  e.g. a merge between superproject commits creating a merge of the submodule
  commits.
- Nested submodules are as easy to work with as non-nested submodules.
- The operation log captures changes in the submodule.

## Design

### Guiding principles

- Submodules are not standalone repositories: every interaction goes through
  the superproject, and `jj` is never run inside a submodule directory.
- Commands involve submodules by default: checking out a superproject commit
  populates the submodule directories that can be populated.
- The submodule store is the source of truth and the working copy is a hint:
  a submodule whose repo or commit is missing is an empty directory plus a
  warning, never an error.

### Storing submodules

Decided per [./git-submodule-storage.md](./git-submodule-storage.md) and
implemented in Phase 1:

- Each submodule is a full jj repo (internal Git backend, own operation log,
  no workspace) at `.jj/repo/submodule_store/<name>`, where `<name>` is the
  `.gitmodules` name percent-encoded so it can never escape the store.
- `.gitmodules` in the tree is authoritative for name ↔ path ↔ url and is
  parsed with gix. Gitlinks without a `.gitmodules` entry stay empty
  directories.
- The submodule's remote is always `origin`; fetches import all branches and
  tags and never abandon unreachable commits, since the superproject may point
  at commits on no branch.
- The superproject's working copy owns submodule files. Each populated
  submodule has its own tree state under
  `.jj/working_copy/submodules/<encoded path>`, backed by the submodule
  repo's store and rooted at the submodule directory. On checkout, submodules
  that go away are cleared *before* the superproject update (so their files
  are never snapshotted as new superproject files), and the ones in the new
  tree are populated after it. The superproject tree state never descends
  into submodule directories.
- A populated submodule with local changes that aren't snapshotted (see
  below; detected by a throwaway snapshot against the submodule's own store,
  honoring its `.gitignore`s) is never clobbered. Its directory is left alone with a warning, and the gitlink it
  currently corresponds to is kept in the tree that gets checked out, so the
  working-copy commit shows the submodule as modified relative to its parent,
  the way `git status` reports a dirty submodule. Once the changes are undone
  the submodule is managed again.
- `jj git clone` clones the submodules of the checked-out commit and
  `jj git fetch` fetches cloned ones (best effort; `git.clone-submodules`,
  `git.fetch-submodules`). After branches and tags, gitlinked commits that are
  still missing are fetched by hash, which the remote may refuse. Relative
  `.gitmodules` URLs resolve against the superproject's `origin`, and URL
  changes in the working-copy commit's `.gitmodules` are followed on fetch.
- Manual management:
  `jj git submodule
  clone|fetch|list|log|delete|populate|set|rebase|describe|push`
  (hidden while experimental). `set` points a gitlink at a commit already in
  the submodule store, given as a revset parsed in the superproject and
  resolved in the submodule repo; it also resolves a conflicted gitlink.
  `log` shows submodule history with the superproject's templates; everything
  in a submodule counts as immutable, since the superproject's
  `immutable_heads()` names bookmarks the submodule doesn't have.
- `jj status` lists submodules that aren't cloned, whose commit isn't fetched,
  or that have local changes that aren't snapshotted.

### Snapshotting new submodule changes

Implemented (experimental, `git.snapshot-submodules`):

- After the superproject snapshot, each populated submodule whose directory
  may have changed is snapshotted through its own tree state into the
  submodule repo's store. If its tree changed, a commit is written in a
  transaction on the submodule repo and the gitlink in the working-copy tree
  moves to it, so the superproject's "snapshot working copy" operation
  records the new gitlink. This needs nothing new from the operation log or
  the backend: the two repos' operations are not tied together, and undoing
  in the superproject just checks out the older gitlink.
- The submodule repo records the commit as the working-copy commit of the
  superproject's workspace (in its own view). The next snapshot rewrites it
  in place if the gitlink still points at it and no parent of the
  superproject's working-copy commit refers to it; otherwise it creates a
  child. Upstream commits and commits the superproject's history refers to
  are therefore never rewritten.
- A submodule is only snapshotted if its directory was populated from the
  commit the gitlink points at. If the gitlink moved while the directory
  couldn't follow, the changes stay unsnapshotted and protected as before.
- Submodule tree states are saved together with the superproject's, so a
  snapshot whose superproject operation doesn't complete is redone.
- `jj git submodule delete` refuses (without `--force`) to delete a repo with
  commits that are on none of its remotes.
- Not done yet: the user's global ignores, auto-track patterns and
  `snapshot.max-new-file-size` don't apply inside submodules; nothing
  garbage-collects snapshot commits the superproject no longer refers to.

### Merging/rebasing with submodules

Partly implemented: `jj git submodule rebase <name> --onto <revset>` updates
a submodule that has local commits. It rebases the commits in
`<onto>..<gitlink of @>` onto the destination in a transaction on the
submodule repo and points the working-copy commit's gitlink at the result;
with no such commits the gitlink just moves. Commits that are ancestors of a
remote bookmark are never rebased. A conflict in the last commit is
materialized in the submodule's files and resolved by editing them (the next
snapshot rewrites the commit); `jj status` reports the submodule as having
conflicts.

Only the working-copy commit's gitlink follows the rewrite. Earlier
superproject commits keep pointing at the pre-rebase submodule commits, so a
rebase that would leave a conflict in an earlier submodule commit is refused:
nothing could check that commit out to resolve it.

### Pushing submodule commits

`jj git submodule push <name> --bookmark <bookmark>` pushes the commit the
working-copy commit's gitlink points at to a bookmark on the submodule's
`origin`, with the same expected-remote-position check as `jj git push`. The
superproject is not touched. In the submodule repo the bookmark becomes a
tracked local bookmark, which is what "pushed from here" means afterwards:

- it follows the commit through later snapshots and rebases, and
  `jj git submodule push <name>` without `--bookmark` pushes it again;
- `jj git submodule rebase` treats only ancestors of *untracked* remote
  bookmarks as off limits, so pushed work can still be rebased;
- a bookmark that exists on the remote but was never pushed from here (say
  `main`) is only ever moved forward.

Commits with conflicts are refused, and so are commits without a description
unless `--allow-empty-description` is given. Snapshot commits start without
one; `jj git submodule describe <name> -m <message>` sets it on the commit the
working-copy commit's gitlink points at (later snapshots keep it), under the
same rule as snapshots: not if the commit is on an untracked remote bookmark
or a parent of the working-copy commit refers to it. So a submodule commit is
described before the superproject change that contains it is committed.

#### Proposed: superproject commits follow submodule rewrites

Whenever a transaction on a submodule repo rewrites commits (rebase,
describe, a snapshot that amends a commit with descendants), it yields a map
from old to new submodule commit ids. In the same superproject transaction,
every *mutable* superproject commit whose tree has a gitlink at the
submodule's path pointing at an old id gets that gitlink replaced by the new
id.

- This is a substitution, not a rebase. The generic rebase of a descendant
  merges trees, and a three-way merge of gitlinks whose base and both sides
  differ is a conflict (verified by hand: substituting in a parent leaves its
  child with "2-sided conflict including a git submodule" and an emptied
  submodule directory until the child is substituted too). So the rewritten
  commits are reparented with their own trees, in which only the gitlink is
  replaced.
- Immutable superproject commits are left alone; the old submodule commits
  stay in the submodule's store for them and for the operation log.
- With that in place, the rule for what a snapshot may rewrite no longer
  needs the working-copy pointer: a submodule commit is rewritten in place
  unless it is an ancestor of an untracked remote bookmark, isn't in the
  submodule repo's index (fetched by hash), or a parent of the working-copy
  commit has that gitlink. `jj edit <earlier commit>` then lets the
  submodule's files of that commit be edited: the snapshot rewrites its
  submodule commit, the submodule repo rebases the later local commits on
  top, and the later superproject commits are substituted. That is how a
  conflict in, or the description of, an earlier local submodule commit is
  fixed.
- Open: a submodule commit that an *immutable* superproject commit refers to
  should count as immutable too. Scanning all history for gitlinks is too
  slow; treating ancestors of the gitlinks of `immutable_heads()` as
  immutable covers a submodule whose pointer only moves forward.
- Not covered: merging diverging gitlinks during a superproject rebase or
  merge (it stays a gitlink conflict, resolved with `jj git submodule set`).
  Doing that automatically means teaching the tree merge about submodules,
  which is a core change to discuss upstream.
