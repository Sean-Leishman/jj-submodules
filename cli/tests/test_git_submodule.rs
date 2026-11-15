// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

/// Hides op and commit ids, which vary with the temp dir in `.gitmodules`.
fn hide_ids(s: String) -> String {
    regex::Regex::new(r"\b[0-9a-f]{12,40}\b")
        .unwrap()
        .replace_all(&s, "[id]")
        .into_owned()
}

/// Creates `submodule` (a colocated repo with one file) and `repo`, a
/// colocated superproject with `submodule` added at `sub` via `git submodule
/// add` and committed. Returns the superproject work dir.
fn set_up_superproject(test_env: &TestEnvironment) -> TestWorkDir<'_> {
    // The superproject tree embeds the temp dir (in .gitmodules) and a random
    // change id (in the gitlink), so commit hashes vary per run. Hide them.
    test_env.add_config(r#"templates.commit_summary = 'description.first_line()'"#);
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "submodule"])
        .success();
    let submodule_dir = test_env.work_dir("submodule");
    submodule_dir.write_file("sub", "sub\n");
    submodule_dir
        .run_jj(["commit", "-m", "Submodule commit"])
        .success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "main"])
        .success();

    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "repo"])
        .success();
    let work_dir = test_env.work_dir("repo");
    work_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "git",
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &format!("{}/submodule", test_env.env_root().display()),
            "sub",
        ])
        .success();
    work_dir
        .run_jj_with(|cmd| {
            // Fixed dates keep the Git-made commit's hash stable in snapshots.
            cmd.env("GIT_AUTHOR_DATE", "2001-02-03T04:05:06+07:00")
                .env("GIT_COMMITTER_DATE", "2001-02-03T04:05:06+07:00")
                .args([
                    "util",
                    "exec",
                    "--",
                    "git",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "user.name=Test user",
                    "commit",
                    "-m",
                    "Add submodule",
                ])
        })
        .success();
    // Drop git's own checkout of the submodule; jj manages it from here on.
    std::fs::remove_dir_all(work_dir.root().join("sub")).unwrap();
    work_dir
}

