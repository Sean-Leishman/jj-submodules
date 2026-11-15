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

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::TryStreamExt as _;

use clap::Subcommand;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::git;
use jj_lib::git::get_git_backend;
use jj_lib::git::get_git_repo;
use jj_lib::git_submodule;
use jj_lib::git_submodule::GitSubmoduleError;
use jj_lib::git_submodule::SubmoduleConfig;
use jj_lib::graph::GraphEdge;
use jj_lib::graph::GraphEdgeType;
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::revset::RevsetExpression;
use jj_lib::rewrite::MoveCommitsLocation;
use jj_lib::rewrite::MoveCommitsTarget;
use jj_lib::rewrite::RebaseOptions;
use jj_lib::rewrite::RebasedCommit;
use jj_lib::rewrite::move_commits;

use crate::cli_util::CommandHelper;
use crate::cli_util::LogContentFormat;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandHelper;
use crate::cli_util::format_template;
use crate::cli_util::print_checkout_stats;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::git_util::GitSubprocessUi;
use crate::git_util::print_git_import_stats_summary;
use crate::graphlog::GraphStyle;
use crate::graphlog::get_graphlog;
use crate::revset_util::RevsetExpressionEvaluator;
use crate::ui::Ui;

/// EXPERIMENTAL: Manage the Git submodules of this repo
///
/// Submodule repos are stored inside the superproject's `.jj` directory and
/// have no working copy of their own. Submodule support is read-only:
/// submodule contents can be populated in the working copy, but changes made
/// inside a submodule directory are not snapshotted.
#[derive(Subcommand, Clone, Debug)]
#[command(hide = true)]
pub enum GitSubmoduleCommand {
    Clone(SubmoduleCloneArgs),
    Delete(SubmoduleDeleteArgs),
    Fetch(SubmoduleFetchArgs),
    List(SubmoduleListArgs),
    Log(SubmoduleLogArgs),
    Populate(SubmodulePopulateArgs),
    PrintGitmodules(SubmodulePrintGitmodulesArgs),
    Rebase(SubmoduleRebaseArgs),
    Set(SubmoduleSetArgs),
}

/// Clone submodules listed in `.gitmodules` into the submodule store
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleCloneArgs {
    /// Names of the submodules to clone
    #[arg(required_unless_present = "all")]
    names: Vec<String>,
    /// Clone every submodule in `.gitmodules` that isn't cloned yet
    #[arg(long, conflicts_with = "names")]
    all: bool,
    /// Read `.gitmodules` from this revision
    #[arg(long, short = 'r', default_value = "@", value_name = "REVSET")]
    revision: RevisionArg,
}

/// Delete submodules from the submodule store
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleDeleteArgs {
    /// Names of the submodules to delete
    #[arg(required = true)]
    names: Vec<String>,
    /// Delete even submodules with commits that are on no remote
    #[arg(long)]
    force: bool,
}

/// Fetch all branches and tags of cloned submodules from their remotes
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleFetchArgs {
    /// Names of the submodules to fetch (default: all cloned submodules)
    names: Vec<String>,
}

/// List submodules in the submodule store
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleListArgs {}

/// Show the commit history of a submodule
///
/// Revsets are parsed with the superproject's aliases and evaluated in the
/// submodule repo, which has remote bookmarks (`main@origin`) but no local
/// ones. Templates are the superproject's `templates.log`.
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleLogArgs {
    /// Name of the submodule
    name: String,
    /// Which submodule revisions to show [default: the commit the working-copy
    /// commit's gitlink points at, the remote bookmarks, and up to 10
    /// generations of their ancestors]
    #[arg(long, short, value_name = "REVSETS")]
    revisions: Option<RevisionArg>,
    /// Limit number of revisions to show
    #[arg(long, short = 'n')]
    limit: Option<usize>,
    /// Render each revision using the given template
    #[arg(long, short = 'T')]
    template: Option<String>,
}

/// Populate submodule directories in the working copy
///
/// Fills the directory of every submodule of the working-copy commit whose
/// repo and commit are in the submodule store. Runs automatically on checkout
/// and after `jj git submodule clone` and `fetch`.
#[derive(clap::Args, Clone, Debug)]
pub struct SubmodulePopulateArgs {}

