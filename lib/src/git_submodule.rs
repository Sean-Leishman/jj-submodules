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
use crate::merged_tree::MergedTree;
use crate::object_id::ObjectId as _;
use crate::op_store::OpStoreResult;
use crate::op_walk;
use crate::ref_name::RemoteName;
use crate::ref_name::WorkspaceName;
use crate::repo::BackendInitializer;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::repo::RepoInitError;
use crate::repo::RepoLoaderError;
use crate::repo_path::RepoPath;
use crate::repo_path::RepoPathBuf;
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

/// Submodule operations written by [`snapshot_submodules()`], to be published
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

/// Snapshots populated submodules with local changes: commits each one's
/// directory in its own repo and moves the gitlink in the working-copy tree
/// to the new commit. Returns the new working-copy tree, or `None` if no
/// gitlink moved.
///
/// A submodule commit is rewritten in place only if an earlier snapshot
/// created it (the submodule repo records it as the working-copy commit of
/// the superproject's workspace) and no parent of `wc_commit` refers to it;
/// otherwise the new commit becomes its child.
///
/// The submodule operations are written but not published, so that a
/// superproject snapshot that doesn't complete leaves nothing behind in the
/// submodule repos but unreferenced commits.
pub async fn snapshot_submodules(
    locked_wc: &mut dyn LockedWorkingCopy,
    store: &dyn SubmoduleStore,
    workspace_name: &WorkspaceName,
    wc_commit: &Commit,
) -> Result<Option<(MergedTree, SubmoduleSnapshotOperations)>, SnapshotError> {
    let snapshots = locked_wc.snapshot_submodules().await?;
    if snapshots.is_empty() {
        return Ok(None);
    }
    let mut gitlinks = vec![];
    let mut operations = vec![];
    for snapshot in snapshots {
        let error = |err: Box<dyn std::error::Error + Send + Sync>| SnapshotError::Other {
            message: format!(
                "Failed to snapshot submodule at {}",
                snapshot.path.as_internal_file_string()
            ),
            err,
        };
        let Some(loader) = store
            .load_repo(&snapshot.name)
            .map_err(|err| error(err.into()))?
        else {
            continue;
        };
        let repo = loader
            .load_at_head()
            .await
            .map_err(|err| error(err.into()))?;
        let base = repo.store().get_commit(&snapshot.base)?;
        // The working copy wrote the tree through its own instance of the
        // submodule's store.
        let (tree_ids, labels) = snapshot.tree.into_tree_ids_and_labels();
        let tree = MergedTree::new(repo.store().clone(), tree_ids, labels);
        let mut amend = repo.view().get_wc_commit_id(workspace_name) == Some(&snapshot.base);
        if amend {
            // A superproject parent refers to the commit: it is history now.
            let old_value = Some(TreeValue::GitSubmodule(snapshot.base.clone()));
            for parent in wc_commit.parents().await? {
                let value = parent.tree().path_value(&snapshot.path).await?;
                amend &= !value.iter().any(|term| *term == old_value);
            }
        }
        let mut tx = repo.start_transaction();
        let builder = if amend {
            tx.repo_mut().rewrite_commit(&base)
        } else {
            let parents = vec![snapshot.base.clone()];
            tx.repo_mut().new_commit(parents, tree.clone())
        };
        let commit = builder.set_tree(tree).write().await?;
        tx.repo_mut()
            .set_wc_commit(workspace_name.to_owned(), commit.id().clone())
            .map_err(|err| error(err.into()))?;
        tx.repo_mut().rebase_descendants().await?;
        let operation = tx
            .write("snapshot submodule working copy")
            .await
            .map_err(|err| error(err.into()))?;
        operations.push(operation);
        gitlinks.push((snapshot.path, commit.id().clone()));
    }
    if gitlinks.is_empty() {
        return Ok(None);
    }
    let tree = locked_wc.set_submodule_gitlinks(&gitlinks).await?;
    Ok(Some((tree, SubmoduleSnapshotOperations(operations))))
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
