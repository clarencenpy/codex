//! Test support: a scripted stand-in for Claude Code's stream-json process,
//! and a client that checks every message the bridge sends.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::collapsible_match
)]

use codex_app_server_protocol::ServerNotification;
use codex_cc_bridge::config::Config;
use codex_cc_bridge::config::ModelConfig;
use codex_cc_bridge::engine::EngineEvent;
use codex_cc_bridge::engine::EngineIo;
use codex_cc_bridge::engine::EngineLauncher;
use codex_cc_bridge::engine::LaunchSpec;
use codex_cc_bridge::server::Bridge;
use codex_cc_bridge::server::ConnectionId;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// What the fake engine saw, for assertions.
#[derive(Default)]
pub struct EngineLog {
    pub launches: Vec<LaunchSpec>,
    pub prompts: Vec<String>,
    pub permission_responses: Vec<Value>,
}

/// A scripted Claude Code. What it does for a prompt is picked by keywords:
///
/// - `run: <cmd>`   Bash tool call behind a permission request
/// - `edit: <path>` Edit tool call behind a permission request
/// - `mcp`          an MCP tool call, no permission request
/// - `slow`         keeps going until interrupted
/// - `die`          the process exits mid-turn
/// - `fail`         the turn ends with an error result
/// - anything else  replies `echo: <prompt>`, streamed in two deltas
#[derive(Default)]
pub struct FakeLauncher {
    pub log: Arc<Mutex<EngineLog>>,
}

impl EngineLauncher for FakeLauncher {
    fn launch(&self, spec: &LaunchSpec) -> anyhow::Result<EngineIo> {
        self.log.lock().unwrap().launches.push(spec.clone());
        let (to_engine, stdin) = mpsc::unbounded_channel::<Value>();
        let (stdout, from_engine) = mpsc::unbounded_channel::<EngineEvent>();
        tokio::spawn(fake_claude(stdin, stdout, self.log.clone()));
        Ok(EngineIo {
            to_engine,
            from_engine,
        })
    }
}

struct Fake {
    stdin: mpsc::UnboundedReceiver<Value>,
    stdout: mpsc::UnboundedSender<EngineEvent>,
    log: Arc<Mutex<EngineLog>>,
    counter: u64,
}

impl Fake {
    fn say(&self, message: Value) {
        let _ = self.stdout.send(EngineEvent::Message(message));
    }

