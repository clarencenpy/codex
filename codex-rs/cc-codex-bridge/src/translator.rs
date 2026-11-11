//! Translate Claude Code's stream-json messages into Codex thread, turn and
//! item messages for one thread.
//!
//! The translator is a pure state machine: no I/O, and the clock is passed
//! in. It upholds these rules (see the tests at the bottom of the file and in
//! `tests/`):
//!
//! - an item is started before it completes, and completes at most once;
//! - no delta follows a completed item;
//! - every turn ends with exactly one `turn/completed`;
//! - nothing is sent for a turn after its `turn/completed`.

use crate::items::Outcome;
use crate::items::ToolKind;
use crate::items::agent_message_item;
use crate::items::changed_path;
use crate::items::describe_tool;
use crate::items::finish_tool_item;
use crate::items::start_tool_item;
use crate::items::tool_kind;
use crate::items::user_message_item;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::CommandExecutionApprovalKind;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnError;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_utils_path_uri::LegacyAppPathString;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;

const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// The client's answer to an approval, reduced to what Claude Code can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

/// The approval request to put to the Codex client.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ApprovalRequest {
    Command(CommandExecutionRequestApprovalParams),
    FileChange(FileChangeRequestApprovalParams),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalAsk {
    /// Claude Code's id for its `can_use_tool` request; answer with this.
    pub engine_request_id: String,
    pub request: ApprovalRequest,
}

/// Something for the caller to do.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Out {
    /// Send to subscribed clients.
    Notify(ServerNotification),
    /// Ask a client and hold the request open.
    Approval(ApprovalAsk),
    /// Write to the engine's stdin.
    ToEngine(Value),
    /// The turn is over; this is its final form. Always follows the turn's
    /// `turn/completed` notification.
    TurnEnded(Turn),
    /// An open approval no longer needs an answer.
    ApprovalObsolete { engine_request_id: String },
}

struct PendingApproval {
    tool_use_id: String,
    input: Value,
    suggestions: Value,
}

struct ActiveTurn {
    turn: Turn,
    started_at_ms: i64,
    /// Ids of items that are started and not yet completed.
    open: HashSet<String>,
    /// Tool-call id to (tool name, input).
    tools: HashMap<String, (String, Value)>,
    /// The agent message currently being streamed: item id and text so far.
    streaming: Option<(String, String)>,
    /// Tool calls the client refused.
    declined: HashSet<String>,
    approvals: HashMap<String, PendingApproval>,
    interrupted: bool,
}

pub struct Translator {
    thread_id: String,
    cwd: String,
    turn: Option<ActiveTurn>,
    /// Id of the assistant message currently streaming.
    message_id: Option<String>,
    next_id: u64,
}

impl Translator {
    pub fn new(thread_id: String, cwd: String) -> Self {
        Self {
            thread_id,
            cwd,
            turn: None,
            message_id: None,
            next_id: 0,
        }
    }

    pub fn has_turn(&self) -> bool {
        self.turn.is_some()
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        self.turn.as_ref().map(|active| active.turn.id.as_str())
    }

    /// The turn in progress, as the client should see it right now.
    pub fn active_turn(&self) -> Option<Turn> {
        self.turn.as_ref().map(|active| active.turn.clone())
    }