#[test]
fn test_git_submodule_print_gitmodules() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);

    let output = work_dir.run_jj(["git", "submodule", "print-gitmodules"]);
    insta::assert_snapshot!(output, @"
    name:sub
    url:$TEST_ENV/submodule
    path:sub

    [EOF]
    ------- stderr -------
    Done importing changes from the underlying Git repo.
    [EOF]
    ");

    let output = work_dir.run_jj(["git", "submodule", "print-gitmodules", "-r", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    No submodules!
    [EOF]
    ");
}

fn submodule_git_log(work_dir: &TestWorkDir<'_>) -> crate::common::CommandOutput {
    work_dir.run_jj([
        "util",
        "exec",
        "--",
        "git",
        "--git-dir",
        ".jj/repo/submodule_store/sub/store/git",
        "log",
        "--format=%s",
        // Upstream no longer writes refs/remotes/ in non-colocated repos.
        "refs/jj/remote-bookmarks/origin/main",
    ])
}

#[test]
fn test_git_submodule_clone_fetch_delete() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let sub_file = work_dir.root().join("sub").join("sub");

    let output = work_dir.run_jj(["git", "submodule", "list"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Done importing changes from the underlying Git repo.
    [EOF]
    ");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: not cloned; run `jj git submodule clone sub`
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");

    let output = work_dir.run_jj(["git", "submodule", "clone", "nope"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Submodule "nope" is not declared in .gitmodules
    [EOF]
    [exit status: 1]
    "#);

    // Cloning populates the working copy.
    assert!(!sub_file.exists());
    let output = work_dir.run_jj(["git", "submodule", "clone", "--all"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Cloning submodule "sub" from $TEST_ENV/submodule
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(
        work_dir
            .root()
            .join(".jj/repo/submodule_store/sub/store/git")
            .is_dir()
    );
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    insta::assert_snapshot!(submodule_git_log(&work_dir), @"
    Submodule commit
    [EOF]
    ");

    let output = work_dir.run_jj(["git", "submodule", "list"]);
    insta::assert_snapshot!(output, @"
    sub: sub
    [EOF]
    ");

    // Populated files are never snapshotted into the superproject.
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");

    // Cloning again is an error for an explicit name, a no-op for --all.
    let output = work_dir.run_jj(["git", "submodule", "clone", "sub"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Cloning submodule "sub" from $TEST_ENV/submodule
    Error: Submodule "sub" already exists in the submodule store
    [EOF]
    [exit status: 1]
    "#);
    let output = work_dir.run_jj(["git", "submodule", "clone", "--all"]);
    insta::assert_snapshot!(output, @"");

    // Moving to a commit without the submodule removes the populated files
    // (and the now-empty directory); coming back repopulates them.
    let output = work_dir.run_jj(["new", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 3 files
    [EOF]
    ");
    assert!(!work_dir.root().join("sub").exists());
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
    let output = work_dir.run_jj(["new", "master"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");

    // Local changes inside a submodule are never clobbered: the directory is
    // kept (with a warning) when leaving, when the gitlink would move, and
    // its files never leak into the superproject's snapshot.
    std::fs::write(&sub_file, "local edit\n").unwrap();
    work_dir.write_file("sub/untracked", "untracked\n");
    let output = work_dir.run_jj(["new", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 1 files
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
    // Like `git add -A`, the snapshot records the submodule's checked-out
    // commit as the gitlink, but never its files.
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    A sub
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
    let output = work_dir.run_jj(["new", "master"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    // Read-only status must not need the working-copy lock.
    let output = work_dir.run_jj(["status", "--ignore-working-copy"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    // Undo the local changes; the submodule is managed again.
    std::fs::write(&sub_file, "sub\n").unwrap();
    std::fs::remove_file(work_dir.root().join("sub/untracked")).unwrap();
    let output = work_dir.run_jj(["new", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 3 files
    [EOF]
    ");
    assert!(!work_dir.root().join("sub").exists());
    let output = work_dir.run_jj(["new", "master"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    ");

    // A new commit in the submodule's remote is picked up by fetch.
    let submodule_dir = test_env.work_dir("submodule");
    submodule_dir.write_file("sub2", "sub2\n");
    submodule_dir
        .run_jj(["commit", "-m", "Second submodule commit"])
        .success();
    submodule_dir
        .run_jj(["bookmark", "set", "-r", "@-", "main"])
        .success();
    let output = work_dir.run_jj(["git", "submodule", "fetch"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching submodule "sub"
    [EOF]
    "#);
    insta::assert_snapshot!(submodule_git_log(&work_dir), @"
    Second submodule commit
    Submodule commit
    [EOF]
    ");

    // Deleting clears the populated files first.
    let output = work_dir.run_jj(["git", "submodule", "delete", "sub"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Deleted submodule "sub"
    [EOF]
    "#);
    assert!(!sub_file.exists());
    let output = work_dir.run_jj(["git", "submodule", "delete", "sub"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Submodule "sub" is not in the submodule store
    [EOF]
    [exit status: 1]
    "#);
    let output = work_dir.run_jj(["git", "submodule", "list"]);
    insta::assert_snapshot!(output, @"");

    // Without the submodule in the store, checkout warns and leaves the
    // directory empty.
    let output = work_dir.run_jj(["new", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 2 files
    [EOF]
    ");
    let output = work_dir.run_jj(["new", "master"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 2 files, modified 0 files, removed 0 files
    Warning: Submodule at sub: submodule "sub" is not cloned; run `jj git submodule clone sub`
    [EOF]
    "#);
    assert!(work_dir.root().join("sub").is_dir());
    assert!(!sub_file.exists());
    let output = work_dir.run_jj(["git", "submodule", "populate"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Warning: Submodule at sub: submodule "sub" is not cloned; run `jj git submodule clone sub`
    [EOF]
    "#);
}

#[test]
fn test_git_submodule_auto_clone_and_fetch() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    // Let the file:// remote serve commits that are on no branch.
    submodule_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "git",
            "config",
            "uploadpack.allowAnySHA1InWant",
            "true",
        ])
        .success();

    // `jj git clone` clones and populates the submodules by itself.
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            &format!("{}/repo", test_env.env_root().display()),
            "clone",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone"
    bookmark: master@origin [new] tracked
    Setting the revset alias `trunk()` to `master@origin`.
    Cloning submodule "sub" from $TEST_ENV/submodule
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    let clone_dir = test_env.work_dir("clone");
    assert_eq!(
        std::fs::read_to_string(clone_dir.root().join("sub/sub")).unwrap(),
        "sub\n"
    );
    let output = clone_dir.run_jj(["git", "submodule", "list"]);
    insta::assert_snapshot!(output, @"
    sub: sub
    [EOF]
    ");

    // Point the superproject at a submodule commit that is on no branch.
    submodule_dir.write_file("detached", "detached\n");
    submodule_dir
        .run_jj(["commit", "-m", "Detached commit"])
        .success();
    let detached_id = submodule_dir
        .run_jj(["log", "-r", "@-", "--no-graph", "-T", "commit_id"])
        .success()
        .stdout
        .into_raw();
    work_dir
        .run_jj_with(|cmd| {
            cmd.env("GIT_AUTHOR_DATE", "2001-02-03T04:05:06+07:00")
                .env("GIT_COMMITTER_DATE", "2001-02-03T04:05:06+07:00")
                .args([
                    "util",
                    "exec",
                    "--",
                    "sh",
                    "-c",
                    &format!(
                        "git update-index --add --cacheinfo 160000,{detached_id},sub && git -c \
                         user.email=test@example.com -c user.name='Test user' commit -m 'Bump \
                         submodule'"
                    ),
                ])
        })
        .success();

    // `jj git fetch` fetches the superproject, then the submodule, including
    // the new gitlink commit by hash, and populates it.
    let output = clone_dir.run_jj(["git", "fetch"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching from Git remotes: origin
    bookmark: master@origin [updated] tracked
    Fetching submodule "sub"
    [EOF]
    "#);
    let output = clone_dir.run_jj(["new", "master@origin"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Bump submodule
    Added 1 files, modified 1 files, removed 0 files
    [EOF]
    ");
    // Fetching must not need the working copy.
    let output = clone_dir.run_jj(["git", "fetch", "--ignore-working-copy"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching from Git remotes: origin
    Nothing changed.
    Fetching submodule "sub"
    [EOF]
    "#);
    let output = clone_dir.run_jj(["git", "submodule", "fetch", "--ignore-working-copy"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching submodule "sub"
    [EOF]
    "#);
    assert_eq!(
        std::fs::read_to_string(clone_dir.root().join("sub/detached")).unwrap(),
        "detached\n"
    );

    // Both can be turned off.
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            "--config=git.clone-submodules=false",
            &format!("{}/repo", test_env.env_root().display()),
            "clone2",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone2"
    bookmark: master@origin [new] tracked
    Setting the revset alias `trunk()` to `master@origin`.
    Working copy  (@) now at: 
    Parent commit (@-)      : Bump submodule
    Added 2 files, modified 0 files, removed 0 files
    Warning: Submodule at sub: submodule "sub" is not cloned; run `jj git submodule clone sub`
    [EOF]
    "#);
    let clone2_dir = test_env.work_dir("clone2");
    assert!(!clone2_dir.root().join("sub/sub").exists());
    let output = clone2_dir.run_jj(["git", "fetch", "--config=git.fetch-submodules=false"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Fetching from Git remotes: origin
    Nothing changed.
    [EOF]
    ");
}

#[test]
fn test_git_submodule_relative_and_changed_url() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let git_commit = |work_dir: &TestWorkDir, script: &str| {
        work_dir
            .run_jj_with(|cmd| {
                cmd.env("GIT_AUTHOR_DATE", "2001-02-03T04:05:06+07:00")
                    .env("GIT_COMMITTER_DATE", "2001-02-03T04:05:06+07:00")
                    .args(["util", "exec", "--", "sh", "-c", script])
            })
            .success();
    };
    let commit = "git -c user.email=test@example.com -c user.name='Test user' commit -qm";
    git_commit(
        &work_dir,
        &format!(
            "git config -f .gitmodules submodule.sub.url ../submodule && git add .gitmodules && {commit} relative"
        ),
    );

    // The relative URL resolves against the superproject's origin.
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            &format!("{}/repo", test_env.env_root().display()),
            "clone",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone"
    bookmark: master@origin [new] tracked
    Setting the revset alias `trunk()` to `master@origin`.
    Cloning submodule "sub" from $TEST_ENV/submodule
    Working copy  (@) now at: 
    Parent commit (@-)      : relative
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    let clone_dir = test_env.work_dir("clone");
    assert!(clone_dir.root().join("sub/sub").exists());
    // Also against a remote not named "origin".
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            "--remote=upstream",
            &format!("{}/repo", test_env.env_root().display()),
            "clone-upstream",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone-upstream"
    bookmark: master@upstream [new] tracked
    Setting the revset alias `trunk()` to `master@upstream`.
    Cloning submodule "sub" from $TEST_ENV/submodule
    Working copy  (@) now at: 
    Parent commit (@-)      : relative
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(
        test_env
            .work_dir("clone-upstream")
            .root()
            .join("sub/sub")
            .exists()
    );

    // The submodule moves; fetch follows the new URL once it's checked out.
    std::fs::rename(
        test_env.env_root().join("submodule"),
        test_env.env_root().join("moved"),
    )
    .unwrap();
    git_commit(
        &work_dir,
        &format!(
            "git config -f .gitmodules submodule.sub.url ../moved && git add .gitmodules && {commit} moved"
        ),
    );
    // Until then, the old URL fails, but only as a warning.
    let output = clone_dir.run_jj(["git", "fetch"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching from Git remotes: origin
    bookmark: master@origin [updated] tracked
    Fetching submodule "sub"
    Warning: Failed to fetch submodule "sub": Could not find repository at '$TEST_ENV/submodule'
    [EOF]
    "#);
    clone_dir.run_jj(["new", "master@origin"]).success();
    let output = clone_dir.run_jj(["git", "submodule", "fetch"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Updating URL of submodule "sub" to $TEST_ENV/moved
    Fetching submodule "sub"
    [EOF]
    "#);
}

#[test]
fn test_git_submodule_set() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    let sub_file = work_dir.root().join("sub").join("sub");

    let output = work_dir.run_jj(["git", "submodule", "set", "sub", "main"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Done importing changes from the underlying Git repo.
    Error: Submodule "sub" is not in the submodule store
    [EOF]
    [exit status: 1]
    "#);

    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();
    submodule_dir.write_file("sub", "next\n");
    submodule_dir.run_jj(["commit", "-m", "Next"]).success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "next"])
        .success();
    submodule_dir.run_jj(["new", "main"]).success();
    submodule_dir.write_file("sub", "alt\n");
    submodule_dir.run_jj(["commit", "-m", "Alt"]).success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "alt"])
        .success();
    work_dir.run_jj(["git", "submodule", "fetch"]).success();

    let output = work_dir.run_jj(["git", "submodule", "set", "nope", "next"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Submodule "nope" is not declared in .gitmodules
    [EOF]
    [exit status: 1]
    "#);
    let output = work_dir.run_jj(["git", "submodule", "set", "sub", "all()"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Revset "all()" must resolve to exactly one commit in submodule "sub"
    [EOF]
    [exit status: 1]
    "#);

    // Moves the gitlink in @ and repopulates the submodule.
    // Submodule repos have no local bookmarks, only remote ones.
    let output = work_dir.run_jj(["git", "submodule", "set", "sub", "next"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Revision `next` doesn't exist
    Hint: Did you mean `next@origin`?
    [EOF]
    [exit status: 1]
    ");
    let output = work_dir.run_jj(["git", "submodule", "set", "sub", "next@origin"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 2 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "next\n");
    let output = work_dir.run_jj(["diff", "--summary"]);
    insta::assert_snapshot!(output, @"
    M sub
    [EOF]
    ");

    // Resolves a conflicted gitlink.
    work_dir
        .run_jj(["bookmark", "create", "-r", "@", "a"])
        .success();
    work_dir.run_jj(["new", "@-"]).success();
    work_dir
        .run_jj(["git", "submodule", "set", "sub", "alt@origin"])
        .success();
    work_dir.run_jj(["new", "a", "@"]).success();
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): 
    Parent commit (@-): 
    Warning: There are unresolved conflicts at these paths:
    sub    2-sided conflict including a git submodule
    [EOF]
    ");
    let output = work_dir.run_jj(["git", "submodule", "set", "sub", "next@origin"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Parent commit (@-)      : 
    Added 1 files, modified 1 files, removed 0 files
    [EOF]
    ");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Working copy  (@) : 
    Parent commit (@-): 
    Parent commit (@-): 
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "next\n");
}

#[test]
fn test_git_submodule_log() {
    let test_env = TestEnvironment::default();
    set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    submodule_dir.write_file("sub", "next\n");
    submodule_dir.run_jj(["commit", "-m", "Next"]).success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "next"])
        .success();
    // A clone sets trunk() to master@origin, which the submodule doesn't have.
    test_env
        .run_jj_in(
            ".",
            [
                "git",
                "clone",
                &format!("{}/repo", test_env.env_root().display()),
                "clone",
            ],
        )
        .success();
    let clone_dir = test_env.work_dir("clone");

    let output = clone_dir.run_jj(["git", "submodule", "log", "sub"]);
    insta::assert_snapshot!(output, @"
    ◆  rlvkpnrz test.user@example.com 2001-02-03 08:05:13 next@origin 798c2409
    │  Next
    ◆  qpvuntsm test.user@example.com 2001-02-03 08:05:08 main@origin faca8698
    │  Submodule commit
    ◆  zzzzzzzz root() 00000000
    [EOF]
    ");
    let output = clone_dir.run_jj([
        "git",
        "submodule",
        "log",
        "sub",
        "-r",
        "::main@origin",
        "-T",
        "description",
    ]);
    insta::assert_snapshot!(output, @"
    ◆  Submodule commit
    ◆
    [EOF]
    ");
    let output = clone_dir.run_jj(["git", "submodule", "log", "nope"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Submodule "nope" is not in the submodule store
    [EOF]
    [exit status: 1]
    "#);
}

#[test]
fn test_git_submodule_undo_workspaces_and_rewrites() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    let sub_file = work_dir.root().join("sub").join("sub");
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();
    submodule_dir.write_file("sub", "next\n");
    submodule_dir.run_jj(["commit", "-m", "Next"]).success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "next"])
        .success();
    work_dir.run_jj(["git", "submodule", "fetch"]).success();
    work_dir.run_jj(["new", "master"]).success();
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    // In the colocated superproject, git sees jj's checkout as clean.
    let output = work_dir.run_jj(["util", "exec", "--", "git", "status", "--porcelain"]);
    insta::assert_snapshot!(output, @"");

    // Undo and op restore move the submodule checkout along with @.
    work_dir
        .run_jj(["git", "submodule", "set", "sub", "next@origin"])
        .success();
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "next\n");
    let output = work_dir.run_jj(["undo"]).normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Undid operation: [id] (2001-02-03 08:05:19) set submodule sub to [id] in commit [id]
    Restored to operation: [id] (2001-02-03 08:05:17) new empty commit
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 2 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    let output = work_dir
        .run_jj(["op", "restore", "@-"])
        .normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Restored to operation: [id] (2001-02-03 08:05:19) set submodule sub to [id] in commit [id]
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 2 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "next\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");

    // A second workspace gets its own populated submodule checkout.
    let output = work_dir.run_jj(["workspace", "add", "../ws2"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Created workspace in "../ws2"
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    let ws2_dir = test_env.work_dir("ws2");
    let output = ws2_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    let ws2_sub_file = ws2_dir.root().join("sub").join("sub");
    insta::assert_snapshot!(
        std::fs::read_to_string(&ws2_sub_file).unwrap_or_else(|err| err.to_string()),
        @"sub");
    // Editing in one workspace's submodule doesn't touch the other's.
    std::fs::write(&sub_file, "local edit\n").unwrap();
    let output = ws2_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");

    // Local submodule changes survive commands that move @ by rewriting.
    let output = work_dir.run_jj(["abandon"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Abandoned 1 commits:
      
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 1 files, removed 0 files
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    // Nothing was recorded, and nothing is on later commands either.
    let op_id = || {
        work_dir
            .run_jj(["op", "log", "-n1", "--no-graph", "-T", "id"])
            .stdout
            .into_raw()
    };
    let before = op_id();
    work_dir.run_jj(["status"]).success();
    assert_eq!(op_id(), before);
    let output = work_dir.run_jj(["rebase", "-r", "@", "-d", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Rebased 1 commits to destination.
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 1 files
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
    // ponytail: the dirty submodule left in place differs from @, so the
    // next command snapshots it and `jj undo` undoes that snapshot, not the
    // rebase; a second undo is needed.
    let output = work_dir.run_jj(["undo"]).normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Undid operation: [id] (2001-02-03 08:05:32) snapshot working copy
    Restored to operation: [id] (2001-02-03 08:05:31) rebase commit [id]
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Warning: Submodule at sub: not in .gitmodules; its populated files were left in place
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
    // Its files never become the superproject's.
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    A sub
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
    let output = work_dir.run_jj(["git", "submodule", "delete", "sub"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Deleted submodule "sub"
    [EOF]
    "#);
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
}

#[test]
fn test_git_submodule_unfetchable_gitlink() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    // The gitlink points at a commit the submodule remote doesn't have (e.g.
    // force-pushed away).
    work_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "sh",
            "-c",
            "git update-index --cacheinfo 160000,1111111111111111111111111111111111111111,sub && \
             git -c user.email=test@example.com -c user.name='Test user' commit -qm 'Bump'",
        ])
        .success();

    // Clone still succeeds; the missing commit is a warning, not an error.
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            &format!("{}/repo", test_env.env_root().display()),
            "clone",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone"
    bookmark: master@origin [new] tracked
    Setting the revset alias `trunk()` to `master@origin`.
    Cloning submodule "sub" from $TEST_ENV/submodule
    Warning: Could not fetch commit 1111111111111111111111111111111111111111 of submodule "sub": the remote doesn't have it or refuses to serve it by hash
    Working copy  (@) now at: 
    Parent commit (@-)      : Bump
    Added 2 files, modified 0 files, removed 0 files
    Warning: Submodule at sub: commit 1111111111111111111111111111111111111111 is not in submodule "sub"; run `jj git submodule fetch sub`
    [EOF]
    "#);
    let clone_dir = test_env.work_dir("clone");
    let output = clone_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: commit 1111111111111111111111111111111111111111 not fetched; run `jj git submodule fetch sub`
    Working copy  (@) : 
    Parent commit (@-): Bump
    [EOF]
    ");
    let output = clone_dir.run_jj(["git", "fetch"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching from Git remotes: origin
    Nothing changed.
    Fetching submodule "sub"
    Warning: Could not fetch commit 1111111111111111111111111111111111111111 of submodule "sub": the remote doesn't have it or refuses to serve it by hash
    Warning: Submodule at sub: commit 1111111111111111111111111111111111111111 is not in submodule "sub"; run `jj git submodule fetch sub`
    [EOF]
    "#);
    let output = clone_dir.run_jj(["git", "submodule", "fetch"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching submodule "sub"
    Warning: Could not fetch commit 1111111111111111111111111111111111111111 of submodule "sub": the remote doesn't have it or refuses to serve it by hash
    Warning: Submodule at sub: commit 1111111111111111111111111111111111111111 is not in submodule "sub"; run `jj git submodule fetch sub`
    [EOF]
    "#);
}

#[test]
fn test_git_submodule_nested() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "inner"])
        .success();
    let inner_dir = test_env.work_dir("inner");
    inner_dir.write_file("inner", "inner\n");
    inner_dir.run_jj(["commit", "-m", "Inner"]).success();
    // Give the submodule a submodule of its own.
    submodule_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "sh",
            "-c",
            &format!(
                "git -c protocol.file.allow=always submodule add -q {}/inner inner && git -c \
                 user.email=test@example.com -c user.name='Test user' commit -qm 'Add inner'",
                test_env.env_root().display()
            ),
        ])
        .success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "nested"])
        .success();
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();

    // The submodule is populated; its own submodule stays empty, with a
    // warning rather than silently.
    let output = work_dir.run_jj(["git", "submodule", "set", "sub", "nested@origin"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 2 files, modified 1 files, removed 0 files
    Warning: Submodule at sub: it has submodules of its own, which jj doesn't populate yet
    [EOF]
    ");
    assert!(work_dir.root().join("sub/.gitmodules").exists());
    assert!(!work_dir.root().join("sub/inner/inner").exists());
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Submodules:
      sub: has submodules of its own, which jj doesn't populate yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
}

#[test]
fn test_git_submodule_type_changes_and_missing_dir() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let sub_file = work_dir.root().join("sub").join("sub");
    // Commits where `sub` is a plain file ("file") and a plain directory
    // ("dir"), made with git plumbing: the working copy ignores whatever is on
    // disk at a submodule's path.
    work_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "sh",
            "-c",
            "set -e
            export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
            gm=$(git rev-parse HEAD:.gitmodules)
            blob=$(echo f | git hash-object -w --stdin)
            tree=$(printf '100644 blob %s\t.gitmodules\n100644 blob %s\tsub\n' $gm $blob | git mktree)
            git update-ref refs/heads/file $(git commit-tree $tree -p HEAD -m file)
            sub=$(printf '100644 blob %s\tplain\n' $blob | git mktree)
            tree=$(printf '100644 blob %s\t.gitmodules\n040000 tree %s\tsub\n' $gm $sub | git mktree)
            git update-ref refs/heads/dir $(git commit-tree $tree -p HEAD -m dir)",
        ])
        .success();
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    let state_dir = work_dir.root().join(".jj/working_copy/submodules");

    // Clean submodule: gitlink -> file -> directory -> gitlink.
    let output = work_dir.run_jj(["new", "file"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : file
    Added 0 files, modified 1 files, removed 1 files
    [EOF]
    ");
    assert_eq!(
        std::fs::read_to_string(work_dir.root().join("sub")).unwrap(),
        "f\n"
    );
    assert!(std::fs::read_dir(&state_dir).unwrap().next().is_none());
    let output = work_dir.run_jj(["new", "dir"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : dir
    Added 1 files, modified 0 files, removed 1 files
    [EOF]
    ");
    assert_eq!(
        std::fs::read_to_string(work_dir.root().join("sub/plain")).unwrap(),
        "f\n"
    );
    let output = work_dir.run_jj(["new", "master"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 2 files, modified 0 files, removed 1 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    assert!(!work_dir.root().join("sub/plain").exists());

    // Dirty submodule: the gitlink is kept in place of the file or directory.
    std::fs::write(&sub_file, "edit\n").unwrap();
    let output = work_dir.run_jj(["new", "file"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : file
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "edit\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): file
    [EOF]
    ");
    let output = work_dir.run_jj(["new", "dir"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : dir
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "edit\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    A sub
    D sub/plain
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): dir
    [EOF]
    ");
    work_dir.run_jj(["new", "master"]).success();
    std::fs::write(&sub_file, "sub\n").unwrap();

    // The submodule directory deleted or replaced by a file is a local
    // change; `populate` restores a deleted one.
    std::fs::remove_dir_all(work_dir.root().join("sub")).unwrap();
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    let output = work_dir.run_jj(["git", "submodule", "populate"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    std::fs::remove_dir_all(work_dir.root().join("sub")).unwrap();
    work_dir.write_file("sub", "f\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    let output = work_dir.run_jj(["git", "submodule", "populate"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Warning: Submodule at sub: its directory could not be created: File exists (os error 17)
    [EOF]
    ");
    std::fs::remove_file(work_dir.root().join("sub")).unwrap();
    let output = work_dir.run_jj(["git", "submodule", "populate"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
}

#[test]
fn test_git_submodule_delete_dirty_and_ignored_leftovers() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    let sub_file = work_dir.root().join("sub").join("sub");
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();
    submodule_dir.write_file(".gitignore", "build/\n");
    submodule_dir
        .run_jj(["commit", "-m", "Ignore build"])
        .success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "ignore"])
        .success();
    work_dir.run_jj(["git", "submodule", "fetch"]).success();
    work_dir
        .run_jj(["git", "submodule", "set", "sub", "ignore@origin"])
        .success();
    work_dir
        .run_jj(["bookmark", "create", "-r", "@", "ign"])
        .success();
    // Ignored files inside the submodule don't make it dirty.
    work_dir.write_file("sub/build/out", "out\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");

    // Deleting the repo of a submodule with local changes is refused.
    std::fs::write(&sub_file, "edit\n").unwrap();
    let output = work_dir.run_jj(["git", "submodule", "delete", "sub"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    Hint: Its repo would be deleted with those changes still unrecorded.
    [EOF]
    [exit status: 1]
    ");
    let output = work_dir.run_jj(["git", "submodule", "list"]);
    insta::assert_snapshot!(output, @"
    sub: sub
    [EOF]
    ");

    // A dirty submodule whose gitlink moves stays put, but the commit keeps
    // the new gitlink: nothing is recorded in the superproject.
    let output = work_dir.run_jj(["new", "master"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 1 files, removed 0 files
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "edit\n");
    let output = work_dir.run_jj(["diff", "--summary"]);
    insta::assert_snapshot!(output, @"");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    work_dir.run_jj(["new", "ign"]).success();
    std::fs::write(&sub_file, "sub\n").unwrap();
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");

    // Leaving the submodule removes its tracked files but not ignored ones;
    // the gitlink is kept so they are never snapshotted as the
    // superproject's.
    let output = work_dir.run_jj(["new", "root()"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 3 files
    Warning: Submodule at sub: it still has ignored files, which jj doesn't delete; the directory was left as is
    [EOF]
    ");
    assert!(!sub_file.exists());
    assert!(work_dir.root().join("sub/build/out").exists());
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    A sub
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
    // Coming back repopulates around them.
    let output = work_dir.run_jj(["new", "ign"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    assert!(work_dir.root().join("sub/build/out").exists());
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
}

#[test]
fn test_git_submodule_bad_gitmodules() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    work_dir.write_file(".gitmodules", "[submodule \"sub\"\n");
    work_dir
        .run_jj(["commit", "-m", "Break .gitmodules"])
        .success();
    work_dir
        .run_jj(["bookmark", "set", "master", "-r", "@-"])
        .success();
    // Automatic clone and fetch warn and go on.
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            &format!("{}/repo", test_env.env_root().display()),
            "clone",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone"
    bookmark: master@origin [new] tracked
    Setting the revset alias `trunk()` to `master@origin`.
    Warning: Not cloning submodules: Failed to parse .gitmodules
    Working copy  (@) now at: 
    Parent commit (@-)      : Break .gitmodules
    Added 2 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    let output = test_env.work_dir("clone").run_jj(["git", "fetch"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Fetching from Git remotes: origin
    Nothing changed.
    [EOF]
    ");
    let output = test_env.work_dir("clone").run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Break .gitmodules
    [EOF]
    ");
}

#[test]
fn test_git_submodule_sparse_two_submodules() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    work_dir
        .run_jj_with(|cmd| {
            cmd.env("GIT_AUTHOR_DATE", "2001-02-03T04:05:06+07:00")
                .env("GIT_COMMITTER_DATE", "2001-02-03T04:05:06+07:00")
                .args([
                    "util",
                    "exec",
                    "--",
                    "sh",
                    "-c",
                    &format!(
                        "git -c protocol.file.allow=always submodule add -q {}/submodule sub2 && \
                         git -c user.email=test@example.com -c user.name='Test user' commit -qm \
                         'Add sub2'",
                        test_env.env_root().display()
                    ),
                ])
        })
        .success();
    std::fs::remove_dir_all(work_dir.root().join("sub2")).unwrap();
    work_dir
        .run_jj(["git", "submodule", "clone", "--all"])
        .success();
    let sub_file = work_dir.root().join("sub/sub");
    let sub2_file = work_dir.root().join("sub2/sub");
    assert!(sub_file.exists() && sub2_file.exists());

    // Excluding both with one dirty: refused, and the clean one is untouched.
    std::fs::write(&sub2_file, "edit\n").unwrap();
    let output = work_dir.run_jj(["sparse", "set", "--clear", "--add=.gitmodules"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Submodule at sub2 has local changes; commit or discard them first
    [EOF]
    [exit status: 1]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    assert_eq!(std::fs::read_to_string(&sub2_file).unwrap(), "edit\n");
    let output = work_dir.run_jj(["sparse", "list"]);
    insta::assert_snapshot!(output, @"
    .
    [EOF]
    ");

    // Clean again: both are cleared, and reset repopulates them.
    std::fs::write(&sub2_file, "sub\n").unwrap();
    let output = work_dir.run_jj(["sparse", "set", "--clear", "--add=.gitmodules"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Added 0 files, modified 0 files, removed 4 files
    [EOF]
    ");
    assert!(!sub_file.exists() && !sub2_file.exists());
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Add sub2
    [EOF]
    ");
    let output = work_dir.run_jj(["sparse", "reset"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Added 4 files, modified 0 files, removed 0 files
    [EOF]
    ");
    assert!(sub_file.exists() && sub2_file.exists());
}

#[test]
fn test_git_submodule_failure_injection() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let sub_file = work_dir.root().join("sub/sub");
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();

    // The submodule repo vanished from the store.
    let store_dir = work_dir.root().join(".jj/repo/submodule_store/sub");
    assert!(store_dir.is_dir(), "store layout changed");
    std::fs::remove_dir_all(&store_dir).unwrap();
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Submodules:
      sub: not cloned; run `jj git submodule clone sub`
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    let output = work_dir.run_jj(["new", "root()"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    Added 0 files, modified 0 files, removed 1 files
    Warning: Submodule at sub: submodule "sub" is no longer cloned; its populated files were left in place
    [EOF]
    "#);
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    A sub
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
    work_dir.run_jj(["new", "master"]).success();
    let output = work_dir.run_jj(["git", "submodule", "clone", "sub"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Cloning submodule "sub" from $TEST_ENV/submodule
    [EOF]
    "#);
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "sub\n");

    // Corrupt nested state: a clear error, and deleting it recovers.
    let state_file = work_dir
        .root()
        .join(".jj/working_copy/submodules/sub/tree_state");
    assert!(state_file.is_file(), "state layout changed");
    std::fs::write(&state_file, b"garbage").unwrap();
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    let output = work_dir.run_jj(["new"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : 
    [EOF]
    ");
    std::fs::remove_dir_all(state_file.parent().unwrap()).unwrap();
    // Files already on disk are never overwritten (hence "skipped").
    let output = work_dir
        .run_jj(["git", "submodule", "populate"])
        .normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Added 1 files, modified 0 files, removed 0 files
    Warning: 1 of those updates were skipped because there were conflicting changes in the working copy.
    Hint: Inspect the changes compared to the intended target with `jj diff --from [id]`.
    Discard the conflicting changes with `jj restore --from [id]`.
    [EOF]
    ");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");

    // Interrupted populate: a state directory without a tree state.
    std::fs::remove_file(&state_file).unwrap();
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : 
    Parent commit (@-): 
    [EOF]
    ");
    let output = work_dir
        .run_jj(["git", "submodule", "populate"])
        .normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Added 1 files, modified 0 files, removed 0 files
    Warning: 1 of those updates were skipped because there were conflicting changes in the working copy.
    Hint: Inspect the changes compared to the intended target with `jj diff --from [id]`.
    Discard the conflicting changes with `jj restore --from [id]`.
    [EOF]
    ");
    assert!(state_file.is_file());

    // Unreachable submodule URL: automatic clone warns, explicit clone errors.
    work_dir.write_file(
        ".gitmodules",
        "[submodule \"sub\"]\n\tpath = sub\n\turl = /nonexistent/submodule\n",
    );
    work_dir.run_jj(["commit", "-m", "Bad URL"]).success();
    work_dir
        .run_jj(["bookmark", "set", "master", "-r", "@-"])
        .success();
    let output = test_env.run_jj_in(
        ".",
        [
            "git",
            "clone",
            &format!("{}/repo", test_env.env_root().display()),
            "clone",
        ],
    );
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Fetching into new repo in "$TEST_ENV/clone"
    bookmark: master@origin [new] tracked
    Setting the revset alias `trunk()` to `master@origin`.
    Cloning submodule "sub" from /nonexistent/submodule
    Warning: Failed to clone submodule "sub": Could not find repository at '/nonexistent/submodule'
    Working copy  (@) now at: 
    Parent commit (@-)      : Bad URL
    Added 2 files, modified 0 files, removed 0 files
    Warning: Submodule at sub: submodule "sub" is not cloned; run `jj git submodule clone sub`
    [EOF]
    "#);
    let clone_dir = test_env.work_dir("clone");
    let output = clone_dir.run_jj(["git", "submodule", "clone", "sub"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Cloning submodule "sub" from /nonexistent/submodule
    Error: Could not find repository at '/nonexistent/submodule'
    [EOF]
    [exit status: 1]
    "#);
    let output = clone_dir.run_jj(["git", "submodule", "list"]);
    insta::assert_snapshot!(output, @"");
}

/// Random command sequences; after each one, local changes inside the
/// submodule are intact, its files never show up as the superproject's, and
/// no command fails internally.
#[test]
fn test_git_submodule_random_sequences() {
    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    let sub_dir = work_dir.root().join("sub");
    let sub_file = sub_dir.join("sub");
    submodule_dir.write_file("sub", "next\n");
    submodule_dir.run_jj(["commit", "-m", "Next"]).success();
    submodule_dir
        .run_jj(["bookmark", "create", "-r", "@-", "next"])
        .success();
    work_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "sh",
            "-c",
            "set -e
            export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
            gm=$(git rev-parse HEAD:.gitmodules)
            blob=$(echo f | git hash-object -w --stdin)
            tree=$(printf '100644 blob %s\\t.gitmodules\\n100644 blob %s\\tsub\\n' $gm $blob | git mktree)
            git update-ref refs/heads/file $(git commit-tree $tree -p HEAD -m file)",
        ])
        .success();
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();
    work_dir
        .run_jj(["git", "submodule", "set", "sub", "next@origin"])
        .success();
    work_dir
        .run_jj(["bookmark", "create", "-r", "@", "bumped"])
        .success();
    work_dir.run_jj(["new", "master"]).success();

    let targets = ["master", "bumped", "file", "root()"];
    let mut log = String::new();
    for seed in 1..=4u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut edit: Option<String> = None;
        let mut untracked = false;
        for step in 0..16 {
            let op = next() % 8;
            let output = match op {
                0 if sub_dir.is_dir() => {
                    let content = format!("edit {seed} {step}\n");
                    std::fs::write(&sub_file, &content).unwrap();
                    edit = Some(content);
                    log += "edit; ";
                    continue;
                }
                1 if sub_dir.is_dir() => {
                    work_dir.write_file("sub/untracked", "untracked\n");
                    untracked = true;
                    log += "untracked; ";
                    continue;
                }
                2 => {
                    // Discard local changes and repopulate.
                    if sub_dir.is_dir() {
                        std::fs::remove_dir_all(&sub_dir).unwrap();
                    }
                    edit = None;
                    untracked = false;
                    log += "discard; ";
                    work_dir.run_jj(["git", "submodule", "populate"])
                }
                3 | 4 => {
                    let target = targets[(next() % targets.len() as u64) as usize];
                    log += &format!("new {target}; ");
                    work_dir.run_jj(["new", target])
                }
                5 => {
                    log += "undo; ";
                    work_dir.run_jj(["undo"])
                }
                6 => {
                    log += "populate; ";
                    work_dir.run_jj(["git", "submodule", "populate"])
                }
                _ => {
                    let rev = ["main@origin", "next@origin"][(next() % 2) as usize];
                    log += &format!("set {rev}; ");
                    work_dir.run_jj(["git", "submodule", "set", "sub", rev])
                }
            };
            let status = work_dir.run_jj(["status"]);
            let diff = work_dir.run_jj(["diff", "--summary"]);
            let all = format!("{output}{status}{diff}");
            assert!(
                !all.contains("Internal error") && !all.contains("panicked"),
                "seed {seed}, after `{log}`:\n{all}"
            );
            if let Some(content) = &edit {
                assert_eq!(
                    std::fs::read_to_string(&sub_file).unwrap_or_default(),
                    *content,
                    "seed {seed}, local edit lost after `{log}`:\n{all}"
                );
            }
            if untracked {
                assert!(
                    sub_dir.join("untracked").exists(),
                    "seed {seed}, untracked file lost after `{log}`:\n{all}"
                );
            }
            for leaked in ["sub/sub", "sub/untracked"] {
                assert!(
                    !format!("{status}{diff}").contains(leaked),
                    "seed {seed}, {leaked} leaked after `{log}`:\n{all}"
                );
            }
        }
        log.clear();
    }
}

/// Falsifier for snapshotting submodules (Phase 2): does by hand, through the
/// library, what a superproject snapshot would do for a dirty submodule, and
/// checks that checkout, Git and undo cope without any change to the op log
/// or the backend.
#[test]
fn test_git_submodule_snapshot_by_hand() {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::default_backend_factories;
    use jj_lib::local_working_copy::TreeState;
    use jj_lib::local_working_copy::TreeStateSettings;
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::repo::Repo as _;
    use jj_lib::repo::RepoLoader;
    use pollster::FutureExt as _;

    let test_env = TestEnvironment::default();
    let work_dir = set_up_superproject(&test_env);
    let submodule_dir = test_env.work_dir("submodule");
    let sub_dir = work_dir.root().join("sub");
    let read = |name: &str| std::fs::read(sub_dir.join(name)).ok();
    work_dir
        .run_jj(["git", "submodule", "clone", "sub"])
        .success();
    assert_eq!(read("sub").unwrap(), b"sub\n");
    let gitlink = submodule_dir
        .run_jj(["log", "--no-graph", "-r", "main", "-T", "commit_id"])
        .success()
        .stdout
        .into_raw();

    // Snapshots the submodule directory into the submodule repo and commits
    // the tree there: on top of `base`, or rewriting `base` if `amend`.
    let settings = testutils::user_settings();
    let snapshot_submodule = |base: &str, amend: bool| -> String {
        let sub_repo = RepoLoader::init_from_file_system(
            &settings,
            &work_dir.root().join(".jj/repo/submodule_store/sub"),
            &default_backend_factories(),
        )
        .unwrap()
        .load_at_head()
        .block_on()
        .unwrap();
        let mut state = TreeState::load(
            sub_repo.store().clone(),
            sub_dir.clone(),
            work_dir.root().join(".jj/working_copy/submodules/sub"),
            &TreeStateSettings::try_from_user_settings(&settings).unwrap(),
        )
        .unwrap();
        state
            .snapshot(&testutils::empty_snapshot_options())
            .block_on()
            .unwrap();
        state.save().unwrap();
        let tree = state.current_tree().clone();
        let base = sub_repo
            .store()
            .get_commit(&CommitId::try_from_hex(base).unwrap())
            .unwrap();
        let mut tx = sub_repo.start_transaction();
        let builder = if amend {
            tx.repo_mut().rewrite_commit(&base)
        } else {
            tx.repo_mut()
                .new_commit(vec![base.id().clone()], tree.clone())
        };
        let commit = builder.set_tree(tree).write().block_on().unwrap();
        tx.repo_mut().rebase_descendants().block_on().unwrap();
        tx.commit("snapshot submodule working copy")
            .block_on()
            .unwrap();
        commit.id().hex()
    };

    // Not text, so any eol or encoding mangling on the way back would show.
    let added = b"\x00bin\r\n\xff";
    std::fs::write(sub_dir.join("sub"), "edited\n").unwrap();
    std::fs::write(sub_dir.join("added"), added).unwrap();
    let first = snapshot_submodule(&gitlink, false);
    let output = work_dir.run_jj(["git", "submodule", "set", "sub", &first]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 1 files, removed 0 files
    [EOF]
    ");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    let change_id = work_dir
        .run_jj(["log", "--no-graph", "-r", "@", "-T", "change_id"])
        .success()
        .stdout
        .into_raw();

    // 1. Round trip: away and back reproduces the submodule files, and the
    // superproject commit holds nothing but the moved gitlink.
    let output = work_dir.run_jj(["new", "@-"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 2 files, removed 1 files
    [EOF]
    ");
    assert_eq!(read("sub").unwrap(), b"sub\n");
    assert_eq!(read("added"), None);
    let output = work_dir.run_jj(["edit", &change_id]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 1 files, modified 2 files, removed 0 files
    [EOF]
    ");
    assert_eq!(read("sub").unwrap(), b"edited\n");
    assert_eq!(read("added").unwrap(), added);
    let output = work_dir.run_jj(["diff", "--summary", "-r", &change_id]);
    insta::assert_snapshot!(output, @"
    M sub
    [EOF]
    ");

    // 2. Git can read the commit (it has to be pushable eventually).
    let output = work_dir
        .run_jj([
            "util",
            "exec",
            "--",
            "git",
            "--git-dir",
            ".jj/repo/submodule_store/sub/store/git",
            "cat-file",
            "-p",
            &first,
        ])
        .success();
    assert!(
        output.stdout.raw().contains(&format!("parent {gitlink}")),
        "{output}"
    );

    // 3. Undo: the gitlink moves back and the files follow, without a clobber
    // warning; nothing is lost, since the commit stays in the submodule repo.
    // Done after a second snapshot, which amends the first like jj amends @,
    // so undo has to check out a commit the submodule repo no longer shows.
    std::fs::write(sub_dir.join("sub"), "edited twice\n").unwrap();
    std::fs::remove_file(sub_dir.join("added")).unwrap();
    let second = snapshot_submodule(&first, true);
    work_dir
        .run_jj(["git", "submodule", "set", "sub", &second])
        .success();
    let output = work_dir.run_jj(["undo"]).normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Undid operation: [id] (2001-02-03 08:05:22) set submodule sub to [id] in commit [id]
    Restored to operation: [id] (2001-02-03 08:05:19) edit commit [id]
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 1 files, modified 2 files, removed 0 files
    [EOF]
    ");
    assert_eq!(read("sub").unwrap(), b"edited\n");
    assert_eq!(read("added").unwrap(), added);
    let output = work_dir.run_jj(["redo"]).normalize_stderr_with(hide_ids);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Restored to operation: [id] (2001-02-03 08:05:22) set submodule sub to [id] in commit [id]
    Working copy  (@) now at: 
    Parent commit (@-)      : Add submodule
    Added 0 files, modified 2 files, removed 1 files
    [EOF]
    ");
    assert_eq!(read("sub").unwrap(), b"edited twice\n");
    assert_eq!(read("added"), None);
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
}