    fn id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}_{}", self.counter)
    }

    /// Stream an assistant text message the way Claude Code does with
    /// `--include-partial-messages`.
    fn text(&mut self, text: &str) {
        let message_id = self.id("msg");
        let (first, second) = text.split_at(text.len() / 2);
        self.say(json!({"type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "message_start", "message": {"id": message_id}}}));
        self.say(json!({"type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}));
        for part in [first, second] {
            self.say(json!({"type": "stream_event", "parent_tool_use_id": null,
                "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": part}}}));
        }
        self.say(json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"id": message_id, "role": "assistant", "content": [{"type": "text", "text": text}]}}));
        self.say(json!({"type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_stop", "index": 0}}));
    }

    fn result(&self, is_error: bool) {
        self.say(json!({"type": "result", "subtype": if is_error { "error_during_execution" } else { "success" },
            "is_error": is_error, "result": if is_error { "the model is overloaded" } else { "" }}));
    }

    /// Wait for stdin. `None` means stdin closed. An interrupt request is
    /// answered and reported as `Err`.
    async fn read(&mut self) -> Option<Result<Value, ()>> {
        let message = self.stdin.recv().await?;
        if message["type"] == "control_request" && message["request"]["subtype"] == "interrupt" {
            self.say(
                json!({"type": "control_response", "response": {"subtype": "success",
                "request_id": message["request_id"], "response": {"still_queued": []}}}),
            );
            self.say(json!({"type": "user", "parent_tool_use_id": null,
                "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}}));
            self.result(false);
            return Some(Err(()));
        }
        Some(Ok(message))
    }

    /// Run one tool call. Returns false when the turn must stop.
    async fn tool(
        &mut self,
        name: &str,
        input: Value,
        structured: Value,
        result_text: &str,
        ask: bool,
    ) -> Option<bool> {
        let tool_use_id = self.id("toolu");
        self.say(json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}]}}));
        if ask {
            let request_id = self.id("req");
            let suggestions = if name == "Bash" {
                json!([{"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "git *"}], "behavior": "allow", "destination": "localSettings"}])
            } else {
                json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}])
            };
            self.say(
                json!({"type": "control_request", "request_id": request_id, "request": {
                "subtype": "can_use_tool", "tool_name": name, "input": input,
                "permission_suggestions": suggestions, "tool_use_id": tool_use_id}}),
            );
            let answer = match self.read().await? {
                Ok(answer) => answer,
                Err(()) => return Some(false),
            };
            assert_eq!(answer["type"], "control_response");
            assert_eq!(answer["response"]["request_id"], request_id);
            let response = answer["response"]["response"].clone();
            self.log
                .lock()
                .unwrap()
                .permission_responses
                .push(response.clone());
            if response["behavior"] != "allow" {
                self.say(json!({"type": "user", "parent_tool_use_id": null, "tool_use_result": "Error: declined",
                    "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id,
                        "content": response["message"], "is_error": true}]}}));
                if response["interrupt"] == true {
                    self.say(json!({"type": "user", "parent_tool_use_id": null,
                        "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}));
                    self.result(false);
                    return Some(false);
                }
                self.text("understood, I will not do that");
                self.result(false);
                return Some(false);
            }
        }
        self.say(json!({"type": "user", "parent_tool_use_id": null, "tool_use_result": structured,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": result_text}]}}));
        Some(true)
    }

    async fn turn(&mut self, prompt: &str) -> Option<()> {
        self.say(json!({"type": "system", "subtype": "init"}));
        if prompt.contains("die") {
            return None;
        }
        if prompt.contains("fail") {
            self.result(true);
            return Some(());
        }
        if prompt.contains("slow") {
            self.text("working on it");
            loop {
                if self.read().await?.is_err() {
                    return Some(());
                }
            }
        }
        let mut used_tool = false;
        if let Some(command) = prompt
            .split("run: ")
            .nth(1)
            .map(|rest| rest.lines().next().unwrap_or(rest).trim())
        {
            used_tool = true;
            let output = format!("ran {command}");
            let proceed = self
                .tool(
                    "Bash",
                    json!({"command": command, "description": "Run the command"}),
                    json!({"stdout": output, "stderr": "", "interrupted": false}),
                    &output,
                    true,
                )
                .await?;
            if !proceed {
                return Some(());
            }
        }
        if let Some(path) = prompt
            .split("edit: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
        {
            used_tool = true;
            let proceed = self
                .tool("Edit", json!({"file_path": path, "old_string": "hi", "new_string": "hello", "replace_all": false}),
                    json!({"filePath": path, "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-hi", "+hello"]}]}),
                    "The file has been updated successfully.", true)
                .await?;
            if !proceed {
                return Some(());
            }
        }
        if prompt.contains("mcp") {
            used_tool = true;
            self.tool(
                "mcp__files__search",
                json!({"query": "x"}),
                json!([{"type": "text", "text": "found"}]),
                "found",
                false,
            )
            .await?;
        }
        let text = if used_tool {
            "done".to_string()
        } else {
            format!("echo: {prompt}")
        };
        self.text(&text);
        self.result(false);
        Some(())
    }
}

