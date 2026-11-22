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

//! Git submodules: reading `.gitmodules` and cloning/fetching submodule repos
//! into the superproject's [`SubmoduleStore`].

#![expect(missing_docs)]

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::pin::pin;
use std::sync::Arc;

use futures::AsyncReadExt as _;
use futures::TryStreamExt as _;
use thiserror::Error;

use crate::backend::BackendError;
use crate::backend::BackendResult;
use crate::backend::CommitId;
use crate::backend::TreeValue;
use crate::commit::Commit;
use crate::config::ConfigGetError;
use crate::file_util::IoResultExt as _;
use crate::file_util::PathError;
use crate::git::GitFetch;
use crate::git::GitFetchError;
use crate::git::GitFetchRefExpression;
use crate::git::GitImportError;
use crate::git::GitImportOptions;
use crate::git::GitImportStats;
use crate::git::GitRefExpansionError;
use crate::git::GitRemoteManagementError;
use crate::git::GitSettings;
use crate::git::GitSubprocessCallback;
use crate::git::RefSpec;
use crate::git::UnexpectedGitBackendError;
use crate::git::add_remote;
use crate::git::get_git_backend;
use crate::git_backend::GitBackend;
use crate::git_subprocess::GitSubprocessContext;
use crate::index::IndexResult;
use crate::merged_tree::MergedTree;
use crate::merged_tree_builder::MergedTreeBuilder;
use crate::object_id::ObjectId as _;
use crate::op_store::OpStoreResult;
use crate::op_walk;
use crate::ref_name::RemoteName;
use crate::ref_name::WorkspaceName;
use crate::repo::BackendInitializer;
use crate::repo::MutableRepo;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo;
use crate::repo::RepoInitError;
use crate::repo::RepoLoaderError;
use crate::repo_path::RepoPath;
use crate::repo_path::RepoPathBuf;
use crate::revset::ResolvedRevsetExpression;
use crate::revset::RevsetExpression;
use crate::revset::RevsetStreamExt as _;
use crate::rewrite::MoveCommitsLocation;
use crate::rewrite::MoveCommitsTarget;
use crate::rewrite::RebaseOptions;
use crate::rewrite::RebasedCommit;
use crate::rewrite::move_commits;
use crate::settings::UserSettings;
use crate::signing_factory::SignInitError;
use crate::signing_factory::signer_from_settings;
use crate::store::Store;
use crate::str_util::StringExpression;
use crate::submodule_store::SubmoduleStore;
use crate::submodule_store::SubmoduleStoreError;
use crate::transaction::TransactionCommitError;
use crate::transaction::UnpublishedOperation;
use crate::working_copy::LockedWorkingCopy;
use crate::working_copy::SnapshotError;
use crate::working_copy::SubmoduleSnapshot;

/// Remote name used inside every submodule repo.
pub const SUBMODULE_REMOTE: &RemoteName = RemoteName::new("origin");

/// One `[submodule "<name>"]` entry from `.gitmodules`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleConfig {
    pub name: String,
    pub path: RepoPathBuf,
    pub url: String,
}