/// Print the submodules declared in `.gitmodules`
#[derive(clap::Args, Clone, Debug)]
pub struct SubmodulePrintGitmodulesArgs {
    /// Read `.gitmodules` from this revision
    #[arg(long, short = 'r', default_value = "@", value_name = "REVSET")]
    revision: RevisionArg,
}

/// Move a submodule's local commits onto another commit of the submodule
///
/// Rebases the submodule commits that lead up to the working-copy commit's
/// gitlink and aren't ancestors of the destination (typically snapshots of
/// changes made in the submodule's directory) onto the destination, and
/// points the gitlink at the result. Without such commits, the gitlink just
/// moves to the destination. Commits that are on a remote are not rebased.
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleRebaseArgs {
    /// Name of the submodule
    name: String,
    /// Revision in the submodule repo to rebase onto
    #[arg(
        long,
        visible_alias = "destination",
        short,
        visible_short_alias = 'd',
        value_name = "SUBMODULE_REVSET"
    )]
    onto: RevisionArg,
}

/// Point a submodule's gitlink at a commit of the submodule
///
/// Rewrites the superproject commit so the submodule's path refers to the
/// given submodule commit, which must already be in the submodule store. This
/// also resolves a conflicted gitlink.
#[derive(clap::Args, Clone, Debug)]
pub struct SubmoduleSetArgs {
    /// Name of the submodule
    name: String,
    /// Revision in the submodule repo to point at
    #[arg(value_name = "SUBMODULE_REVSET")]
    submodule_revision: RevisionArg,
    /// The superproject revision to rewrite
    #[arg(long, short = 'r', default_value = "@", value_name = "REVSET")]
    revision: RevisionArg,
}

pub async fn cmd_git_submodule(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &GitSubmoduleCommand,
) -> Result<(), CommandError> {
    match subcommand {
        GitSubmoduleCommand::Clone(args) => cmd_submodule_clone(ui, command, args).await,
        GitSubmoduleCommand::Delete(args) => cmd_submodule_delete(ui, command, args).await,
        GitSubmoduleCommand::Fetch(args) => cmd_submodule_fetch(ui, command, args).await,
        GitSubmoduleCommand::List(args) => cmd_submodule_list(ui, command, args).await,
        GitSubmoduleCommand::Log(args) => cmd_submodule_log(ui, command, args).await,
        GitSubmoduleCommand::Populate(args) => cmd_submodule_populate(ui, command, args).await,
        GitSubmoduleCommand::Rebase(args) => cmd_submodule_rebase(ui, command, args).await,
        GitSubmoduleCommand::Set(args) => cmd_submodule_set(ui, command, args).await,
        GitSubmoduleCommand::PrintGitmodules(args) => {
            cmd_submodule_print_gitmodules(ui, command, args).await
        }
    }
}

async fn cmd_submodule_clone(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmoduleCloneArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let commit = workspace_command
        .resolve_single_rev(ui, &args.revision)
        .await?;
    if args.all {
        clone_declared_submodules(ui, &workspace_command, &commit).await?;
    } else {
        let repo = workspace_command.repo().clone();
        let submodules = git_submodule::read_gitmodules(repo.store(), &commit.tree()).await?;
        let gitlinks = git_submodule::submodule_gitlinks(&commit.tree(), &submodules).await?;
        for name in &args.names {
            let Some(config) = submodules.get(name) else {
                return Err(user_error(format!(
                    "Submodule {name:?} is not declared in .gitmodules"
                )));
            };
            clone_one(ui, &workspace_command, config, gitlinks.get(name)).await?;
        }
    }
    populate_submodules(ui, &mut workspace_command).await
}

/// Clones every submodule declared in `commit`'s `.gitmodules` that isn't in
/// the store yet. Best effort: a failed clone is a warning, not an error.
pub(crate) async fn clone_declared_submodules(
    ui: &mut Ui,
    workspace_command: &WorkspaceCommandHelper,
    commit: &Commit,
) -> Result<(), CommandError> {
    let repo = workspace_command.repo().clone();
    let submodules = match git_submodule::read_gitmodules(repo.store(), &commit.tree()).await {
        Ok(submodules) => submodules,
        Err(err) => {
            writeln!(ui.warning_default(), "Not cloning submodules: {err}")?;
            return Ok(());
        }
    };
    let gitlinks = git_submodule::submodule_gitlinks(&commit.tree(), &submodules).await?;
    let cloned = repo.submodule_store().list()?;
    for (name, config) in &submodules {
        if cloned.contains(name) {
            continue;
        }
        if let Err(err) = clone_one(ui, workspace_command, config, gitlinks.get(name)).await {
            writeln!(
                ui.warning_default(),
                "Failed to clone submodule {name:?}: {}",
                err.error
            )?;
        }
    }
    Ok(())
}