async fn fake_claude(
    stdin: mpsc::UnboundedReceiver<Value>,
    stdout: mpsc::UnboundedSender<EngineEvent>,
    log: Arc<Mutex<EngineLog>>,
) {
    let mut fake = Fake {
        stdin,
        stdout,
        log,
        counter: 0,
    };
    loop {
        let Some(message) = fake.read().await else {
            break;
        };
        let Ok(message) = message else {
            continue; // an interrupt with nothing running
        };
        if message["type"] != "user" {
            continue;
        }
        let prompt = message["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        fake.log.lock().unwrap().prompts.push(prompt.clone());
        if fake.turn(&prompt).await.is_none() {
            break;
        }
    }
    let _ = fake.stdout.send(EngineEvent::Exited(
        "Claude Code exited (exit status: 1)".to_string(),
    ));
}

pub fn test_config(state_dir: &Path) -> Config {
    Config {
        engine_command: vec!["claude".to_string()],
        models: vec![
            ModelConfig {
                id: "sonnet".to_string(),
                display_name: None,
                description: None,
                is_default: false,
            },
            ModelConfig {
                id: "opus".to_string(),
                display_name: Some("Claude Opus".to_string()),
                description: None,
                is_default: true,
            },
        ],
        state_dir: state_dir.to_path_buf(),
        tested_claude_version: "2.1.322".to_string(),
        interrupt_timeout_ms: 500,
    }
}

/// Checks the ordering rules over the notifications one client receives.
#[derive(Default)]
pub struct Invariants {
    items: HashMap<String, &'static str>,
    turns: HashMap<String, &'static str>,
    pub errors: Vec<String>,
}

impl Invariants {
    fn observe(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::TurnStarted(n) => {
                if self.turns.insert(n.turn.id.clone(), "started").is_some() {
                    self.errors
                        .push(format!("turn {} started twice", n.turn.id));
                }
            }
            ServerNotification::TurnCompleted(n) => {
                if self.turns.insert(n.turn.id.clone(), "ended") != Some("started") {
                    self.errors.push(format!(
                        "turn {} ended without being in progress",
                        n.turn.id
                    ));
                }
                for item in &n.turn.items {
                    if self.items.get(item.id()) != Some(&"completed") {
                        self.errors
                            .push(format!("turn ended with item {} not completed", item.id()));
                    }
                }
            }
            ServerNotification::ItemStarted(n) => {
                if self
                    .items
                    .insert(n.item.id().to_string(), "started")
                    .is_some()
                {
                    self.errors
                        .push(format!("item {} started twice", n.item.id()));
                }
                if self.turns.get(&n.turn_id) != Some(&"started") {
                    self.errors.push(format!(
                        "item {} started outside a running turn",
                        n.item.id()
                    ));
                }
            }
            ServerNotification::ItemCompleted(n) => {
                if self.items.insert(n.item.id().to_string(), "completed") != Some("started") {
                    self.errors
                        .push(format!("item {} completed without being open", n.item.id()));
                }
                if self.turns.get(&n.turn_id) != Some(&"started") {
                    self.errors.push(format!(
                        "item {} completed outside a running turn",
                        n.item.id()
                    ));
                }
            }
            ServerNotification::AgentMessageDelta(n) => {
                if self.items.get(&n.item_id) != Some(&"started") {
                    self.errors
                        .push(format!("delta for item {} that is not open", n.item_id));
                }
            }
            _ => {}
        }
    }
}

/// A client attached straight to the bridge.
pub struct Client {
    pub bridge: Arc<Bridge>,
    pub connection: ConnectionId,
    incoming: mpsc::UnboundedReceiver<Value>,
    /// Everything received so far, in order.
    pub messages: Vec<Value>,
    pub invariants: Invariants,
    next_id: i64,
    /// How to answer approval requests while waiting for something else.
    pub decision: Option<&'static str>,
}

impl Client {
    pub fn connect(bridge: &Arc<Bridge>) -> Self {
        let (outgoing, incoming) = mpsc::unbounded_channel();
        Self {
            bridge: bridge.clone(),
            connection: bridge.connect(outgoing),
            incoming,
            messages: Vec::new(),
            invariants: Invariants::default(),
            next_id: 0,
            decision: None,
        }
    }

    pub async fn send(&self, message: Value) {
        self.bridge.handle_message(self.connection, message).await;
    }