#[derive(Debug, Error)]
pub enum GitSubmoduleError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(".gitmodules is conflicted or not a regular file")]
    GitmodulesNotFile,
    #[error("Failed to parse .gitmodules")]
    InvalidGitmodules(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("Submodule {0:?} already exists in the submodule store")]
    AlreadyExists(String),
    #[error("Submodule {0:?} is not in the submodule store")]
    NotCloned(String),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Config(#[from] ConfigGetError),
    #[error(transparent)]
    Sign(#[from] SignInitError),
    #[error(transparent)]
    RepoInit(#[from] RepoInitError),
    #[error(transparent)]
    RepoLoad(#[from] RepoLoaderError),
    #[error(transparent)]
    Store(#[from] SubmoduleStoreError),
    #[error(transparent)]
    UnexpectedBackend(#[from] UnexpectedGitBackendError),
    #[error(transparent)]
    Remote(#[from] GitRemoteManagementError),
    #[error(transparent)]
    RefExpansion(#[from] GitRefExpansionError),
    #[error(transparent)]
    Fetch(#[from] GitFetchError),
    #[error(transparent)]
    Import(#[from] GitImportError),
    #[error(transparent)]
    Commit(#[from] TransactionCommitError),
}

/// Reads `.gitmodules` at the root of `tree`. Absent file → empty map.
/// Entries without a usable `path` or `url` are skipped.
pub async fn read_gitmodules(
    store: &Store,
    tree: &MergedTree,
) -> Result<BTreeMap<String, SubmoduleConfig>, GitSubmoduleError> {
    let path = RepoPath::from_internal_string(".gitmodules").unwrap();
    let id = match tree.path_value(path).await?.into_resolved() {
        Ok(None) => return Ok(BTreeMap::new()),
        Ok(Some(TreeValue::File { id, .. })) => id,
        _ => return Err(GitSubmoduleError::GitmodulesNotFile),
    };
    let mut bytes = vec![];
    store
        .read_file(path, &id)
        .await?
        .read_to_end(&mut bytes)
        .await
        .map_err(|err| BackendError::ReadFile {
            path: path.to_owned(),
            id: id.clone(),
            source: err.into(),
        })?;
    parse_gitmodules(&bytes)
}

fn parse_gitmodules(bytes: &[u8]) -> Result<BTreeMap<String, SubmoduleConfig>, GitSubmoduleError> {
    let file = gix::submodule::File::from_bytes(bytes, None, &gix::config::File::default())
        .map_err(|err| GitSubmoduleError::InvalidGitmodules(err.into()))?;
    let mut submodules = BTreeMap::new();
    for name in file.names() {
        let (Ok(path), Ok(url)) = (file.path(name), file.url(name)) else {
            continue;
        };
        let Ok(path) = RepoPathBuf::from_internal_string(path.to_string()) else {
            continue;
        };
        let name = name.to_string();
        submodules.insert(
            name.clone(),
            SubmoduleConfig {
                name,
                path,
                url: url.to_bstring().to_string(),
            },
        );
    }
    Ok(submodules)
}

/// The commit each declared submodule is checked out at in `tree`, by name.
/// Submodules whose path holds no gitlink are absent.
pub async fn submodule_gitlinks(
    tree: &MergedTree,
    submodules: &BTreeMap<String, SubmoduleConfig>,
) -> Result<HashMap<String, CommitId>, GitSubmoduleError> {
    let mut gitlinks = HashMap::new();
    for config in submodules.values() {
        if let Some(TreeValue::GitSubmodule(id)) = tree.path_value(&config.path).await?.as_normal()
        {
            gitlinks.insert(config.name.clone(), id.clone());
        }
    }
    Ok(gitlinks)
}

/// Inverts [`read_gitmodules`]: gitlink path → submodule name.
pub fn gitmodules_by_path(
    submodules: &BTreeMap<String, SubmoduleConfig>,
) -> HashMap<RepoPathBuf, String> {
    submodules
        .values()
        .map(|config| (config.path.clone(), config.name.clone()))
        .collect()
}

/// Resolves a `.gitmodules` URL like Git does: `./x` and `../x` are relative
/// to `base`, the superproject's remote URL (or its path if it has no remote).
/// Other URLs are returned unchanged.
pub fn resolve_submodule_url(url: &str, base: &str) -> String {
    if !url.starts_with("./") && !url.starts_with("../") {
        return url.to_owned();
    }
    let mut base = base.trim_end_matches('/');
    let mut rest = url;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            rest = r;
            // Drop the last component; for scp-like `host:path`, keep the ':'.
            base = match base.rfind(['/', ':']) {
                Some(i) if base.as_bytes()[i] == b':' => &base[..=i],
                Some(i) => &base[..i],
                None => "",
            };
        } else {
            break;
        }
    }
    if base.is_empty() || base.ends_with(':') {
        format!("{base}{rest}")
    } else {
        format!("{base}/{rest}")
    }
}

/// Creates a new workspace-less repo for submodule `name` in `store` and
/// fetches it from `url` like [`fetch_submodule`].
pub async fn clone_submodule(
    store: &dyn SubmoduleStore,
    settings: &UserSettings,
    name: &str,
    url: &str,
    object_hash: gix::hash::Kind,
    wanted: &[CommitId],
    callback: &mut dyn GitSubprocessCallback,
) -> Result<(GitImportStats, Vec<CommitId>), GitSubmoduleError> {
    let repo_path = store.repo_path(name);
    if repo_path.exists() {
        return Err(GitSubmoduleError::AlreadyExists(name.to_owned()));
    }
    fs::create_dir_all(&repo_path).context(&repo_path)?;
    let result = async {
        let backend_initializer: &BackendInitializer = &|settings, store_path| {
            Ok(Box::new(GitBackend::init_internal(
                settings,
                store_path,
                object_hash,
            )?))
        };
        let repo = ReadonlyRepo::init(
            settings,
            &repo_path,
            backend_initializer,
            signer_from_settings(settings)?,
            ReadonlyRepo::default_workspace_store_initializer(),
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .await?;
        let mut tx = repo.start_transaction();
        add_remote(tx.repo_mut(), SUBMODULE_REMOTE, url, None)?;
        tx.commit("add git remote origin").await?;
        // Reload so the gix repo behind the store sees the new remote config.
        let repo = store
            .load_repo(name)?
            .expect("just initialized")
            .load_at_head()
            .await?;
        fetch_submodule(&repo, settings, wanted, callback).await
    }
    .await;
    if result.is_err() {
        fs::remove_dir_all(&repo_path).ok();
    }
    result
}

/// Fetches all branches and tags of a submodule repo from its remote, then
/// any of the `wanted` commits (typically the superproject's gitlinks) that
/// are still missing, by hash. A remote may refuse the latter; that is not an
/// error, the commit just stays missing and is returned.
///
/// Unreachable commits are never abandoned: the superproject may reference
/// submodule commits that are on no branch.
pub async fn fetch_submodule(
    repo: &Arc<ReadonlyRepo>,
    settings: &UserSettings,
    wanted: &[CommitId],
    callback: &mut dyn GitSubprocessCallback,
) -> Result<(GitImportStats, Vec<CommitId>), GitSubmoduleError> {
    let git_settings = GitSettings::from_settings(settings)?;
    let import_options = GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    let mut tx = repo.start_transaction();
    let stats = {
        let mut git_fetch = GitFetch::new(
            tx.repo_mut(),
            git_settings.to_subprocess_options(),
            &import_options,
        )?;
        git_fetch.fetch(
            SUBMODULE_REMOTE,
            &GitFetchRefExpression {
                bookmark: StringExpression::all(),
                tag: StringExpression::all(),
            },
            callback,
            None,
        )?;
        git_fetch.import_refs().await?
    };
    tx.commit("fetch from git remote origin").await?;

    let store = repo.store();
    let missing: Vec<&CommitId> = wanted
        .iter()
        .filter(|id| {
            matches!(
                store.get_commit(id),
                Err(BackendError::ObjectNotFound { .. })
            )
        })
        .collect();
    if !missing.is_empty() {
        let git_backend = get_git_backend(store)?;
        let git_ctx = GitSubprocessContext::from_git_backend(
            git_backend,
            git_settings.to_subprocess_options(),
        );
        // Keep the fetched commits alive with a ref import_refs() ignores.
        let refspecs: Vec<RefSpec> = missing
            .iter()
            .map(|id| RefSpec::forced(id.hex(), format!("refs/jj/gitlink/{}", id.hex())))
            .collect();
        for refspec in refspecs {
            // One at a time: git aborts the whole fetch on one refused hash.
            if let Err(err) = git_ctx.spawn_fetch(SUBMODULE_REMOTE, &[refspec], &[], callback, None)
            {
                tracing::info!(?err, "failed to fetch submodule commit by hash");
            }
        }
    }
    let still_missing = missing
        .into_iter()
        .filter(|id| store.get_commit(id).is_err())
        .cloned()
        .collect();
    Ok((stats, still_missing))
}

/// Submodule operations written by [`commit_submodule_snapshots()`], to be published
/// once the superproject operation that refers to their commits is.
#[must_use]
pub struct SubmoduleSnapshotOperations(Vec<UnpublishedOperation>);

impl SubmoduleSnapshotOperations {
    pub async fn publish(self) -> Result<(), TransactionCommitError> {
        for operation in self.0 {
            operation.publish().await?;
        }
        Ok(())
    }
}

/// What [`commit_submodule_snapshots()`] did.
pub struct SubmoduleSnapshotOutcome {
    /// The working-copy tree with the gitlinks moved.
    pub tree: MergedTree,
    /// To be published once the superproject operation is.
    pub operations: SubmoduleSnapshotOperations,
    /// By submodule path, the submodule commits that were replaced and what
    /// replaced them, for [`rebase_descendants_following_submodules()`].
    pub rewrites: HashMap<RepoPathBuf, HashMap<CommitId, CommitId>>,
}

/// Commits the new contents of submodule directories in the submodules' own
/// repos and moves the gitlinks in the working-copy tree to the new commits.
///
/// The commit a gitlink points at is rewritten in place if it was made here
/// (see [`locally_written()`]), no parent of `wc_commit` refers to it, and
/// nothing immutable depends on it; otherwise the new commit becomes its
/// child. Either way, the local submodule commits that descendants of
/// `wc_commit` in `superproject` have on top of it are rebased onto the new
/// commit. The superproject, not the submodule repo, says which commits
/// those are: it may point at commits the submodule repo has hidden.
///
/// The submodule operations are written but not published, so that a
/// superproject snapshot that doesn't complete leaves nothing behind in the
/// submodule repos but unreferenced commits.
///
/// `snapshots` come from [`LockedWorkingCopy::snapshot_submodules()`];
/// `protected` has, by submodule path, the submodule commits that superproject
/// commits which must not change refer to (see [`is_protected()`]).
pub async fn commit_submodule_snapshots(
    locked_wc: &mut dyn LockedWorkingCopy,
    store: &dyn SubmoduleStore,
    workspace_name: &WorkspaceName,
    superproject: &dyn Repo,
    wc_commit: &Commit,
    snapshots: Vec<SubmoduleSnapshot>,
    protected: &HashMap<RepoPathBuf, Vec<CommitId>>,
) -> Result<SubmoduleSnapshotOutcome, SnapshotError> {
    fn other(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> SnapshotError {
        SnapshotError::Other {
            message: "Failed to snapshot submodules".to_owned(),
            err: err.into(),
        }
    }
    let wc_id = RevsetExpression::commit(wc_commit.id().clone());
    let later_commits: Vec<Commit> = wc_id
        .descendants()
        .minus(&wc_id)
        .evaluate(superproject)
        .map_err(other)?
        .stream()
        .commits(superproject.store())
        .try_collect()
        .await
        .map_err(other)?;

    let mut gitlinks = vec![];
    let mut operations = vec![];
    let mut all_rewrites = HashMap::new();
    for snapshot in snapshots {
        let Some(loader) = store.load_repo(&snapshot.name).map_err(other)? else {
            continue;
        };
        let repo = loader.load_at_head().await.map_err(other)?;
        let base = repo.store().get_commit(&snapshot.base)?;
        // The working copy wrote the tree through its own instance of the
        // submodule's store.
        let (tree_ids, labels) = snapshot.tree.into_tree_ids_and_labels();
        let tree = MergedTree::new(repo.store().clone(), tree_ids, labels);
        let base_value = Some(TreeValue::GitSubmodule(snapshot.base.clone()));
        let mut later_gitlinks = vec![];
        for commit in &later_commits {
            let value = commit.tree().path_value(&snapshot.path).await?;
            if let Some(Some(TreeValue::GitSubmodule(id))) = value.as_resolved()
                && *id != snapshot.base
                && repo.index().has_id(id).await.map_err(other)?
            {
                later_gitlinks.push(id.clone());
            }
        }
        let no_heads = vec![];
        let protected = protected.get(&snapshot.path).unwrap_or(&no_heads);
        // Whether all of these commits are ours to rewrite.
        let rewritable = async |ids: Vec<CommitId>| -> Result<bool, SnapshotError> {
            let local = locally_written(&repo, ids.iter().cloned())
                .await
                .map_err(other)?;
            for id in &ids {
                if !local.contains(id) || is_protected(&repo, id, protected).await.map_err(other)? {
                    return Ok(false);
                }
            }
            Ok(true)
        };

        let mut tx = repo.start_transaction();
        let base_indexed = repo.index().has_id(&snapshot.base).await.map_err(other)?;
        for id in &later_gitlinks {
            let commit = repo.store().get_commit(id)?;
            tx.repo_mut().add_head(&commit).await?;
        }
        if base_indexed {
            tx.repo_mut().add_head(&base).await?;
        }
        let evaluate = async |repo: &dyn Repo, expression: Arc<ResolvedRevsetExpression>| {
            let revset = expression.evaluate(repo).map_err(other)?;
            let ids: Vec<CommitId> = revset.stream().try_collect().await.map_err(other)?;
            Ok::<_, SnapshotError>(ids)
        };
        let base_id = RevsetExpression::commit(snapshot.base.clone());

        let mut amend = base_indexed && rewritable(vec![snapshot.base.clone()]).await?;
        if amend {
            // A superproject parent refers to the commit: it is history now.
            for parent in wc_commit.parents().await? {
                let value = parent.tree().path_value(&snapshot.path).await?;
                amend &= !value.iter().any(|term| *term == base_value);
            }
        }
        if amend {
            // Rewriting it rebases everything on top of it.
            let on_top = base_id.descendants().minus(&base_id);
            amend = rewritable(evaluate(tx.repo(), on_top).await?).await?;
        }
        let mut rewrites = HashMap::new();
        let commit = if amend {
            let commit = tx
                .repo_mut()
                .rewrite_commit(&base)
                .set_tree(tree)
                .write()
                .await?;
            tx.repo_mut()
                .rebase_descendants_with_options(
                    &RevsetExpression::none(),
                    &RebaseOptions::default(),
                    |old_commit, rebased| {
                        if let RebasedCommit::Rewritten(new_commit) = rebased {
                            rewrites.insert(old_commit.id().clone(), new_commit.id().clone());
                        }
                    },
                )
                .await?;
            commit
        } else {
            let parents = vec![snapshot.base.clone()];
            let commit = tx.repo_mut().new_commit(parents, tree).write().await?;
            // What later superproject commits have on top of the base moves
            // on top of the new commit, if it is all ours.
            let roots = match base_indexed && !later_gitlinks.is_empty() {
                true => {
                    let later = RevsetExpression::commits(later_gitlinks);
                    evaluate(tx.repo(), base_id.range(&later).roots()).await?
                }
                false => vec![],
            };
            let moved = RevsetExpression::commits(roots.clone()).descendants();
            if !roots.is_empty() && rewritable(evaluate(tx.repo(), moved).await?).await? {
                let stats = move_commits(
                    tx.repo_mut(),
                    &MoveCommitsLocation {
                        new_parent_ids: vec![commit.id().clone()],
                        new_child_ids: vec![],
                        target: MoveCommitsTarget::Roots(roots),
                    },
                    &RebaseOptions::default(),
                )
                .await?;
                tx.repo_mut().rebase_descendants().await?;
                for (old_id, rebased) in stats.rebased_commits {
                    if let RebasedCommit::Rewritten(new_commit) = rebased {
                        rewrites.insert(old_id, new_commit.id().clone());
                    }
                }
            }
            commit
        };
        rewrites.insert(snapshot.base.clone(), commit.id().clone());
        // Shown as the working-copy commit in `jj git submodule log`.
        tx.repo_mut()
            .set_wc_commit(workspace_name.to_owned(), commit.id().clone())
            .map_err(other)?;
        tx.repo_mut().rebase_descendants().await?;
        let operation = tx
            .write("snapshot submodule working copy")
            .await
            .map_err(other)?;
        operations.push(operation);
        gitlinks.push((snapshot.path.clone(), commit.id().clone()));
        all_rewrites.insert(snapshot.path, rewrites);
    }
    let tree = locked_wc.set_submodule_gitlinks(&gitlinks).await?;
    Ok(SubmoduleSnapshotOutcome {
        tree,
        operations: SubmoduleSnapshotOperations(operations),
        rewrites: all_rewrites,
    })
}

/// Rebases the descendants of `roots`, commits rewritten in this transaction,
/// like `rebase_descendants()` does, except at the submodule paths in
/// `rewrites`: there each descendant keeps its own gitlink, mapped through
/// the rewrites, where a rebase would merge it with its parent's change and
/// call it a conflict whenever both differ. Returns the number of commits
/// rebased.
pub async fn rebase_descendants_following_submodules(
    mut_repo: &mut MutableRepo,
    roots: Vec<CommitId>,
    rewrites: &HashMap<RepoPathBuf, HashMap<CommitId, CommitId>>,
) -> BackendResult<usize> {
    let mut num_rebased = 0;
    mut_repo
        .transform_descendants(roots, async |rewriter| {
            if !rewriter.parents_changed() {
                return Ok(());
            }
            let old_tree = rewriter.old_commit().tree();
            let builder = rewriter.rebase().await?;
            let mut tree_builder = MergedTreeBuilder::new(builder.tree());
            for (path, rewrites) in rewrites {
                // A conflicted gitlink is left to the merge.
                let old_value = old_tree.path_value(path).await?;
                let Some(old_value) = old_value.as_resolved() else {
                    continue;
                };
                let new_value = match old_value {
                    Some(TreeValue::GitSubmodule(id)) => Some(TreeValue::GitSubmodule(
                        rewrites.get(id).unwrap_or(id).clone(),
                    )),
                    other => other.clone(),
                };
                tree_builder.set_or_remove(path.clone(), new_value);
            }
            let tree = tree_builder.write_tree().await?;
            builder.set_tree(tree).write().await?;
            num_rebased += 1;
            Ok(())
        })
        .await?;
    Ok(num_rebased)
}

/// Whether submodule commit `id` must not be rewritten because a superproject
/// commit that must not change depends on it: `heads` are the submodule
/// commits such superproject commits point at, and their ancestors are
/// assumed to be referred to by those commits' own ancestors.
pub async fn is_protected(
    repo: &ReadonlyRepo,
    id: &CommitId,
    heads: &[CommitId],
) -> IndexResult<bool> {
    let index = repo.index();
    for head in heads {
        // Commits fetched by hash are in the store but not in the index.
        if head == id
            || (index.has_id(head).await?
                && index.has_id(id).await?
                && index.is_ancestor(id, head).await?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The subset of `ids` that transactions on `repo`, a submodule repo, wrote
/// themselves, as opposed to fetched. Only those are ours to rewrite: neither
/// a tracked remote bookmark nor being on no bookmark at all says who made a
/// commit.
// ponytail: walks the operation log until every id is found, so all of it
// for a commit that wasn't made here. Keep a set of locally written commits
// if that shows up in profiles.
pub async fn locally_written(
    repo: &ReadonlyRepo,
    ids: impl IntoIterator<Item = CommitId>,
) -> OpStoreResult<HashSet<CommitId>> {
    let mut wanted: HashSet<CommitId> = ids.into_iter().collect();
    let mut found = HashSet::new();
    let mut ops = pin!(op_walk::walk_ancestors(std::slice::from_ref(
        repo.operation()
    )));
    while !wanted.is_empty()
        && let Some(op) = ops.try_next().await?
    {
        // Imports don't record the commits they bring in; commit builders do.
        found.extend(wanted.extract_if(|id| op.predecessors_for_commit(id).is_some()));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gitmodules() {
        let bytes = b"
[submodule \"lib/foo\"]
\tpath = vendor/foo
\turl = https://example.com/foo.git
[submodule \"nourl\"]
\tpath = vendor/nourl
[submodule \"bar\"]
\turl = ../bar.git
\tpath = bar
";
        let parsed = parse_gitmodules(bytes).unwrap();
        assert_eq!(
            parsed.keys().collect::<Vec<_>>(),
            ["bar", "lib/foo"],
            "entry without url is skipped"
        );
        assert_eq!(
            parsed["lib/foo"],
            SubmoduleConfig {
                name: "lib/foo".to_owned(),
                path: RepoPathBuf::from_internal_string("vendor/foo").unwrap(),
                url: "https://example.com/foo.git".to_owned(),
            }
        );
        assert_eq!(parsed["bar"].url, "../bar.git");
        assert!(parse_gitmodules(b"").unwrap().is_empty());
        assert!(parse_gitmodules(b"[submodule \"x\"\npath=").is_err());
    }

    #[test]
    fn test_resolve_submodule_url() {
        let resolve = resolve_submodule_url;
        let https = "https://example.com/org/repo.git";
        assert_eq!(resolve("https://x/y.git", https), "https://x/y.git");
        assert_eq!(
            resolve("../sub.git", https),
            "https://example.com/org/sub.git"
        );
        assert_eq!(resolve("../../o/s", https), "https://example.com/o/s");
        assert_eq!(resolve("./sub", "https://h/repo/"), "https://h/repo/sub");
        assert_eq!(resolve("../sub", "git@h:org/repo"), "git@h:org/sub");
        assert_eq!(resolve("../sub", "git@h:repo"), "git@h:sub");
        assert_eq!(resolve("../sub", "/tmp/repo"), "/tmp/sub");
    }
}
