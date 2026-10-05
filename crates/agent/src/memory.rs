use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Persistent, agent-written memory that survives across threads and runs.
///
/// Facts are stored as a flat `key → value` map in a single JSON file under the
/// user data directory. Only the key index is ever injected into the system
/// prompt — values are read on demand via the `recall` tool — so memory never
/// bloats the prompt.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    #[serde(default)]
    entries: BTreeMap<String, String>,
}

impl Memory {
    /// Stores a fact under `key`, overwriting any previous value.
    pub fn remember(&mut self, key: &str, value: &str) {
        self.entries.insert(key.to_owned(), value.to_owned());
    }

    /// Returns the fact stored under `key`, if any.
    pub fn recall(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// Removes the fact stored under `key`. Returns whether a fact was removed.
    pub fn forget(&mut self, key: &str) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Returns the fact keys in stable, sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Whether no facts are stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Renders a compact key index for the system prompt — keys only, never
    /// values — so the model knows what it can `recall` without inlining every
    /// fact.
    pub fn index_prompt(&self) -> String {
        let mut output = String::new();
        for key in self.entries.keys() {
            output.push_str("- ");
            output.push_str(key);
            output.push('\n');
        }
        output
    }
}

/// Loads and persists [`Memory`] to a single JSON file, writing atomically so a
/// crash mid-save never leaves a truncated file behind.
#[derive(Debug)]
pub struct MemoryStore {
    path: PathBuf,
    memory: Memory,
}

impl MemoryStore {
    /// The default on-disk location for agent memory, under the user data dir.
    pub fn default_path() -> PathBuf {
        paths::data_dir().join("agent_memory.json")
    }

    /// Creates a store backed by `path` without reading it yet.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            memory: Memory::default(),
        }
    }

    /// Loads memory from `path`. A missing file yields empty memory; an invalid
    /// file is an error so callers don't silently discard unreadable data.
    pub fn load(path: PathBuf) -> Result<Self> {
        let memory = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing memory file {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Memory::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading memory file {}", path.display()));
            }
        };
        Ok(Self { path, memory })
    }

    /// The file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stores a fact and persists it.
    pub fn remember(&mut self, key: &str, value: &str) -> Result<()> {
        self.memory.remember(key, value);
        self.save()
    }

    /// Returns the fact stored under `key`, if any.
    pub fn recall(&self, key: &str) -> Option<&str> {
        self.memory.recall(key)
    }

    /// Removes a fact and persists the change. Returns whether a fact existed.
    pub fn forget(&mut self, key: &str) -> Result<bool> {
        let removed = self.memory.forget(key);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// The fact keys in stable, sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.memory.keys()
    }

    /// Whether no facts are stored.
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty()
    }

    /// Renders a compact key index for the system prompt.
    pub fn index_prompt(&self) -> String {
        self.memory.index_prompt()
    }

    /// Serializes and atomically writes the current memory to disk.
    pub fn save(&self) -> Result<()> {
        let contents = serde_json::to_vec_pretty(&self.memory).context("serializing memory")?;
        atomic_write(&self.path, &contents)
    }
}

/// Writes `contents` to `path` via a temp file in the same directory followed
/// by an atomic rename, so readers never observe a partially-written file.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating memory directory {}", parent.display()))?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating memory temp file in {}", parent.display()))?;
    temp.write_all(contents)
        .with_context(|| format!("writing memory temp file {}", temp.path().display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persisting memory file {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_crud_and_index() {
        let mut memory = Memory::default();
        assert!(memory.is_empty());
        assert_eq!(memory.index_prompt(), "");

        memory.remember("deploy-host", "t-rex.proxmox.local");
        memory.remember("db-url", "postgres://localhost/patsprints");
        assert_eq!(memory.recall("deploy-host"), Some("t-rex.proxmox.local"));
        assert_eq!(memory.recall("missing"), None);

        let index = memory.index_prompt();
        assert!(index.contains("- deploy-host"));
        assert!(index.contains("- db-url"));
        // Values must never leak into the injected index.
        assert!(!index.contains("t-rex.proxmox.local"));

        assert!(memory.forget("deploy-host"));
        assert!(!memory.forget("deploy-host"));
        assert_eq!(memory.recall("deploy-host"), None);
    }

    #[test]
    fn store_round_trips_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");

        {
            let mut store = MemoryStore::new(path.clone());
            store.remember("answer", "42").unwrap();
            store.remember("name", "zed").unwrap();
        }

        let store = MemoryStore::load(path).unwrap();
        assert_eq!(store.recall("answer"), Some("42"));
        assert_eq!(store.recall("name"), Some("zed"));
        assert!(store.index_prompt().contains("- answer"));
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::load(dir.path().join("missing.json")).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn forget_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");

        let mut store = MemoryStore::new(path.clone());
        store.remember("drop-me", "value").unwrap();
        assert!(store.forget("drop-me").unwrap());
        assert!(!store.forget("drop-me").unwrap());

        let reloaded = MemoryStore::load(path).unwrap();
        assert_eq!(reloaded.recall("drop-me"), None);
    }
}
