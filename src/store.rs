use std::collections::BTreeMap;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::catalog::validate as validate_catalog;
use crate::catalog::{Event, Model};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct State {
    pub initialized: bool,
    pub catalog: BTreeMap<String, Model>,
    pub subscriptions: BTreeMap<u64, Subscription>,
    pub last_success: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub channel_id: u64,
    pub role_id: Option<u64>,
    pub free_only: bool,
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub min_context: u32,
    #[serde(default = "default_true")]
    pub ping_enabled: bool,
    #[serde(default)]
    pub compact: bool,
    pub pending: Vec<Event>,
}

fn default_true() -> bool {
    true
}

impl Subscription {
    pub fn matches(&self, model: &Model) -> bool {
        if (self.free_only && !model.is_free())
            || model.context_length < u64::from(self.min_context)
        {
            return false;
        }
        let search = self.search.trim().to_lowercase();
        search.is_empty()
            || model.name.to_lowercase().contains(&search)
            || model.id.to_lowercase().contains(&search)
    }
}

fn validate_ids(state: &State) -> Result<(), Error> {
    for (guild, sub) in &state.subscriptions {
        if *guild == 0 || sub.channel_id == 0 {
            return Err("subscription IDs must be nonzero".into());
        }
        if let Some(role) = sub.role_id {
            if role == 0 {
                return Err("subscription role ID must be nonzero".into());
            }
            if role == *guild {
                return Err("subscription role ID must differ from guild ID".into());
            }
        }
    }
    Ok(())
}

pub struct Store {
    pub state: State,
    path: PathBuf,
}

impl Store {
    pub async fn load(path: PathBuf) -> Result<Self, Error> {
        let state: State = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error.into()),
        };
        if state.initialized || !state.catalog.is_empty() {
            validate_catalog(&state.catalog)?;
        }
        validate_ids(&state)?;
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
                        search: " Example ".into(),
                        min_context: 8192,
                        ping_enabled: false,
                        compact: true,
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
                        search: String::new(),
                        min_context: 0,
                        ping_enabled: true,
                        compact: false,
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
    async fn legacy_states_preserve_pending_and_default_settings() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        let mut expected = populated_state();
        let mut legacy = serde_json::to_value(&expected).unwrap();
        for sub in legacy["subscriptions"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            for field in ["search", "min_context", "ping_enabled", "compact"] {
                sub.as_object_mut().unwrap().remove(field);
            }
        }
        for sub in expected.subscriptions.values_mut() {
            sub.search.clear();
            sub.min_context = 0;
            sub.ping_enabled = true;
            sub.compact = false;
        }
        let bytes = serde_json::to_vec(&legacy).unwrap();
        let decoded: State = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.subscriptions, expected.subscriptions);
        tokio::fs::write(&path, &bytes).await.unwrap();
        let mut loaded = Store::load(path.clone()).await.unwrap();
        assert_eq!(loaded.state.subscriptions, expected.subscriptions);
        assert_eq!(loaded.state.catalog, expected.catalog);
        assert_eq!(loaded.state.last_success, expected.last_success);
        assert!(loaded.state.initialized);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
        loaded.commit(loaded.state.clone()).await.unwrap();
        assert_eq!(
            Store::load(path).await.unwrap().state.subscriptions,
            expected.subscriptions
        );
    }

    #[test]
    fn subscription_filters_match_name_or_id_and_combine_constraints() {
        let state = populated_state();
        let mut model = state.catalog["example/model"].clone();
        model.name = "Friendly ÉXAMPLE".into();
        let mut sub = state.subscriptions[&u64::MAX].clone();
        for search in ["", " \t\n", " EXAMPLE/MO ", " fRiEnDlY ", "éxample"] {
            sub.search = search.into();
            assert!(sub.matches(&model), "{search:?}");
        }
        for search in ["missing", "Description", "friendly example/model"] {
            sub.search = search.into();
            assert!(!sub.matches(&model), "{search:?}");
        }
        sub.search = "friendly".into();
        sub.min_context = 8193;
        assert!(!sub.matches(&model));
        sub.min_context = 8192;
        assert!(sub.matches(&model));
        model.pricing.insert("prompt".into(), "1".into());
        assert!(!sub.matches(&model));
        sub.free_only = false;
        assert!(sub.matches(&model));
        sub.search = "missing".into();
        assert!(!sub.matches(&model));
        sub.search.clear();
        model.context_length = 0;
        assert!(!sub.matches(&model));
        sub.min_context = 0;
        assert!(sub.matches(&model));
        sub.min_context = u32::MAX;
        model.context_length = u64::MAX;
        assert!(sub.matches(&model));
        model.pricing.clear();
        sub.free_only = true;
        assert!(!sub.matches(&model));
    }

    #[tokio::test]
    async fn load_rejects_invalid_subscription_ids_without_overwriting() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        for (guild, channel, role) in [
            (0, 42, None),
            (1, 0, None),
            (1, 42, Some(0)),
            (1, 42, Some(1)),
        ] {
            for ping_enabled in [false, true] {
                let mut state = populated_state();
                let mut sub = state.subscriptions[&u64::MAX].clone();
                sub.channel_id = channel;
                sub.role_id = role;
                sub.ping_enabled = ping_enabled;
                state.subscriptions = BTreeMap::from([(guild, sub)]);
                let bytes = serde_json::to_vec(&state).unwrap();
                tokio::fs::write(&path, &bytes).await.unwrap();
                assert!(Store::load(path.clone()).await.is_err());
                assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
            }
        }
    }

    #[tokio::test]
    async fn load_validates_catalog_for_initialized_and_uninitialized_states() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        for initialized in [false, true] {
            for invalid in ["empty", "key", "id", "name"] {
                let mut state = populated_state();
                state.initialized = initialized;
                match invalid {
                    "empty" => state.catalog.clear(),
                    "key" => {
                        let model = state.catalog.pop_first().unwrap().1;
                        state.catalog.insert("wrong/key".into(), model);
                    }
                    "id" => {
                        let mut model = state.catalog.pop_first().unwrap().1;
                        model.id = " ".into();
                        state.catalog.insert(model.id.clone(), model);
                    }
                    _ => state.catalog.values_mut().next().unwrap().name = " ".into(),
                }
                let bytes = serde_json::to_vec(&state).unwrap();
                tokio::fs::write(&path, &bytes).await.unwrap();
                let loaded = Store::load(path.clone()).await;
                assert_eq!(loaded.is_ok(), !initialized && invalid == "empty");
                assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
            }
        }
    }

    #[tokio::test]
    async fn new_settings_round_trip_preserves_pending_without_filtering() {
        let directory = TestDirectory::new();
        let path = directory.state_path();
        let mut store = Store::load(path.clone()).await.unwrap();
        for ping_enabled in [false, true] {
            let mut state = populated_state();
            let sub = state.subscriptions.get_mut(&u64::MAX).unwrap();
            sub.search = " Unmatched ÉXAMPLE ".into();
            sub.min_context = u32::MAX;
            sub.ping_enabled = ping_enabled;
            sub.compact = !ping_enabled;
            assert!(!sub.matches(&sub.pending[0].model));
            store.commit(state.clone()).await.unwrap();
            let loaded = Store::load(path.clone()).await.unwrap();
            assert_eq!(loaded.state.subscriptions, state.subscriptions);
            assert_eq!(loaded.state.catalog, state.catalog);
        }
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
