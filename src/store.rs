use std::collections::BTreeMap;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::catalog::{Event, Model};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct State {
    pub initialized: bool,
    pub catalog: BTreeMap<String, Model>,
    pub subscriptions: BTreeMap<u64, Subscription>,
    pub last_success: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub channel_id: u64,
    pub role_id: Option<u64>,
    pub free_only: bool,
    pub pending: Vec<Event>,
}

pub struct Store {
    pub state: State,
    path: PathBuf,
}

impl Store {
    pub async fn load(path: PathBuf) -> Result<Self, Error> {
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { state, path })
    }

    pub async fn commit(&mut self, next: State) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&next)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let name = path.file_name().ok_or("state path has no file name")?;
            let (temporary, mut file) = loop {
                let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let mut temporary_name = name.to_os_string();
                temporary_name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
                let temporary = path.with_file_name(temporary_name);
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                {
                    Ok(file) => break (temporary, file),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error.into()),
                }
            };
            let result = (|| -> Result<(), Error> {
                file.write_all(&bytes)?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&temporary, &path)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        })
        .await??;
        self.state = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "model-radar-store-test-{}-{}",
                    std::process::id(),
                    sequence
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{error}"),
                }
            }
        }

        fn state_path(&self) -> PathBuf {
            self.0.join("state.json")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn populated_state() -> State {
        let model = Model {
            id: "example/model".into(),
            name: "Example".into(),
            description: "Description".into(),
            context_length: 8192,
            pricing: BTreeMap::from([
                ("prompt".into(), "0".into()),
                ("completion".into(), "0".into()),
            ]),
        };
        State {
            initialized: true,
            catalog: BTreeMap::from([(model.id.clone(), model.clone())]),
            subscriptions: BTreeMap::from([
                (
                    u64::MAX,
                    Subscription {
                        channel_id: 42,
                        role_id: Some(99),
                        free_only: true,
                        pending: vec![Event {
                            model,
                            kind: "new".into(),
                        }],
                    },
                ),
                (
                    1,
                    Subscription {
                        channel_id: 43,
                        role_id: None,
                        free_only: false,
                        pending: Vec::new(),
                    },
                ),
            ]),
            last_success: Some(123456),
        }
    }

    #[tokio::test]
    async fn missing_file_defaults_without_writing() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        let store = Store::load(path.clone()).await.unwrap();
        assert!(!store.state.initialized);
        assert!(store.state.catalog.is_empty());
        assert!(store.state.subscriptions.is_empty());
        assert_eq!(store.state.last_success, None);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn persistence_round_trip_and_replacement() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        let mut store = Store::load(path.clone()).await.unwrap();
        let next = populated_state();
        store.commit(next.clone()).await.unwrap();
        let loaded = Store::load(path.clone()).await.unwrap();
        let expected = serde_json::to_value(&next).unwrap();
        assert_eq!(serde_json::to_value(&store.state).unwrap(), expected);
        assert_eq!(serde_json::to_value(&loaded.state).unwrap(), expected);
        store.commit(State::default()).await.unwrap();
        let loaded = Store::load(path).await.unwrap();
        assert!(!loaded.state.initialized);
        assert!(loaded.state.catalog.is_empty());
        assert!(loaded.state.subscriptions.is_empty());
        assert_eq!(loaded.state.last_success, None);
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn corrupt_files_are_errors_and_not_overwritten() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        for bytes in ["", "not json", "{}", "null", "{\"initialized\":false"] {
            tokio::fs::write(&path, bytes).await.unwrap();
            assert!(Store::load(path.clone()).await.is_err());
            assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes.as_bytes());
        }
    }

    #[tokio::test]
    async fn failed_rename_preserves_state_and_cleans_temporary() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        let mut store = Store::load(path.clone()).await.unwrap();
        tokio::fs::create_dir(&path).await.unwrap();
        assert!(store.commit(populated_state()).await.is_err());
        assert!(!store.state.initialized);
        assert!(store.state.catalog.is_empty());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
        assert!(Store::load(path).await.is_err());
    }

    #[tokio::test]
    async fn missing_parent_commit_preserves_state() {
        let directory = TestDirectory::new();
        let path = directory.0.join("missing").join("state.json");
        let mut store = Store::load(path).await.unwrap();
        assert!(store.commit(populated_state()).await.is_err());
        assert!(!store.state.initialized);
        assert!(store.state.subscriptions.is_empty());
    }
}