    pub fn waiting_on_approval(&self) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|active| !active.approvals.is_empty())
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("{}-{}", self.thread_id, self.next_id)
    }

    fn status_out(&self) -> Out {
        let status = match &self.turn {
            None => ThreadStatus::Idle,
            Some(active) => ThreadStatus::Active {
                active_flags: if active.approvals.is_empty() {
                    Vec::new()
                } else {
                    vec![ThreadActiveFlag::WaitingOnApproval]
                },
            },
        };
        Out::Notify(ServerNotification::ThreadStatusChanged(
            ThreadStatusChangedNotification {
                thread_id: self.thread_id.clone(),
                status,
            },
        ))
    }

    // ---- turn lifecycle ---------------------------------------------------

    /// Begin a turn. `input` is the client's input for a turn it asked for;
    /// `None` for a turn Claude Code started by itself (for example when a
    /// background task reports back).
    pub fn start_turn(
        &mut self,
        turn_id: String,
        input: Option<Vec<UserInput>>,
        client_user_message_id: Option<String>,
        now_ms: i64,
    ) -> Vec<Out> {
        let mut out = Vec::new();
        if self.turn.is_some() {
            return out;
        }
        let turn = Turn {
            id: turn_id,
            root_turn_id: None,
            items: Vec::new(),
            items_view: TurnItemsView::Full,
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(now_ms / 1000),
            completed_at: None,
            duration_ms: None,
        };
        out.push(Out::Notify(ServerNotification::TurnStarted(
            TurnStartedNotification {
                thread_id: self.thread_id.clone(),
                turn: turn.clone(),
            },
        )));
        self.turn = Some(ActiveTurn {
            turn,
            started_at_ms: now_ms,
            open: HashSet::new(),
            tools: HashMap::new(),
            streaming: None,
            declined: HashSet::new(),
            approvals: HashMap::new(),
            interrupted: false,
        });
        out.push(self.status_out());
        if let Some(input) = input {
            let id = self.new_id();
            let item = user_message_item(id, input, client_user_message_id, now_ms);
            self.item_started(item.clone(), now_ms, &mut out);
            self.item_completed(item, now_ms, &mut out);
        }
        out
    }

    fn ensure_turn(&mut self, now_ms: i64, out: &mut Vec<Out>) {
        if self.turn.is_none() {
            let id = self.new_id();
            out.extend(self.start_turn(id, None, None, now_ms));
        }
    }

    fn item_started(&mut self, item: ThreadItem, now_ms: i64, out: &mut Vec<Out>) {
        let Some(active) = self.turn.as_mut() else {
            return;
        };
        active.open.insert(item.id().to_string());
        active.turn.items.push(item.clone());
        out.push(Out::Notify(ServerNotification::ItemStarted(
            ItemStartedNotification {
                item,
                thread_id: self.thread_id.clone(),
                turn_id: active.turn.id.clone(),
                started_at_ms: now_ms,
            },
        )));
    }

    fn item_completed(&mut self, mut item: ThreadItem, now_ms: i64, out: &mut Vec<Out>) {
        let Some(active) = self.turn.as_mut() else {
            return;
        };
        if !active.open.remove(item.id()) {
            return; // not open: already completed, or never started
        }
        item.completed_at_ms = Some(now_ms);
        if let Some(slot) = active
            .turn
            .items
            .iter_mut()
            .find(|existing| existing.id() == item.id())
        {
            *slot = item.clone();
        }
        out.push(Out::Notify(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                item,
                thread_id: self.thread_id.clone(),
                turn_id: active.turn.id.clone(),
                content_token_count: None,
                completed_at_ms: now_ms,
            },
        )));
    }

    fn find_item(&self, id: &str) -> Option<ThreadItem> {
        self.turn
            .as_ref()?
            .turn
            .items
            .iter()
            .find(|item| item.id() == id)
            .cloned()
    }

    /// End the turn: close open items, then exactly one `turn/completed`.
    pub fn finish_turn(
        &mut self,
        status: TurnStatus,
        error: Option<String>,
        now_ms: i64,
    ) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(active) = self.turn.as_ref() else {
            return out;
        };
        let completed_ok = status == TurnStatus::Completed;
        let open: Vec<String> = active
            .turn
            .items
            .iter()
            .map(|item| item.id().to_string())
            .filter(|id| active.open.contains(id))
            .collect();
        for id in open {
            let Some(item) = self.find_item(&id) else {
                continue;
            };
            if matches!(item.payload, ThreadItemPayload::AgentMessage { .. }) {
                self.finish_streaming(None, now_ms, &mut out);
                continue;
            }
            let declined = self
                .turn
                .as_ref()
                .is_some_and(|active| active.declined.contains(&id));
            let outcome = if declined {
                Outcome::Declined
            } else if completed_ok {
                Outcome::Completed
            } else {
                Outcome::Failed
            };
            let finished = finish_tool_item(item, outcome, &Value::Null, None, now_ms);
            self.item_completed(finished, now_ms, &mut out);
        }
        let Some(mut active) = self.turn.take() else {
            return out;
        };
        for engine_request_id in active.approvals.into_keys() {
            out.push(Out::ApprovalObsolete { engine_request_id });
        }
        let failed = status == TurnStatus::Failed;
        active.turn.status = status;
        active.turn.completed_at = Some(now_ms / 1000);
        active.turn.duration_ms = Some((now_ms - active.started_at_ms).max(0));
        if failed {
            let error = TurnError {
                message: error.unwrap_or_else(|| "turn failed".to_string()),
                codex_error_info: None,
                additional_details: None,
                misalignment: None,
            };
            active.turn.error = Some(error.clone());
            out.push(Out::Notify(ServerNotification::Error(ErrorNotification {
                error,
                will_retry: false,
                thread_id: self.thread_id.clone(),
                turn_id: active.turn.id.clone(),
            })));
        }
        out.push(Out::Notify(ServerNotification::TurnCompleted(
            TurnCompletedNotification {
                thread_id: self.thread_id.clone(),
                turn: active.turn.clone(),
            },
        )));
        out.push(self.status_out());
        out.push(Out::TurnEnded(active.turn));
        out
    }

    // ---- agent text -------------------------------------------------------

    fn start_streaming(&mut self, id: String, now_ms: i64, out: &mut Vec<Out>) {
        self.finish_streaming(None, now_ms, out);
        self.item_started(
            agent_message_item(id.clone(), String::new(), now_ms, None),
            now_ms,
            out,
        );
        if let Some(active) = self.turn.as_mut() {
            active.streaming = Some((id, String::new()));
        }
    }

    fn delta(&mut self, text: &str, out: &mut Vec<Out>) {
        let Some(active) = self.turn.as_mut() else {
            return;
        };
        let Some((item_id, so_far)) = active.streaming.as_mut() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        so_far.push_str(text);
        out.push(Out::Notify(ServerNotification::AgentMessageDelta(
            AgentMessageDeltaNotification {
                thread_id: self.thread_id.clone(),
                turn_id: active.turn.id.clone(),
                item_id: item_id.clone(),
                delta: text.to_string(),
            },
        )));
    }

    /// Complete the streaming agent message. `full_text` is the finished
    /// text when Claude Code has stated it; any part not yet streamed is sent
    /// as a last delta first.
    fn finish_streaming(&mut self, full_text: Option<&str>, now_ms: i64, out: &mut Vec<Out>) {
        let Some((item_id, so_far)) = self
            .turn
            .as_mut()
            .and_then(|active| active.streaming.take())
        else {
            return;
        };
        let text = match full_text {
            Some(full) => {
                if let Some(rest) = full.strip_prefix(so_far.as_str()) {
                    if let Some(active) = self.turn.as_mut() {
                        active.streaming = Some((item_id.clone(), so_far.clone()));
                    }
                    self.delta(rest, out);
                    if let Some(active) = self.turn.as_mut() {
                        active.streaming = None;
                    }
                }
                full.to_string()
            }
            None => so_far,
        };
        let started = self
            .find_item(&item_id)
            .and_then(|item| item.started_at_ms)
            .unwrap_or(now_ms);
        self.item_completed(
            agent_message_item(item_id, text, started, Some(now_ms)),
            now_ms,
            out,
        );
    }

    // ---- tools ------------------------------------------------------------

    fn tool_started(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        now_ms: i64,
        out: &mut Vec<Out>,
    ) {
        if self.find_item(id).is_some() {
            return; // one item per tool-call id
        }
        self.finish_streaming(None, now_ms, out);
        let item = start_tool_item(id, name, input, &self.cwd, now_ms);
        if let Some(active) = self.turn.as_mut() {
            active
                .tools
                .insert(id.to_string(), (name.to_string(), input.clone()));
        }
        self.item_started(item, now_ms, out);
    }

    fn tool_finished(
        &mut self,
        id: &str,
        is_error: bool,
        content: &Value,
        structured: Option<&Value>,
        now_ms: i64,
        out: &mut Vec<Out>,
    ) {
        let Some(active) = self.turn.as_ref() else {
            return;
        };
        if !active.open.contains(id) {
            return;
        }
        let outcome = if active.declined.contains(id) {
            Outcome::Declined
        } else if is_error {
            Outcome::Failed
        } else {
            Outcome::Completed
        };
        let Some(item) = self.find_item(id) else {
            return;
        };
        let structured = structured.filter(|_| !is_error);
        let finished = finish_tool_item(item, outcome, content, structured, now_ms);
        self.item_completed(finished, now_ms, out);
    }

    fn permission_requested(
        &mut self,
        engine_request_id: &str,
        request: &Value,
        now_ms: i64,
        out: &mut Vec<Out>,
    ) {
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let tool_use_id = match request.get("tool_use_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => self.new_id(),
        };
        self.tool_started(&tool_use_id, &tool_name, &input, now_ms, out);
        let Some(item) = self.find_item(&tool_use_id) else {
            return;
        };
        let Some(active) = self.turn.as_mut() else {
            return;
        };
        active.approvals.insert(
            engine_request_id.to_string(),
            PendingApproval {
                tool_use_id: tool_use_id.clone(),
                input: input.clone(),
                suggestions: request
                    .get("permission_suggestions")
                    .cloned()
                    .unwrap_or(Value::Null),
            },
        );
        let turn_id = active.turn.id.clone();
        let kind = tool_kind(&tool_name);
        let request = if kind == ToolKind::File {
            ApprovalRequest::FileChange(FileChangeRequestApprovalParams {
                thread_id: self.thread_id.clone(),
                turn_id,
                item_id: tool_use_id,
                started_at_ms: now_ms,
                reason: Some(format!(
                    "Claude Code wants to change {}",
                    changed_path(&item).unwrap_or_else(|| "a file".to_string())
                )),
                grant_root: None,
            })
        } else {
            let reason = if kind == ToolKind::Command {
                input
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            } else {
                Some(format!("Claude Code wants to use the {tool_name} tool"))
            };
            ApprovalRequest::Command(CommandExecutionRequestApprovalParams {
                kind: CommandExecutionApprovalKind::Command,
                thread_id: self.thread_id.clone(),
                turn_id,
                item_id: tool_use_id,
                started_at_ms: now_ms,
                approval_id: None,
                environment_id: None,
                reason,
                network_approval_context: None,
                command: Some(describe_tool(&tool_name, &input)),
                cwd: Some(LegacyAppPathString::from_string(self.cwd.clone())),
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            })
        };
        out.push(self.status_out());
        out.push(Out::Approval(ApprovalAsk {
            engine_request_id: engine_request_id.to_string(),
            request,
        }));
    }

    /// The client answered an approval: tell Claude Code.
    pub fn approval_answered(&mut self, engine_request_id: &str, decision: Decision) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(active) = self.turn.as_mut() else {
            return out;
        };
        let Some(pending) = active.approvals.remove(engine_request_id) else {
            return out;
        };
        let response = match decision {
            Decision::Accept => json!({ "behavior": "allow", "updatedInput": pending.input }),
            Decision::AcceptForSession => {
                // Apply Claude Code's own suggested rule, but only for this
                // session: never write it to the user's settings files.
                let rules: Vec<Value> = pending
                    .suggestions
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|rule| {
                        rule.get("type").and_then(Value::as_str) != Some("setMode") || {
                            rule.get("mode").and_then(Value::as_str) == Some("acceptEdits")
                        }
                    })
                    .map(|rule| {
                        let mut rule = rule.clone();
                        if let Some(fields) = rule.as_object_mut() {
                            fields.insert("destination".to_string(), json!("session"));
                        }
                        rule
                    })
                    .collect();
                json!({ "behavior": "allow", "updatedInput": pending.input, "updatedPermissions": rules })
            }
            Decision::Decline => {
                active.declined.insert(pending.tool_use_id);
                json!({ "behavior": "deny", "message": "The user declined this action." })
            }
            Decision::Cancel => {
                active.declined.insert(pending.tool_use_id);
                active.interrupted = true;
                json!({ "behavior": "deny", "message": "The user cancelled this action.", "interrupt": true })
            }
        };
        out.push(Out::ToEngine(json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": engine_request_id, "response": response },
        })));
        out.push(self.status_out());
        out
    }

    /// Ask Claude Code to stop the turn. The turn ends when it reports back.
    pub fn request_interrupt(&mut self) -> Vec<Out> {
        let Some(active) = self.turn.as_mut() else {
            return Vec::new();
        };
        active.interrupted = true;
        let id = self.new_id();
        vec![Out::ToEngine(json!({
            "type": "control_request",
            "request_id": format!("interrupt-{id}"),
            "request": { "subtype": "interrupt" },
        }))]
    }

    /// The engine process is gone.
    pub fn engine_exited(&mut self, reason: &str, now_ms: i64) -> Vec<Out> {
        let Some(active) = self.turn.as_ref() else {
            return Vec::new();
        };
        if active.interrupted {
            self.finish_turn(TurnStatus::Interrupted, None, now_ms)
        } else {
            self.finish_turn(TurnStatus::Failed, Some(reason.to_string()), now_ms)
        }
    }

    // ---- engine messages --------------------------------------------------

    /// Handle one stream-json message from Claude Code. Unknown shapes are
    /// ignored: the format is not a stable contract.
    pub fn on_engine_message(&mut self, message: &Value, now_ms: i64) -> Vec<Out> {
        let mut out = Vec::new();
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // Messages from subagents carry the parent tool call's id; only the
        // main conversation is shown.
        let from_subagent = message
            .get("parent_tool_use_id")
            .is_some_and(|parent| !parent.is_null());
        match kind {
            "stream_event" if !from_subagent => self.on_stream_event(message, now_ms, &mut out),
            "assistant" if !from_subagent => self.on_assistant(message, now_ms, &mut out),
            "user" if !from_subagent => self.on_user(message, now_ms, &mut out),
            "control_request" => self.on_control_request(message, now_ms, &mut out),
            "result" => {
                if let Some(active) = self.turn.as_ref() {
                    let failed = message.get("is_error").and_then(Value::as_bool) == Some(true);
                    if active.interrupted {
                        out.extend(self.finish_turn(TurnStatus::Interrupted, None, now_ms));
                    } else if failed {
                        out.extend(self.finish_turn(
                            TurnStatus::Failed,
                            Some(describe_failure(message)),
                            now_ms,
                        ));
                    } else {
                        out.extend(self.finish_turn(TurnStatus::Completed, None, now_ms));
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn on_stream_event(&mut self, message: &Value, now_ms: i64, out: &mut Vec<Out>) {
        let Some(event) = message.get("event") else {
            return;
        };
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                self.message_id = event
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
            }
            "content_block_start"
                if event.pointer("/content_block/type").and_then(Value::as_str) == Some("text") =>
            {
                self.ensure_turn(now_ms, out);
                let index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
                let id = match &self.message_id {
                    Some(message_id) => format!("{message_id}-{index}"),
                    None => self.new_id(),
                };
                self.start_streaming(id, now_ms, out);
            }
            "content_block_delta" => {
                if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str)
                    && event.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                {
                    self.delta(text, out);
                }
            }
            _ => {}
        }
    }

    fn on_assistant(&mut self, message: &Value, now_ms: i64, out: &mut Vec<Out>) {
        let Some(blocks) = message
            .pointer("/message/content")
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in blocks {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        continue;
                    }
                    self.ensure_turn(now_ms, out);
                    let streaming = self
                        .turn
                        .as_ref()
                        .is_some_and(|active| active.streaming.is_some());
                    if !streaming {
                        // No partial messages were streamed: one delta
                        // carries the whole text.
                        let id = self.new_id();
                        self.start_streaming(id, now_ms, out);
                    }
                    self.finish_streaming(Some(text), now_ms, out);
                }
                "tool_use" => {
                    let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    self.ensure_turn(now_ms, out);
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    self.tool_started(id, name, &input, now_ms, out);
                }
                _ => {}
            }
        }
    }

    fn on_user(&mut self, message: &Value, now_ms: i64, out: &mut Vec<Out>) {
        let Some(content) = message.pointer("/message/content") else {
            return;
        };
        if let Some(text) = content.as_str() {
            self.note_interrupt_marker(text);
            return;
        }
        for block in content.as_array().into_iter().flatten() {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "tool_result" => {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                    let content = block.get("content").cloned().unwrap_or(Value::Null);
                    self.tool_finished(
                        id,
                        is_error,
                        &content,
                        message.get("tool_use_result"),
                        now_ms,
                        out,
                    );
                }
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.note_interrupt_marker(text);
                    }
                }
                _ => {}
            }
        }
    }

    fn note_interrupt_marker(&mut self, text: &str) {
        if text.starts_with(INTERRUPT_MARKER)
            && let Some(active) = self.turn.as_mut()
        {
            active.interrupted = true;
        }
    }

    fn on_control_request(&mut self, message: &Value, now_ms: i64, out: &mut Vec<Out>) {
        let Some(request_id) = message.get("request_id").and_then(Value::as_str) else {
            return;
        };
        let request = message.get("request").cloned().unwrap_or(Value::Null);
        if request.get("subtype").and_then(Value::as_str) == Some("can_use_tool") {
            self.ensure_turn(now_ms, out);
            self.permission_requested(request_id, &request, now_ms, out);
            return;
        }
        // Answer anything else so Claude Code is not left waiting.
        out.push(Out::ToEngine(json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": request_id,
                "error": "cc-codex-bridge does not support this control request",
            },
        })));
    }
}

fn describe_failure(result: &Value) -> String {
    let detail = result
        .get("result")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            result
                .get("errors")
                .filter(|errors| !errors.is_null())
                .map(Value::to_string)
        })
        .or_else(|| {
            result
                .get("subtype")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
    match detail {
        Some(detail) if !detail.is_empty() => format!("Claude Code reported an error: {detail}"),
        _ => "Claude Code reported an error".to_string(),
    }
}
