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
    let submodules = git_submodule::read_gitmodules(repo.store(), &commit.tree()).await?;
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
// ponytail: always "origin"; Git prefers the current branch's upstream remote.
fn resolve_url(workspace_command: &WorkspaceCommandHelper, url: &str) -> String {
    let base = fetch_url(workspace_command.repo(), "origin")
        .unwrap_or_else(|| workspace_command.workspace_root().display().to_string());
    git_submodule::resolve_submodule_url(url, &base)
}

pub(crate) async fn populate_submodules(
    ui: &mut Ui,
    workspace_command: &mut WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    let (mut locked_ws, wc_commit) = workspace_command.start_working_copy_mutation().await?;
    let stats = locked_ws
        .locked_wc()
        .populate_submodules()
        .await
        .map_err(|err| internal_error_with_message("Failed to populate submodules", err))?;
    let operation_id = locked_ws.locked_wc().old_operation_id().clone();
    locked_ws.finish(operation_id).await?;
    print_checkout_stats(ui, &stats, &wc_commit)?;
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
    // Empty the submodule directories while their repo can still be read.
    let (mut locked_ws, wc_commit) = workspace_command.start_working_copy_mutation().await?;
    let declared = git_submodule::read_gitmodules(repo.store(), &wc_commit.tree()).await?;
    for name in &args.names {
        if let Some(config) = declared.get(name) {
            locked_ws
                .locked_wc()
                .clear_submodule(&config.path)
                .await
                .map_err(|err| internal_error_with_message("Failed to clear submodule", err))?;
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
        let submodules = git_submodule::read_gitmodules(repo.store(), &tree).await?;
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
/// not cloned, gitlink commit not fetched, or local changes jj doesn't record.
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
    let (mut locked_ws, _)= workspace_command.start_working_copy_mutation().await?;
    let dirty = locked_ws
        .locked_wc()
        .dirty_submodules()
        .await
        .map_err(|err| internal_error_with_message("Failed to inspect submodules", err))?;
    // Dropped unfinished: nothing was changed.
    drop(locked_ws);
    problems.extend(dirty.into_iter().map(|path| {
        (
            path,
            "has local changes, which jj does not record yet".to_owned(),
        )
    }));
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
    // Parsed with the superproject's aliases, resolved in the submodule repo.
    let expression = workspace_command
        .parse_revset(ui, &args.submodule_revision)?
        .expression()
        .clone();
    let id_prefix_context = IdPrefixContext::default();
    let evaluator = RevsetExpressionEvaluator::new(
        sub_repo.as_ref(),
        command.revset_extensions().clone(),
        &id_prefix_context,
        expression,
    );
    let ids: Vec<CommitId> = evaluator
        .evaluate_to_commit_ids()?
        .take(2)
        .try_collect()
        .await?;
    let [sub_commit_id] = ids.as_slice() else {
        return Err(user_error(format!(
            "Revset {:?} must resolve to exactly one commit in submodule {:?}",
            args.submodule_revision.as_ref(),
            args.name
        )));
    };

    let mut tx = workspace_command.start_transaction();
    let mut tree_builder = MergedTreeBuilder::new(commit.tree());
    tree_builder.set_or_remove(
        config.path.clone(),
        Some(TreeValue::GitSubmodule(sub_commit_id.clone())),
    );
    let new_tree = tree_builder.write_tree().await?;
    tx.repo_mut()
        .rewrite_commit(&commit)
        .set_tree(new_tree)
        .write()
        .await?;
    tx.finish(
        ui,
        format!(
            "set submodule {} to {} in commit {}",
            args.name,
            sub_commit_id.hex(),
            commit.id().hex()
        ),
    )
    .await
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
