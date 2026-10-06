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
