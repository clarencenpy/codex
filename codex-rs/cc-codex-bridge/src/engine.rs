//! The engine: one Claude Code process per thread, spoken to in stream-json.
//!
//! [`EngineLauncher`] is a trait so tests can stand in a scripted engine
//! without starting a process.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub cwd: PathBuf,
    pub session_id: String,
    /// Continue the existing conversation instead of starting a new one.
    pub resume: bool,
    pub model: Option<String>,
}

#[derive(Debug)]
pub enum EngineEvent {
    /// One JSON message the engine printed.
    Message(Value),
    /// The process is gone; the string says why, as far as is known.
    Exited(String),
}

/// Both ends of a running engine. Dropping `to_engine` closes the engine's
/// stdin, which ends the process.
pub struct EngineIo {
    pub to_engine: mpsc::UnboundedSender<Value>,
    pub from_engine: mpsc::UnboundedReceiver<EngineEvent>,
}

pub trait EngineLauncher: Send + Sync {
    fn launch(&self, spec: &LaunchSpec) -> anyhow::Result<EngineIo>;
}

/// Starts the real `claude` CLI.
pub struct ClaudeLauncher {
    pub command: Vec<String>,
}

impl ClaudeLauncher {
    /// Arguments appended to the engine command. Verified against Claude Code
    /// 2.1.322: stream-json in and out needs no `--print` when stdio is piped,
    /// `--verbose` is required for stream-json output, and
    /// `--permission-prompt-tool stdio` turns permission prompts into
    /// `can_use_tool` control requests on stdout.
    pub fn arguments(spec: &LaunchSpec) -> Vec<String> {
        let mut args: Vec<String> = [
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        args.push(
            if spec.resume {
                "--resume"
            } else {
                "--session-id"
            }
            .to_string(),
        );
        args.push(spec.session_id.clone());
        if let Some(model) = &spec.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        args
    }
}

impl EngineLauncher for ClaudeLauncher {
    fn launch(&self, spec: &LaunchSpec) -> anyhow::Result<EngineIo> {
        let (program, base_args) = self
            .command
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("engineCommand is empty"))?;
        let mut child = tokio::process::Command::new(program)
            .args(base_args)
            .args(Self::arguments(spec))
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| anyhow::anyhow!("could not start `{program}`: {err}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("engine has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("engine has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("engine has no stderr"))?;

        let (to_engine, mut outgoing) = mpsc::unbounded_channel::<Value>();
        let (events, from_engine) = mpsc::unbounded_channel::<EngineEvent>();

        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let mut line = message.to_string();
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin here closes it, which lets the engine exit.
        });

        let stderr_tail = tokio::spawn(async move {
            let mut tail: Vec<String> = Vec::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("engine stderr: {line}");
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
            tail.join("\n")
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(message) => {
                        if events.send(EngineEvent::Message(message)).is_err() {
                            break;
                        }
                    }
                    Err(_) => tracing::warn!("engine printed a line that is not JSON: {line}"),
                }
            }
            let status = child.wait().await;
            let stderr = stderr_tail.await.unwrap_or_default();
            let mut reason = match status {
                Ok(status) => format!("Claude Code exited ({status})"),
                Err(err) => format!("Claude Code exited ({err})"),
            };
            if !stderr.trim().is_empty() {
                reason.push_str(": ");
                reason.push_str(stderr.trim());
            }
            let _ = events.send(EngineEvent::Exited(reason));
        });

        Ok(EngineIo {
            to_engine,
            from_engine,
        })
    }
}

/// Run `<engine command> --version` and return the version number it prints.
pub async fn engine_version(command: &[String]) -> Option<String> {
    let (program, args) = command.split_first()?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(program)
            .args(args)
            .arg("--version")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|part| part.split('.').count() == 3 && part.split('.').all(|n| !n.is_empty()))
        .map(ToString::to_string)
}