    async fn next(&mut self) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(10), self.incoming.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out; last messages: {:#?}",
                    self.messages.iter().rev().take(5).collect::<Vec<_>>()
                )
            })
            .expect("bridge closed the connection");
        if message.get("method").is_some() && message.get("id").is_none() {
            // Every notification must be one the protocol crate knows.
            let typed: ServerNotification = serde_json::from_value(message.clone())
                .unwrap_or_else(|err| panic!("not a valid ServerNotification ({err}): {message}"));
            self.invariants.observe(&typed);
        }
        if message.get("method").is_some()
            && message.get("id").is_some()
            && let Some(decision) = self.decision
        {
            self.send(json!({"id": message["id"], "result": {"decision": decision}}))
                .await;
        }
        self.messages.push(message.clone());
        message
    }

    /// Receive until a message matches.
    pub async fn wait_for(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            let message = self.next().await;
            if matches(&message) {
                return message;
            }
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next_id += 1;
        let id = self.next_id + 1_000_000; // keep clear of the bridge's own request ids
        self.send(json!({"id": id, "method": method, "params": params}))
            .await;
        let reply = self
            .wait_for(|message| message["id"] == id && message.get("method").is_none())
            .await;
        match reply.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(reply["result"].clone()),
        }
    }

    pub async fn initialize(&mut self) {
        self.request("initialize", json!({"clientInfo": {"name": "test", "title": null, "version": "0"}, "capabilities": null}))
            .await
            .expect("initialize");
        self.send(json!({"method": "initialized"})).await;
    }

    pub async fn start_thread(&mut self, cwd: &Path) -> String {
        let reply = self
            .request("thread/start", json!({"cwd": cwd}))
            .await
            .expect("thread/start");
        reply["thread"]["id"]
            .as_str()
            .expect("thread id")
            .to_string()
    }

    pub async fn start_turn(&mut self, thread_id: &str, prompt: &str) -> String {
        let reply = self
            .request("turn/start", json!({"threadId": thread_id, "input": [{"type": "text", "text": prompt, "text_elements": []}]}))
            .await
            .expect("turn/start");
        reply["turn"]["id"].as_str().expect("turn id").to_string()
    }

    /// Wait for a turn to end and return it.
    pub async fn turn_completed(&mut self, turn_id: &str) -> Value {
        if let Some(done) = self.messages.iter().find(|message| {
            message["method"] == "turn/completed" && message["params"]["turn"]["id"] == turn_id
        }) {
            return done["params"]["turn"].clone();
        }
        let done = self
            .wait_for(|message| {
                message["method"] == "turn/completed" && message["params"]["turn"]["id"] == turn_id
            })
            .await;
        done["params"]["turn"].clone()
    }

    pub async fn run_turn(&mut self, thread_id: &str, prompt: &str) -> Value {
        let turn_id = self.start_turn(thread_id, prompt).await;
        self.turn_completed(&turn_id).await
    }

    pub fn notifications(&self, method: &str) -> Vec<&Value> {
        self.messages
            .iter()
            .filter(|message| message["method"] == method)
            .collect()
    }

    pub fn assert_ordered(&self) {
        assert_eq!(self.invariants.errors, Vec::<String>::new());
    }
}

/// `status: item(status), item, ...` for a turn.
pub fn shape(turn: &Value) -> String {
    let items: Vec<String> = turn["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| match item["status"].as_str() {
            Some(status) => format!("{}({status})", item["type"].as_str().unwrap_or("?")),
            None => item["type"].as_str().unwrap_or("?").to_string(),
        })
        .collect();
    format!(
        "{}: {}",
        turn["status"].as_str().unwrap_or("?"),
        items.join(", ")
    )
}

pub fn new_bridge(state_dir: &Path) -> (Arc<Bridge>, Arc<Mutex<EngineLog>>) {
    let launcher = Arc::new(FakeLauncher::default());
    let log = launcher.log.clone();
    let bridge = Bridge::new(test_config(state_dir), launcher, Vec::new()).expect("bridge");
    (bridge, log)
}
