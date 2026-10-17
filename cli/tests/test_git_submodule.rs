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
    let output = work_dir.run_jj(["op", "restore", "@-"]).normalize_stderr_with(hide_ids);
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
    Warning: Submodule at sub: it has local changes, which jj cannot snapshot yet; the directory was left as is
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
    let output = work_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    M sub
    Submodules:
      sub: has local changes, which jj does not record yet
    Working copy  (@) : 
    Parent commit (@-): Add submodule
    [EOF]
    ");
    // That snapshot is stable: later commands don't snapshot again.
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
    Warning: There are unresolved conflicts at these paths:
    sub    2-sided conflict including 1 deletion and a git submodule
    New conflicts appeared in 1 commits:
      
    Hint: To resolve the conflicts, start by creating a commit on top of
    the conflicted commit:
      jj new nmzmmopx
    Then use `jj resolve`, or edit the conflict markers in the file directly.
    Once the conflicts are resolved, you can inspect the result with `jj diff`.
    Then run `jj squash` to move the resolution into the conflicted commit.
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
    Added 0 files, modified 1 files, removed 0 files
    Warning: 1 of those updates were skipped because there were conflicting changes in the working copy.
    Hint: Inspect the changes compared to the intended target with `jj diff --from [id]`.
    Discard the conflicting changes with `jj restore --from [id]`.
    Warning: Submodule at sub: not in .gitmodules; its populated files were left in place
    Warning: There are unresolved conflicts at these paths:
    sub    2-sided conflict including 1 deletion and a git submodule
    New conflicts appeared in 1 commits:
      
    Hint: To resolve the conflicts, start by creating a commit on top of
    the conflicted commit:
      jj new nmzmmopx
    Then use `jj resolve`, or edit the conflict markers in the file directly.
    Once the conflicts are resolved, you can inspect the result with `jj diff`.
    Then run `jj squash` to move the resolution into the conflicted commit.
    [EOF]
    ");
    assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "local edit\n");
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
