// Copyright 2023 The Jujutsu Authors
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

//! Storage of Git submodules as full, workspace-less jj repos owned by the
//! superproject. See `docs/design/git-submodule-storage.md`.

use std::fmt::Debug;
use std::path::PathBuf;

use thiserror::Error;

use crate::file_util::PathError;
use crate::repo::RepoLoader;
use crate::repo::StoreLoadError;

/// Error from a [`SubmoduleStore`] operation.
#[derive(Debug, Error)]
pub enum SubmoduleStoreError {
    /// File system error inside the store.
    #[error(transparent)]
    Path(#[from] PathError),
    /// The submodule repo exists but couldn't be loaded.
    #[error("Failed to load submodule repo")]
    Load(#[from] StoreLoadError),
}

/// Stores the repos of the superproject's Git submodules, keyed by the
/// submodule name from `.gitmodules`.
///
/// Submodule repos have no workspace; the superproject's working copy owns
/// their files. All interaction with a submodule goes through the
/// superproject.
pub trait SubmoduleStore: Send + Sync + Debug {
    /// Name of this store implementation (written to the `type` file).
    fn name(&self) -> &str;

    /// Names of the submodules present in the store, sorted.
    fn list(&self) -> Result<Vec<String>, SubmoduleStoreError>;

    /// Directory the repo for submodule `name` lives in (whether or not it
    /// exists yet).
    fn repo_path(&self, name: &str) -> PathBuf;

    /// Loads the repo for submodule `name`, or `None` if it isn't in the store.
    fn load_repo(&self, name: &str) -> Result<Option<RepoLoader>, SubmoduleStoreError>;

    /// Removes submodule `name` from the store. No-op if absent.
    fn delete(&self, name: &str) -> Result<(), SubmoduleStoreError>;
}
