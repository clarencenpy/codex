//! Persistence: the thread to Claude session map and finished turns, as
//! plain JSON files under the state directory.
//!
//! - `threads.json`: every thread's record.
//! - `turns/<thread>.jsonl`: one finished turn per line.
//! - `turns/<thread>.active.json`: the turn in progress, so a bridge that
//!   dies mid-turn can still show it (as interrupted) afterwards.

use anyhow::Context;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRecord {
    pub id: String,
    pub claude_session_id: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub preview: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub archived: bool,
    /// Whether Claude Code has seen this session, so the next launch resumes
    /// it instead of starting a new one.
    #[serde(default)]
    pub started: bool,
}

pub struct Store {
    dir: PathBuf,
    threads: Mutex<BTreeMap<String, ThreadRecord>>,
}

impl Store {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("turns"))
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let file = dir.join("threads.json");
        let threads: BTreeMap<String, ThreadRecord> = match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("cannot parse {}", file.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", file.display())),
        };
        let store = Self {
            dir: dir.to_path_buf(),
            threads: Mutex::new(threads),
        };
        // A turn that was in progress when the previous bridge stopped did
        // not finish as far as this bridge can tell: record it as interrupted.
        let ids: Vec<String> = store.lock().keys().cloned().collect();
        for id in ids {
            if let Some(mut turn) = store.take_active_turn(&id) {
                turn.status = TurnStatus::Interrupted;
                store.append_turn(&id, &turn)?;
            }
        }
        Ok(store)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ThreadRecord>> {
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, threads: &BTreeMap<String, ThreadRecord>) -> anyhow::Result<()> {
        write_atomically(
            &self.dir.join("threads.json"),
            &serde_json::to_vec_pretty(threads)?,
        )
    }

    pub fn insert(&self, record: ThreadRecord) -> anyhow::Result<()> {
        let mut threads = self.lock();
        threads.insert(record.id.clone(), record);
        self.save(&threads)
    }

    pub fn get(&self, id: &str) -> Option<ThreadRecord> {
        self.lock().get(id).cloned()
    }

    /// Threads, most recently updated first.
    pub fn list(&self, archived: bool) -> Vec<ThreadRecord> {
        let mut records: Vec<ThreadRecord> = self
            .lock()
            .values()
            .filter(|record| record.archived == archived)
            .cloned()
            .collect();
        records.sort_by_key(|record| std::cmp::Reverse((record.updated_at, record.created_at)));
        records
    }

    pub fn update(&self, id: &str, change: impl FnOnce(&mut ThreadRecord)) -> anyhow::Result<()> {
        let mut threads = self.lock();
        if let Some(record) = threads.get_mut(id) {
            change(record);
        }
        self.save(&threads)
    }

    fn turns_file(&self, id: &str) -> PathBuf {
        self.dir.join("turns").join(format!("{id}.jsonl"))
    }

    fn active_file(&self, id: &str) -> PathBuf {
        self.dir.join("turns").join(format!("{id}.active.json"))
    }

    pub fn append_turn(&self, id: &str, turn: &Turn) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(turn)?;
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.turns_file(id))?;
        file.write_all(&line)?;
        let _ = std::fs::remove_file(self.active_file(id));
        Ok(())
    }

    pub fn turns(&self, id: &str) -> Vec<Turn> {
        let Ok(text) = std::fs::read_to_string(self.turns_file(id)) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    pub fn save_active_turn(&self, id: &str, turn: &Turn) -> anyhow::Result<()> {
        write_atomically(&self.active_file(id), &serde_json::to_vec(turn)?)
    }

    fn take_active_turn(&self, id: &str) -> Option<Turn> {
        let file = self.active_file(id);
        let text = std::fs::read_to_string(&file).ok()?;
        let _ = std::fs::remove_file(&file);
        serde_json::from_str(&text).ok()
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}
