//! One actor per loaded thread. It owns the thread's Claude Code process and
//! is the only writer to it: prompts, approval answers and interrupts all go
//! through here, and the engine's messages come back through here into the
//! translator.

use crate::engine::EngineEvent;
use crate::engine::EngineIo;
use crate::engine::EngineLauncher;
use crate::engine::LaunchSpec;
use crate::store::Store;
use crate::translator::ApprovalAsk;
use crate::translator::Decision;
use crate::translator::Out;
use crate::translator::Translator;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use serde_json::Value;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// What a thread tells the server.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ThreadEvent {
    Notify(ServerNotification),
    Approval(ApprovalAsk),
    ApprovalObsolete { engine_request_id: String },
}

pub enum Command {
    StartTurn {
        turn_id: String,
        input: Vec<UserInput>,
        client_user_message_id: Option<String>,
    },
    Interrupt {
        turn_id: String,
    },
    Approval {
        engine_request_id: String,
        decision: Decision,
    },
    /// Stop the engine and end the actor.
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// What the server can read about a thread without asking its actor.
#[derive(Debug, Clone)]
pub struct Live {
    pub status: ThreadStatus,
    pub active_turn: Option<Turn>,
    pub open_approvals: Vec<ApprovalAsk>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            status: ThreadStatus::Idle,
            active_turn: None,
            open_approvals: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct ThreadHandle {
    pub commands: mpsc::UnboundedSender<Command>,
    pub live: Arc<Mutex<Live>>,
}

impl ThreadHandle {
    pub fn live(&self) -> Live {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub struct SessionDeps {
    pub store: Arc<Store>,
    pub launcher: Arc<dyn EngineLauncher>,
    pub interrupt_timeout: Duration,
    /// Receives everything the thread wants sent to clients.
    pub events: mpsc::UnboundedSender<(String, ThreadEvent)>,
}

struct QueuedTurn {
    turn_id: String,
    input: Vec<UserInput>,
    client_user_message_id: Option<String>,
    cancelled: bool,
}

struct Session {
    thread_id: String,
    deps: SessionDeps,
    translator: Translator,
    engine: Option<EngineIo>,
    queue: VecDeque<QueuedTurn>,
    live: Arc<Mutex<Live>>,
    open_approvals: Vec<ApprovalAsk>,
    /// When an unconfirmed interrupt gives up and stops the engine.
    interrupt_deadline: Option<tokio::time::Instant>,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The turn a client is told about when it asks for one: it starts once the
/// session is free.
pub fn queued_turn(turn_id: &str) -> Turn {
    Turn {
        id: turn_id.to_string(),
        root_turn_id: None,
        items: Vec::new(),
        items_view: TurnItemsView::Full,
        status: TurnStatus::InProgress,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

/// Start the actor for a thread.
pub fn spawn(thread_id: String, cwd: String, deps: SessionDeps) -> ThreadHandle {
    let (commands, receiver) = mpsc::unbounded_channel();
    let live = Arc::new(Mutex::new(Live::default()));
    let session = Session {
        translator: Translator::new(thread_id.clone(), cwd),
        thread_id,
        deps,
        engine: None,
        queue: VecDeque::new(),
        live: live.clone(),
        open_approvals: Vec::new(),
        interrupt_deadline: None,
    };
    tokio::spawn(session.run(receiver));
    ThreadHandle { commands, live }
}

impl Session {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        loop {
            let deadline = self.interrupt_deadline;
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(Command::Shutdown { done }) => {
                            let out = self.translator.finish_turn(TurnStatus::Interrupted, None, now_ms());
                            self.apply(out);
                            self.engine = None;
                            let _ = done.send(());
                            return;
                        }
                        Some(command) => self.on_command(command),
                        None => return,
                    }
                }
                event = next_engine_event(&mut self.engine) => {
                    match event {
                        Some(EngineEvent::Message(message)) => {
                            let out = self.translator.on_engine_message(&message, now_ms());
                            self.apply(out);
                        }
                        Some(EngineEvent::Exited(reason)) => self.engine_gone(&reason),
                        None => self.engine_gone("Claude Code exited"),
                    }
                }
                () = sleep_until(deadline) => {
                    // The interrupt was never confirmed: stop the process.
                    // The session resumes on the next turn.
                    tracing::warn!(thread = %self.thread_id, "interrupt not confirmed; stopping the engine");
                    self.engine = None;
                    let out = self.translator.finish_turn(TurnStatus::Interrupted, None, now_ms());
                    self.apply(out);
                }
            }
            self.dispatch();
            self.publish();
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::StartTurn {
                turn_id,
                input,
                client_user_message_id,
            } => self.queue.push_back(QueuedTurn {
                turn_id,
                input,
                client_user_message_id,
                cancelled: false,
            }),
            Command::Interrupt { turn_id } => {
                if let Some(queued) = self
                    .queue
                    .iter_mut()
                    .find(|queued| queued.turn_id == turn_id)
                {
                    queued.cancelled = true;
                } else if self.translator.active_turn_id() == Some(turn_id.as_str()) {
                    let out = self.translator.request_interrupt();
                    self.apply(out);
                    self.interrupt_deadline =
                        Some(tokio::time::Instant::now() + self.deps.interrupt_timeout);
                }
            }
            Command::Approval {
                engine_request_id,
                decision,
            } => {
                self.open_approvals
                    .retain(|ask| ask.engine_request_id != engine_request_id);
                let out = self
                    .translator
                    .approval_answered(&engine_request_id, decision);
                self.apply(out);
            }
            Command::Shutdown { .. } => {}
        }
    }

    fn engine_gone(&mut self, reason: &str) {
        self.engine = None;
        let out = self.translator.engine_exited(reason, now_ms());
        self.apply(out);
    }

    /// Send the next queued prompt once no turn is running.
    fn dispatch(&mut self) {
        while !self.translator.has_turn() {
            let Some(next) = self.queue.pop_front() else {
                return;
            };
            let now = now_ms();
            let text = prompt_text(&next.input);
            let out = self.translator.start_turn(
                next.turn_id,
                Some(next.input),
                next.client_user_message_id,
                now,
            );
            self.apply(out);
            if next.cancelled {
                let out = self
                    .translator
                    .finish_turn(TurnStatus::Interrupted, None, now);
                self.apply(out);
                continue;
            }
            if let Err(err) = self.send_prompt(&text) {
                let out = self.translator.finish_turn(
                    TurnStatus::Failed,
                    Some(format!("could not send the prompt to Claude Code: {err}")),
                    now_ms(),
                );
                self.apply(out);
            }
        }
    }

    fn send_prompt(&mut self, text: &str) -> anyhow::Result<()> {
        if self.engine.is_none() {
            let record = self
                .deps
                .store
                .get(&self.thread_id)
                .ok_or_else(|| anyhow::anyhow!("thread record is missing"))?;
            let engine = self.deps.launcher.launch(&LaunchSpec {
                cwd: record.cwd,
                session_id: record.claude_session_id,
                resume: record.started,
                model: record.model,
            })?;
            self.engine = Some(engine);
            self.deps
                .store
                .update(&self.thread_id, |record| record.started = true)?;
        }
        let message = json!({ "type": "user", "message": { "role": "user", "content": text } });
        self.send_to_engine(message)
    }

    fn send_to_engine(&mut self, message: Value) -> anyhow::Result<()> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Claude Code is not running"))?;
        engine
            .to_engine
            .send(message)
            .map_err(|_| anyhow::anyhow!("Claude Code is not accepting input"))
    }