async fn clone_one(
    ui: &mut Ui,
    workspace_command: &WorkspaceCommandHelper,
    config: &SubmoduleConfig,
    gitlink: Option<&CommitId>,
) -> Result<(), CommandError> {
    let repo = workspace_command.repo();
    let object_hash = get_git_backend(repo.store())?.git_repo().object_hash();
    let url = resolve_url(workspace_command, &config.url);
    writeln!(
        ui.status(),
        "Cloning submodule {:?} from {url}",
        config.name
    )?;
    let (stats, missing) = git_submodule::clone_submodule(
        repo.submodule_store().as_ref(),
        workspace_command.settings(),
        &config.name,
        &url,
        object_hash,
        gitlink.map(std::slice::from_ref).unwrap_or_default(),
        &mut GitSubprocessUi::new(ui),
    )
    .await?;
    print_git_import_stats_summary(ui, &stats)?;
    warn_unfetched(ui, &config.name, &missing)
}

fn warn_unfetched(ui: &Ui, name: &str, missing: &[CommitId]) -> Result<(), CommandError> {
    for id in missing {
        writeln!(
            ui.warning_default(),
            "Could not fetch commit {} of submodule {name:?}: the remote doesn't have it or \
             refuses to serve it by hash",
            id.hex()
        )?;
    }
    Ok(())
}

fn fetch_url(repo: &ReadonlyRepo, remote: &str) -> Option<String> {
    let git_repo = get_git_repo(repo.store()).ok()?;
    let remote = git_repo.try_find_remote_without_url_rewrite(remote)?.ok()?;
    Some(
        remote
            .url(gix::remote::Direction::Fetch)?
            .to_bstring()
            .to_string(),
    )
}

/// Resolves a `.gitmodules` URL against the superproject's remote.
// ponytail: "origin", else the only remote; Git prefers the current branch's
// upstream remote.
fn resolve_url(workspace_command: &WorkspaceCommandHelper, url: &str) -> String {
    let repo = workspace_command.repo();
    let base = fetch_url(repo, "origin")
        .or_else(|| {
            let git_repo = get_git_repo(repo.store()).ok()?;
            let names = git_repo.remote_names();
            if names.len() != 1 {
                return None;
            }
            fetch_url(repo, &names.iter().next()?.to_string())
        })
        .unwrap_or_else(|| workspace_command.workspace_root().display().to_string());
    git_submodule::resolve_submodule_url(url, &base)
}

pub(crate) async fn populate_submodules(
    ui: &mut Ui,
    workspace_command: &mut WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    // --ignore-working-copy/--at-op: the clone/fetch still happened, the
    // directories fill in on the next checkout.
    if workspace_command.check_working_copy_writable().is_err() {
        return Ok(());
    }
    let (mut locked_ws, wc_commit) = workspace_command.start_working_copy_mutation().await?;
    let mut stats = locked_ws
        .locked_wc()
        .populate_submodules()
        .await
        .map_err(|err| internal_error_with_message("Failed to populate submodules", err))?;
    let operation_id = locked_ws.locked_wc().old_operation_id().clone();
    locked_ws.finish(operation_id).await?;
    // Unlike a checkout, show every submodule that couldn't be populated.
    let skipped = std::mem::take(&mut stats.skipped_submodules);
    print_checkout_stats(ui, &stats, &wc_commit)?;
    for (path, reason) in &skipped {
        writeln!(
            ui.warning_default(),
            "Submodule at {}: {reason}",
            path.as_internal_file_string()
        )?;
    }
    Ok(())
}

async fn cmd_submodule_populate(
    ui: &mut Ui,
    command: &CommandHelper,
    _args: &SubmodulePopulateArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    populate_submodules(ui, &mut workspace_command).await
}

