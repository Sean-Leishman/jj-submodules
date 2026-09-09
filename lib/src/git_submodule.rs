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
use std::fs;
use std::sync::Arc;

use futures::AsyncReadExt as _;
use thiserror::Error;

use crate::backend::BackendError;
use crate::backend::TreeValue;
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
use crate::git::UnexpectedGitBackendError;
use crate::git::add_remote;
use crate::git_backend::GitBackend;
use crate::merged_tree::MergedTree;
use crate::ref_name::RemoteName;
use crate::repo::BackendInitializer;
use crate::repo::ReadonlyRepo;
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

/// Inverts [`read_gitmodules`]: gitlink path → submodule name.
pub fn gitmodules_by_path(
    submodules: &BTreeMap<String, SubmoduleConfig>,
) -> HashMap<RepoPathBuf, String> {
    submodules
        .values()
        .map(|config| (config.path.clone(), config.name.clone()))
        .collect()
}

/// Creates a new workspace-less repo for submodule `name` in `store` and
/// fetches all branches and tags from `url`.
pub async fn clone_submodule(
    store: &dyn SubmoduleStore,
    settings: &UserSettings,
    name: &str,
    url: &str,
    object_hash: gix::hash::Kind,
    callback: &mut dyn GitSubprocessCallback,
) -> Result<GitImportStats, GitSubmoduleError> {
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
        fetch_submodule(&repo, settings, callback).await
    }
    .await;
    if result.is_err() {
        fs::remove_dir_all(&repo_path).ok();
    }
    result
}

/// Fetches all branches and tags of a submodule repo from its remote.
///
/// Unreachable commits are never abandoned: the superproject may reference
/// submodule commits that are on no branch.
pub async fn fetch_submodule(
    repo: &Arc<ReadonlyRepo>,
    settings: &UserSettings,
    callback: &mut dyn GitSubprocessCallback,
) -> Result<GitImportStats, GitSubmoduleError> {
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
    Ok(stats)
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
}
