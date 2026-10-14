//! Which profiles exist: the names every profile-scoped command and
//! `sss status` work from.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::config::Config;
use crate::repo::Repo;
use crate::secrets::StateRoot;

/// The known profiles, sorted: those with a directory in the data
/// repository, those with a `[profiles.<name>]` table in `config`, and those
/// with a credential file under `state_root`.
pub fn known(repo: &Repo, state_root: &StateRoot, config: &Config) -> Result<Vec<String>> {
    let mut names: BTreeSet<String> = repo.profiles()?.into_iter().collect();
    names.extend(config.profiles.keys().cloned());
    names.extend(state_root.profiles()?);
    Ok(names.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::test_support::session;

    #[test]
    fn known_is_the_sorted_union_of_repo_config_and_state_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path().join("data"));
        std::fs::create_dir_all(repo.profiles_dir().join("carol")).unwrap();
        std::fs::create_dir_all(repo.profiles_dir().join("alice")).unwrap();
        std::fs::write(repo.config_path(), "[profiles.bob]\n[profiles.alice]\n").unwrap();
        let state_root = StateRoot::at(tmp.path().join("state"));
        state_root
            .profile("dave")
            .unwrap()
            .save_pandora_session(&session())
            .unwrap();
        std::fs::create_dir_all(state_root.root().join("erin")).unwrap();
        let config = Config::load(&repo.config_path()).unwrap();
        assert_eq!(
            known(&repo, &state_root, &config).unwrap(),
            ["alice", "bob", "carol", "dave"]
        );
    }

    #[test]
    fn nothing_anywhere_is_no_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path().join("data"));
        std::fs::create_dir_all(repo.root()).unwrap();
        std::fs::write(repo.config_path(), "").unwrap();
        let config = Config::load(&repo.config_path()).unwrap();
        let state_root = StateRoot::at(tmp.path().join("state"));
        assert!(known(&repo, &state_root, &config).unwrap().is_empty());
    }
}