async fn cmd_submodule_delete(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmoduleDeleteArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let repo = workspace_command.repo().clone();
    let store = repo.submodule_store();
    for name in &args.names {
        if store.load_repo(name)?.is_none() {
            return Err(GitSubmoduleError::NotCloned(name.clone()).into());
        }
    }
    // Snapshots of local changes exist nowhere but in the submodule repo.
    if !args.force {
        let local_only = "visible_heads() ~ ::remote_bookmarks() ~ root()";
        for name in &args.names {
            let sub_repo = load_submodule(&repo, name).await?;
            let revision = RevisionArg::from(local_only.to_owned());
            let ids =
                submodule_revset(ui, command, &workspace_command, &sub_repo, &revision).await?;
            if !ids.is_empty() {
                return Err(user_error(format!(
                    "Submodule {name:?} has commits that are on none of its remotes"
                ))
                .hinted(format!(
                    "See them with `jj git submodule log {name} -r '{local_only}'`; `--force` \
                     deletes them."
                )));
            }
        }
    }
    // Empty the submodule directories while their repo can still be read.
    let (mut locked_ws, wc_commit) = workspace_command.start_working_copy_mutation().await?;
    let declared = git_submodule::read_gitmodules(repo.store(), &wc_commit.tree()).await?;
    for name in &args.names {
        if let Some(config) = declared.get(name) {
            let stats = locked_ws
                .locked_wc()
                .clear_submodule(&config.path)
                .await
                .map_err(|err| internal_error_with_message("Failed to clear submodule", err))?;
            if let Some((path, reason)) = stats.skipped_submodules.first() {
                return Err(user_error(format!(
                    "Submodule at {}: {reason}",
                    path.as_internal_file_string()
                ))
                .hinted("Its repo would be deleted with those changes still unrecorded."));
            }
        }
    }
    let operation_id = locked_ws.locked_wc().old_operation_id().clone();
    locked_ws.finish(operation_id).await?;
    for name in &args.names {
        store.delete(name)?;
        writeln!(ui.status(), "Deleted submodule {name:?}")?;
    }
    Ok(())
}

async fn cmd_submodule_fetch(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmoduleFetchArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let store = workspace_command.repo().submodule_store().clone();
    for name in &args.names {
        if store.load_repo(name)?.is_none() {
            return Err(GitSubmoduleError::NotCloned(name.clone()).into());
        }
    }
    let names = if args.names.is_empty() {
        store.list()?
    } else {
        args.names.clone()
    };
    fetch_stored_submodules(ui, &mut workspace_command, &names, &[], false).await
}