    fn emit(&self, event: ThreadEvent) {
        let _ = self.deps.events.send((self.thread_id.clone(), event));
    }

    fn apply(&mut self, out: Vec<Out>) {
        for step in out {
            match step {
                Out::Notify(notification) => {
                    if let ServerNotification::ItemCompleted(completed) = &notification
                        && let ThreadItemPayload::UserMessage { content, .. } =
                            &completed.item.payload
                    {
                        let preview = preview_of(content);
                        let _ = self.deps.store.update(&self.thread_id, |record| {
                            if record.preview.is_empty() {
                                record.preview = preview;
                            }
                        });
                    }
                    self.emit(ThreadEvent::Notify(notification));
                }
                Out::Approval(ask) => {
                    self.open_approvals.push(ask.clone());
                    self.emit(ThreadEvent::Approval(ask));
                }
                Out::ApprovalObsolete { engine_request_id } => {
                    self.open_approvals
                        .retain(|ask| ask.engine_request_id != engine_request_id);
                    self.emit(ThreadEvent::ApprovalObsolete { engine_request_id });
                }
                Out::ToEngine(message) => {
                    if let Err(err) = self.send_to_engine(message) {
                        tracing::warn!(thread = %self.thread_id, "dropping a message for the engine: {err}");
                    }
                }
                Out::TurnEnded(turn) => {
                    self.interrupt_deadline = None;
                    if let Err(err) = self.deps.store.append_turn(&self.thread_id, &turn) {
                        tracing::error!(thread = %self.thread_id, "could not record the turn: {err:#}");
                    }
                    let _ = self.deps.store.update(&self.thread_id, |record| {
                        record.updated_at = now_ms() / 1000
                    });
                }
            }
        }
    }

    /// Refresh what the server reads without asking, and keep the running
    /// turn on disk.
    fn publish(&mut self) {
        let active_turn = self.translator.active_turn();
        if let Some(turn) = &active_turn
            && let Err(err) = self.deps.store.save_active_turn(&self.thread_id, turn)
        {
            tracing::error!(thread = %self.thread_id, "could not save the running turn: {err:#}");
        }
        let status = if active_turn.is_none() && self.queue.is_empty() {
            ThreadStatus::Idle
        } else {
            ThreadStatus::Active {
                active_flags: if self.translator.waiting_on_approval() {
                    vec![ThreadActiveFlag::WaitingOnApproval]
                } else {
                    Vec::new()
                },
            }
        };
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live = Live {
            status,
            active_turn,
            open_approvals: self.open_approvals.clone(),
        };
    }
}

async fn next_engine_event(engine: &mut Option<EngineIo>) -> Option<EngineEvent> {
    match engine {
        Some(engine) => engine.from_engine.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Flatten Codex user input into the text sent to Claude Code.
pub fn prompt_text(input: &[UserInput]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in input {
        match item {
            UserInput::Text { text, .. } => parts.push(text.clone()),
            UserInput::LocalImage { path, .. } | UserInput::LocalAudio { path } => {
                parts.push(path.to_string_lossy().into_owned());
            }
            UserInput::Skill { path, .. } => parts.push(format!("@{}", path.to_string_lossy())),
            UserInput::Mention { path, .. } => parts.push(format!("@{path}")),
            UserInput::Image { .. } | UserInput::Audio { .. } => {
                parts.push("[attachment omitted: cc-codex-bridge forwards text only]".to_string());
            }
        }
    }
    let text = parts.join("\n").replace("\r\n", "\n").trim().to_string();
    // A leading "/" would make Claude Code treat the prompt as one of its own
    // commands. A prompt from the Codex client is always plain text.
    if text.starts_with('/') {
        format!(" {text}")
    } else {
        text
    }
}

fn preview_of(input: &[UserInput]) -> String {
    let text = input
        .iter()
        .find_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}
