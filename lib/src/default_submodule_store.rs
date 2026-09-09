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

#![expect(missing_docs)]

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use percent_encoding::AsciiSet;
use percent_encoding::NON_ALPHANUMERIC;
use percent_encoding::percent_decode_str;
use percent_encoding::utf8_percent_encode;

use crate::default_backend_factories::default_backend_factories;
use crate::file_util::IoResultExt as _;
use crate::file_util::PathError;
use crate::repo::RepoLoader;
use crate::settings::UserSettings;
use crate::submodule_store::SubmoduleStore;
use crate::submodule_store::SubmoduleStoreError;

/// Submodule names come from `.gitmodules`, i.e. untrusted repo content.
/// Encode everything that could be a path separator, a `..` component, or
/// otherwise unportable, so the store directory name can never escape the
/// store. `.` is left alone except when it leads the name (see
/// [`encode_name`]).
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.');

/// Reversibly maps a submodule name to a directory name.
pub fn encode_name(name: &str) -> String {
    let encoded = utf8_percent_encode(name, ENCODE_SET).to_string();
    match encoded.strip_prefix('.') {
        Some(rest) => format!("%2E{rest}"),
        None => encoded,
    }
}

/// Inverse of [`encode_name`].
pub fn decode_name(dir_name: &str) -> String {
    percent_decode_str(dir_name)
        .decode_utf8_lossy()
        .into_owned()
}

#[derive(Debug)]
pub struct DefaultSubmoduleStore {
    path: PathBuf,
    settings: UserSettings,
}

impl DefaultSubmoduleStore {
    pub const NAME: &str = "default";

    /// Load an existing SubmoduleStore
    pub fn load(settings: &UserSettings, store_path: &Path) -> Self {
        Self {
            path: store_path.to_path_buf(),
            settings: settings.clone(),
        }
    }

    pub fn init(settings: &UserSettings, store_path: &Path) -> Self {
        Self::load(settings, store_path)
    }
}

impl SubmoduleStore for DefaultSubmoduleStore {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn list(&self) -> Result<Vec<String>, SubmoduleStoreError> {
        let mut names = vec![];
        for entry in fs::read_dir(&self.path).context(&self.path)? {
            let entry = entry.context(&self.path)?;
            if entry.path().is_dir() {
                names.push(decode_name(&entry.file_name().to_string_lossy()));
            }
        }
        names.sort();
        Ok(names)
    }

    fn repo_path(&self, name: &str) -> PathBuf {
        self.path.join(encode_name(name))
    }

    fn load_repo(&self, name: &str) -> Result<Option<RepoLoader>, SubmoduleStoreError> {
        let path = self.repo_path(name);
        if !path.exists() {
            return Ok(None);
        }
        // ponytail: default factories only; submodules are always internal-git
        // repos. Thread the superproject's StoreFactories through if a custom
        // backend ever needs to host submodules.
        let loader =
            RepoLoader::init_from_file_system(&self.settings, &path, &default_backend_factories())?;
        Ok(Some(loader))
    }

    fn delete(&self, name: &str) -> Result<(), SubmoduleStoreError> {
        let path = self.repo_path(name);
        match fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(PathError { path, source: err }.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_name_round_trips_and_stays_in_dir() {
        for name in [
            "plain", "a/b", "..", ".hidden", "50%", "sp ace", "ünï", "a\\b", "..\\x",
        ] {
            let encoded = encode_name(name);
            assert_eq!(decode_name(&encoded), name, "{name}");
            assert!(!encoded.contains(['/', '\\']), "{encoded}");
            assert!(!encoded.starts_with('.'), "{encoded}");
        }
    }
}