/// Fetches the given stored submodules, asking by hash for the gitlinks of the
/// working-copy commit and `also_scan` if branches and tags didn't bring them,
/// then populates. With `best_effort`, a failed fetch is a warning.
pub(crate) async fn fetch_stored_submodules(
    ui: &mut Ui,
    workspace_command: &mut WorkspaceCommandHelper,
    names: &[String],
    also_scan: &[CommitId],
    best_effort: bool,
) -> Result<(), CommandError> {
    if names.is_empty() {
        return Ok(());
    }
    let repo = workspace_command.repo().clone();
    let store = repo.submodule_store();
    let mut gitlinks: HashMap<String, Vec<CommitId>> = HashMap::new();
    // URLs come from the working-copy commit, the first one scanned.
    let mut urls: HashMap<String, String> = HashMap::new();
    for id in workspace_command
        .get_wc_commit_id()
        .into_iter()
        .chain(also_scan)
    {
        let tree = repo.store().get_commit(id)?.tree();
        let submodules = match git_submodule::read_gitmodules(repo.store(), &tree).await {
            Ok(submodules) => submodules,
            Err(err) => {
                writeln!(
                    ui.warning_default(),
                    "Ignoring .gitmodules of commit {}: {err}",
                    id.hex()
                )?;
                continue;
            }
        };
        for config in submodules.values() {
            urls.entry(config.name.clone())
                .or_insert_with(|| resolve_url(workspace_command, &config.url));
        }
        for (name, gitlink) in git_submodule::submodule_gitlinks(&tree, &submodules).await? {
            let ids = gitlinks.entry(name).or_default();
            if !ids.contains(&gitlink) {
                ids.push(gitlink);
            }
        }
    }
    for name in names {
        let Some(loader) = store.load_repo(name)? else {
            writeln!(ui.warning_default(), "Submodule {name:?} is not cloned")?;
            continue;
        };
        let mut sub_repo = loader.load_at_head().await?;
        // Follow URL changes in .gitmodules, like `git submodule sync`.
        if let Some(url) = urls.get(name)
            && fetch_url(&sub_repo, git_submodule::SUBMODULE_REMOTE.as_str()).as_ref() != Some(url)
        {
            writeln!(ui.status(), "Updating URL of submodule {name:?} to {url}")?;
            git::set_remote_urls(
                sub_repo.store(),
                git_submodule::SUBMODULE_REMOTE,
                Some(url),
                None,
            )?;
            // Reload so the gix repo behind the store sees the new URL.
            sub_repo = store
                .load_repo(name)?
                .expect("loaded above")
                .load_at_head()
                .await?;
        }
        writeln!(ui.status(), "Fetching submodule {name:?}")?;
        let result = git_submodule::fetch_submodule(
            &sub_repo,
            workspace_command.settings(),
            gitlinks.get(name).map(Vec::as_slice).unwrap_or_default(),
            &mut GitSubprocessUi::new(ui),
        )
        .await;
        match result {
            Ok((stats, missing)) => {
                print_git_import_stats_summary(ui, &stats)?;
                warn_unfetched(ui, name, &missing)?;
            }
            Err(err) if best_effort => {
                writeln!(
                    ui.warning_default(),
                    "Failed to fetch submodule {name:?}: {err}"
                )?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    populate_submodules(ui, workspace_command).await
}

/// Submodules of `wc_commit` that need attention, as `(path, problem)`:
/// not cloned, gitlink commit not fetched, conflicted, or local changes that
/// aren't snapshotted.
pub(crate) async fn submodule_status(
    workspace_command: &mut WorkspaceCommandHelper,
    wc_commit: &Commit,
) -> Result<Vec<(RepoPathBuf, String)>, CommandError> {
    let repo = workspace_command.repo().clone();
    let tree = wc_commit.tree();
    let Ok(submodules) = git_submodule::read_gitmodules(repo.store(), &tree).await else {
        return Ok(vec![]);
    };
    if submodules.is_empty() {
        return Ok(vec![]);
    }
    let gitlinks = git_submodule::submodule_gitlinks(&tree, &submodules).await?;
    let mut problems = vec![];
    for (name, config) in &submodules {
        let Some(gitlink) = gitlinks.get(name) else {
            continue;
        };
        let problem = match repo.submodule_store().load_repo(name)? {
            None => format!("not cloned; run `jj git submodule clone {name}`"),
            Some(loader) => match loader.store().get_commit(gitlink) {
                Err(_) => format!(
                    "commit {} not fetched; run `jj git submodule fetch {name}`",
                    gitlink.hex()
                ),
                Ok(commit)
                    if commit
                        .tree()
                        .path_value(RepoPath::from_internal_string(".gitmodules").unwrap())
                        .await?
                        .is_present() =>
                {
                    "has submodules of its own, which jj doesn't populate yet".to_owned()
                }
                Ok(commit) if commit.has_conflict() => "has conflicts".to_owned(),
                Ok(_) => continue,
            },
        };
        problems.push((config.path.clone(), problem));
    }
    // ponytail: --ignore-working-copy/--at-op skip the dirty check, which
    // needs the lock; a lock-free read of nested states would cover them.
    if workspace_command.check_working_copy_writable().is_err() {
        problems.sort();
        return Ok(problems);
    }
    let (mut locked_ws, _) = workspace_command.start_working_copy_mutation().await?;
    let dirty = locked_ws
        .locked_wc()
        .dirty_submodules()
        .await
        .map_err(|err| internal_error_with_message("Failed to inspect submodules", err))?;
    // Dropped unfinished: nothing was changed.
    drop(locked_ws);
    problems.extend(
        dirty
            .into_iter()
            .map(|path| (path, "has local changes that aren't snapshotted".to_owned())),
    );
    problems.sort();
    Ok(problems)
}

async fn cmd_submodule_list(
    ui: &mut Ui,
    command: &CommandHelper,
    _args: &SubmoduleListArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let repo = workspace_command.repo();
    let wc_commit = workspace_command
        .get_wc_commit_id()
        .map(|id| repo.store().get_commit(id));
    let declared = match wc_commit {
        Some(commit) => git_submodule::read_gitmodules(repo.store(), &commit?.tree()).await?,
        None => Default::default(),
    };
    for name in repo.submodule_store().list()? {
        match declared.get(&name) {
            Some(config) => writeln!(
                ui.stdout(),
                "{name}: {}",
                config.path.as_internal_file_string()
            )?,
            None => writeln!(
                ui.stdout(),
                "{name}: (not in .gitmodules of the working copy)"
            )?,
        }
    }
    Ok(())
}

async fn cmd_submodule_set(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmoduleSetArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let commit = workspace_command
        .resolve_single_rev(ui, &args.revision)
        .await?;
    workspace_command.check_rewritable([commit.id()]).await?;
    let repo = workspace_command.repo().clone();
    let submodules = git_submodule::read_gitmodules(repo.store(), &commit.tree()).await?;
    let Some(config) = submodules.get(&args.name) else {
        return Err(user_error(format!(
            "Submodule {:?} is not declared in .gitmodules",
            args.name
        )));
    };
    let sub_repo = load_submodule(&repo, &args.name).await?;
    let sub_commit_id = submodule_single_commit(
        ui,
        command,
        &workspace_command,
        &sub_repo,
        &args.name,
        &args.submodule_revision,
    )
    .await?;
    set_gitlink(ui, &mut workspace_command, &commit, config, &sub_commit_id).await
}

async fn cmd_submodule_rebase(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmoduleRebaseArgs,
) -> Result<(), CommandError> {
    let name = &args.name;
    let mut workspace_command = command.workspace_helper(ui).await?;
    let commit = workspace_command
        .resolve_single_rev(ui, &RevisionArg::AT)
        .await?;
    workspace_command.check_rewritable([commit.id()]).await?;
    let repo = workspace_command.repo().clone();
    let submodules = git_submodule::read_gitmodules(repo.store(), &commit.tree()).await?;
    let Some(config) = submodules.get(name) else {
        return Err(user_error(format!(
            "Submodule {name:?} is not declared in .gitmodules"
        )));
    };
    let Some(old_id) = git_submodule::submodule_gitlinks(&commit.tree(), &submodules)
        .await?
        .remove(name)
    else {
        return Err(user_error(format!(
            "Submodule {name:?} has no resolved gitlink in the working-copy commit"
        )));
    };
    let sub_repo = load_submodule(&repo, name).await?;
    let onto_id =
        submodule_single_commit(ui, command, &workspace_command, &sub_repo, name, &args.onto)
            .await?;
    let (old, onto) = (old_id.hex(), onto_id.hex());
    let evaluate = async |revset: String| {
        let revision = RevisionArg::from(revset);
        submodule_revset(ui, command, &workspace_command, &sub_repo, &revision).await
    };
    // Commits fetched by hash are in the store but in no revset.
    if evaluate(format!("present({old})")).await?.is_empty() {
        return Err(user_error(format!(
            "Commit {old} is not in the history of submodule {name:?}"
        ))
        .hinted(format!(
            "`jj git submodule set {name}` can point the gitlink at another commit."
        )));
    }
    let roots = evaluate(format!("roots({onto}..{old})")).await?;
    let new_id = if roots.is_empty() {
        // Nothing local: the old commit is the destination or an ancestor.
        onto_id
    } else {
        if !evaluate(format!("({onto}..{old}) & ::remote_bookmarks()"))
            .await?
            .is_empty()
        {
            return Err(user_error(format!(
                "Commits of submodule {name:?} that are on a remote would be rebased"
            ))
            .hinted(format!(
                "`jj git submodule set {name}` can point the gitlink at another commit."
            )));
        }
        let mut tx = sub_repo.start_transaction();
        let stats = move_commits(
            tx.repo_mut(),
            &MoveCommitsLocation {
                new_parent_ids: vec![onto_id],
                new_child_ids: vec![],
                target: MoveCommitsTarget::Roots(roots),
            },
            &RebaseOptions::default(),
        )
        .await?;
        tx.repo_mut().rebase_descendants().await?;
        let new_id = match stats.rebased_commits.get(&old_id) {
            Some(RebasedCommit::Rewritten(commit)) => commit.id().clone(),
            Some(RebasedCommit::Abandoned { parent_id }) => parent_id.clone(),
            None => old_id.clone(),
        };
        // ponytail: only the tip is checked out, so only its conflicts can be
        // resolved. Lifting this needs the superproject commits that refer
        // to the earlier submodule commits to follow the rewrite.
        let stuck = stats.rebased_commits.values().any(|rebased| match rebased {
            RebasedCommit::Rewritten(commit) => commit.id() != &new_id && commit.has_conflict(),
            RebasedCommit::Abandoned { .. } => false,
        });
        if stuck {
            return Err(user_error(format!(
                "Rebasing would leave conflicts in earlier commits of submodule {name:?}, which \
                 can't be resolved yet"
            )));
        }
        if tx.repo().has_changes() {
            tx.commit(format!("rebase submodule commits onto {onto}"))
                .await?;
        }
        writeln!(
            ui.status(),
            "Rebased {} commits of submodule {name:?}",
            stats.num_rebased_targets + stats.num_rebased_descendants
        )?;
        new_id
    };
    if new_id == old_id {
        writeln!(ui.status(), "Nothing changed.")?;
        return Ok(());
    }
    set_gitlink(ui, &mut workspace_command, &commit, config, &new_id).await?;
    if sub_repo.store().get_commit(&new_id)?.has_conflict() {
        writeln!(
            ui.warning_default(),
            "Submodule {name:?} now has conflicts; resolve them in its files."
        )?;
    }
    Ok(())
}

/// Rewrites `commit` so the gitlink of submodule `config` points at
/// `sub_commit_id`.
async fn set_gitlink(
    ui: &Ui,
    workspace_command: &mut WorkspaceCommandHelper,
    commit: &Commit,
    config: &SubmoduleConfig,
    sub_commit_id: &CommitId,
) -> Result<(), CommandError> {
    let mut tx = workspace_command.start_transaction();
    let mut tree_builder = MergedTreeBuilder::new(commit.tree());
    tree_builder.set_or_remove(
        config.path.clone(),
        Some(TreeValue::GitSubmodule(sub_commit_id.clone())),
    );
    let new_tree = tree_builder.write_tree().await?;
    tx.repo_mut()
        .rewrite_commit(commit)
        .set_tree(new_tree)
        .write()
        .await?;
    tx.finish(
        ui,
        format!(
            "set submodule {} to {} in commit {}",
            config.name,
            sub_commit_id.hex(),
            commit.id().hex()
        ),
    )
    .await
}

/// Evaluates `revision` in the submodule repo. It is parsed with the
/// superproject's aliases; its symbols resolve in the submodule repo, which
/// has remote bookmarks (`main@origin`) but no local ones.
async fn submodule_revset(
    ui: &Ui,
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    sub_repo: &ReadonlyRepo,
    revision: &RevisionArg,
) -> Result<Vec<CommitId>, CommandError> {
    let expression = workspace_command
        .parse_revset(ui, revision)?
        .expression()
        .clone();
    let id_prefix_context = IdPrefixContext::default();
    let evaluator = RevsetExpressionEvaluator::new(
        sub_repo,
        command.revset_extensions().clone(),
        &id_prefix_context,
        expression,
    );
    Ok(evaluator.evaluate_to_commit_ids()?.try_collect().await?)
}

async fn submodule_single_commit(
    ui: &Ui,
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    sub_repo: &ReadonlyRepo,
    name: &str,
    revision: &RevisionArg,
) -> Result<CommitId, CommandError> {
    let ids = submodule_revset(ui, command, workspace_command, sub_repo, revision).await?;
    let Ok([id]) = <[CommitId; 1]>::try_from(ids) else {
        return Err(user_error(format!(
            "Revset {:?} must resolve to exactly one commit in submodule {name:?}",
            revision.as_ref(),
        )));
    };
    Ok(id)
}

async fn load_submodule(
    repo: &ReadonlyRepo,
    name: &str,
) -> Result<Arc<ReadonlyRepo>, CommandError> {
    let Some(loader) = repo.submodule_store().load_repo(name)? else {
        return Err(GitSubmoduleError::NotCloned(name.to_owned()).into());
    };
    Ok(loader.load_at_head().await?)
}

// ponytail: a trimmed copy of `jj log`'s graph loop (no prioritized branches,
// elided nodes, patches or paths); share it with cmd_log if this grows.
async fn cmd_submodule_log(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmoduleLogArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let settings = workspace_command.settings();
    let repo = workspace_command.repo().clone();
    let sub_repo = load_submodule(&repo, &args.name).await?;
    let revision = match &args.revisions {
        Some(revision) => revision.clone(),
        None => {
            let gitlink = match workspace_command.get_wc_commit_id() {
                Some(id) => {
                    let tree = repo.store().get_commit(id)?.tree();
                    let submodules = git_submodule::read_gitmodules(repo.store(), &tree).await?;
                    git_submodule::submodule_gitlinks(&tree, &submodules)
                        .await?
                        .remove(&args.name)
                }
                None => None,
            };
            let heads = match gitlink {
                Some(id) => format!("present({}) | remote_bookmarks()", id.hex()),
                None => "remote_bookmarks()".to_owned(),
            };
            RevisionArg::from(format!("ancestors({heads}, 10)"))
        }
    };
    // Parsed with the superproject's aliases, resolved in the submodule repo.
    let expression = workspace_command
        .parse_revset(ui, &revision)?
        .expression()
        .clone();
    let id_prefix_context = IdPrefixContext::default();
    let evaluator = RevsetExpressionEvaluator::new(
        sub_repo.as_ref(),
        command.revset_extensions().clone(),
        &id_prefix_context,
        expression,
    );
    let revset = evaluator.evaluate()?;

    // Submodules are read-only, and the superproject's immutable_heads()
    // likely names bookmarks the submodule doesn't have.
    let language = workspace_command
        .env()
        .commit_template_language_with_immutable(
            sub_repo.as_ref(),
            &id_prefix_context,
            RevsetExpression::all(),
        );
    let template_string = match &args.template {
        Some(value) => value.clone(),
        None => settings.get_string("templates.log")?,
    };
    let template = workspace_command
        .parse_template(ui, &language, &template_string)?
        .labeled(["log", "commit"]);
    let node_template = workspace_command
        .parse_template(ui, &language, &settings.get_string("templates.log_node")?)?
        .labeled(["log", "commit", "node"]);
    let graph_style = GraphStyle::from_settings(settings)?;
    let with_content_format = LogContentFormat::new(ui, settings)?;

    ui.request_pager();
    let mut formatter = ui.stdout_formatter();
    let mut raw_output = formatter.raw()?;
    let mut graph = get_graphlog(graph_style, raw_output.as_mut());
    let mut stream = revset.stream_graph().take(args.limit.unwrap_or(usize::MAX));
    while let Some((commit_id, edges)) = stream.try_next().await? {
        let edges = edges
            .into_iter()
            .map(|edge| match edge.edge_type {
                GraphEdgeType::Missing => GraphEdge::missing(edge.target),
                GraphEdgeType::Direct => GraphEdge::direct(edge.target),
                GraphEdgeType::Indirect => GraphEdge::indirect(edge.target),
            })
            .collect_vec();
        let commit = sub_repo.store().get_commit_async(&commit_id).await?;
        let mut buffer = vec![];
        let within_graph = with_content_format.sub_width(graph.width(&commit_id, &edges));
        within_graph
            .write(ui.new_formatter(&mut buffer).as_mut(), async |formatter| {
                template.format(&commit, formatter)
            })
            .await?;
        let node_symbol = format_template(ui, &Some(commit), &node_template);
        graph.add_node(
            &commit_id,
            &edges,
            &node_symbol,
            &String::from_utf8_lossy(&buffer),
        )?;
    }
    Ok(())
}

async fn cmd_submodule_print_gitmodules(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubmodulePrintGitmodulesArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let repo = workspace_command.repo();
    let commit = workspace_command
        .resolve_single_rev(ui, &args.revision)
        .await?;
    let submodules = git_submodule::read_gitmodules(repo.store(), &commit.tree()).await?;
    if submodules.is_empty() {
        writeln!(ui.status(), "No submodules!")?;
    }
    for (name, config) in submodules {
        writeln!(
            ui.stdout(),
            "name:{name}\nurl:{}\npath:{}\n",
            config.url,
            config.path.as_internal_file_string()
        )?;
    }
    Ok(())
}
