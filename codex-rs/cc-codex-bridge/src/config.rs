//! Bridge configuration. Everything has a default except the model list,
//! which stays empty until the user names the Claude models they want.

use anyhow::Context;
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;

/// Claude Code version the stream-json handling was tested with.
pub const TESTED_CLAUDE_VERSION: &str = "2.1.322";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum ModelEntry {
    Id(String),
    Full(ModelConfig),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigFile {
    engine_command: Option<Vec<String>>,
    models: Option<Vec<ModelEntry>>,
    state_dir: Option<PathBuf>,
    tested_claude_version: Option<String>,
    interrupt_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Command that starts the engine. The bridge appends the stream-json
    /// flags, `--session-id` or `--resume`, and `--model`. It runs in the
    /// bridge's own environment, so whichever login the user set up for
    /// `claude` is the one used; the bridge never reads it.
    pub engine_command: Vec<String>,
    /// Claude models offered to the Codex client.
    pub models: Vec<ModelConfig>,
    pub state_dir: PathBuf,
    pub tested_claude_version: String,
    /// How long an interrupt may go unconfirmed before the engine process is
    /// stopped.
    pub interrupt_timeout_ms: u64,
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CCB_CONFIG") {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cc-codex-bridge")
        .join("config.json")
}

fn default_state_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cc-codex-bridge")
}

impl Config {
    /// Load the config file at `path`; a missing file means all defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let file: ConfigFile = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("cannot parse {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        let engine_command = file
            .engine_command
            .unwrap_or_else(|| vec!["claude".to_string()]);
        anyhow::ensure!(
            !engine_command.is_empty(),
            "engineCommand must not be empty, e.g. [\"claude\"]"
        );
        let models = file
            .models
            .unwrap_or_default()
            .into_iter()
            .map(|entry| match entry {
                ModelEntry::Id(id) => ModelConfig {
                    id,
                    display_name: None,
                    description: None,
                    is_default: false,
                },
                ModelEntry::Full(model) => model,
            })
            .collect();
        Ok(Self {
            engine_command,
            models,
            state_dir: file.state_dir.unwrap_or_else(default_state_dir),
            tested_claude_version: file
                .tested_claude_version
                .unwrap_or_else(|| TESTED_CLAUDE_VERSION.to_string()),
            interrupt_timeout_ms: file.interrupt_timeout_ms.unwrap_or(5_000),
        })
    }

    /// The model the client gets when it names none.
    pub fn default_model(&self) -> Option<&ModelConfig> {
        self.models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| self.models.first())
    }
}
