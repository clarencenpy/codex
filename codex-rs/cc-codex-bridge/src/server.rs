//! The Codex app-server facade: the only part a Codex client sees. It
//! handles the client requests the bridge implements and answers everything
//! else with a JSON-RPC method-not-found error.

use crate::config::Config;
use crate::engine::EngineLauncher;
use crate::session;
use crate::session::Command;
use crate::session::SessionDeps;
use crate::session::ThreadEvent;
use crate::session::ThreadHandle;
use crate::store::Store;
use crate::store::ThreadRecord;
use crate::translator::ApprovalAsk;
use crate::translator::ApprovalRequest;
use crate::translator::Decision;
use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequestResolvedNotification;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadArchivedNotification;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStartedNotification;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_protocol::openai_models::InputModality;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

const MODEL_PROVIDER: &str = "anthropic";
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

pub type ConnectionId = u64;

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn internal(err: impl std::fmt::Display) -> Self {
        Self::new(INTERNAL_ERROR, err.to_string())
    }
}

/// An approval request that is out with a client.
struct SentApproval {
    thread_id: String,
    engine_request_id: String,
}

struct Connection {
    outgoing: mpsc::UnboundedSender<Value>,
    initialized: bool,
    next_request_id: i64,
    /// Server-initiated requests awaiting this client's answer.
    pending: HashMap<i64, SentApproval>,
}

#[derive(Default)]
struct State {
    connections: HashMap<ConnectionId, Connection>,
    next_connection: ConnectionId,
    threads: HashMap<String, ThreadHandle>,
    /// Thread id to the connections that get its notifications.
    subscribers: HashMap<String, Vec<ConnectionId>>,
}

pub struct Bridge {
    config: Config,
    store: Arc<Store>,
    launcher: Arc<dyn EngineLauncher>,
    state: Mutex<State>,
    events: mpsc::UnboundedSender<(String, ThreadEvent)>,
    /// Shown to each client after it initializes.
    warnings: Vec<ConfigWarningNotification>,
}

impl Bridge {
    pub fn new(
        config: Config,
        launcher: Arc<dyn EngineLauncher>,
        warnings: Vec<ConfigWarningNotification>,
    ) -> anyhow::Result<Arc<Self>> {
        let store = Arc::new(Store::open(&config.state_dir)?);
        let (events, mut receiver) = mpsc::unbounded_channel();
        let bridge = Arc::new(Self {
            config,
            store,
            launcher,
            state: Mutex::new(State::default()),
            events,
            warnings,
        });
        let weak = Arc::downgrade(&bridge);
        tokio::spawn(async move {
            while let Some((thread_id, event)) = receiver.recv().await {
                let Some(bridge) = weak.upgrade() else {
                    return;
                };
                bridge.on_thread_event(&thread_id, event);
            }
        });
        Ok(bridge)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ---- connections ------------------------------------------------------

    /// Register a client. Messages for it are sent on `outgoing`.
    pub fn connect(&self, outgoing: mpsc::UnboundedSender<Value>) -> ConnectionId {
        let mut state = self.state();
        state.next_connection += 1;
        let id = state.next_connection;
        state.connections.insert(
            id,
            Connection {
                outgoing,
                initialized: false,
                next_request_id: 0,
                pending: HashMap::new(),
            },
        );
        id
    }

    pub fn disconnect(&self, connection: ConnectionId) {
        let mut state = self.state();
        state.connections.remove(&connection);
        for subscribers in state.subscribers.values_mut() {
            subscribers.retain(|id| *id != connection);
        }
    }

    /// Handle one message from a client.
    pub async fn handle_message(&self, connection: ConnectionId, message: Value) {
        let Some(object) = message.as_object() else {
            tracing::warn!("dropping a message that is not a JSON object");
            return;
        };
        let id = object.get("id").filter(|id| !id.is_null()).cloned();
        let params = object.get("params").cloned();
        match (object.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let mut after = Vec::new();
                let reply = match self
                    .handle_request(connection, method, params, &mut after)
                    .await
                {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err(err) => {
                        json!({ "id": id, "error": { "code": err.code, "message": err.message } })
                    }
                };
                self.send_to(connection, reply);
                for message in after {
                    self.send_to(connection, message);
                }
            }
            (Some("initialized"), None) => {
                for warning in &self.warnings {
                    self.send_to(
                        connection,
                        notification(ServerNotification::ConfigWarning(warning.clone())),
                    );
                }
            }
            (Some(_), None) => {}
            (None, Some(id)) => self.handle_response(connection, &id, object.get("result")),
            (None, None) => tracing::warn!("dropping a message with neither method nor id"),
        }
    }

    fn send_to(&self, connection: ConnectionId, message: Value) {
        if let Some(connection) = self.state().connections.get(&connection) {
            let _ = connection.outgoing.send(message);
        }
    }

    fn broadcast(&self, thread_id: &str, message: &Value) {
        let state = self.state();
        for id in state.subscribers.get(thread_id).into_iter().flatten() {
            if let Some(connection) = state.connections.get(id) {
                let _ = connection.outgoing.send(message.clone());
            }
        }
    }

    fn subscribe(&self, thread_id: &str, connection: ConnectionId) {
        let mut state = self.state();
        let subscribers = state.subscribers.entry(thread_id.to_string()).or_default();
        if !subscribers.contains(&connection) {
            subscribers.push(connection);
        }
    }

    // ---- events from threads ----------------------------------------------

    fn on_thread_event(&self, thread_id: &str, event: ThreadEvent) {
        match event {
            ThreadEvent::Notify(notification_value) => {
                self.broadcast(thread_id, &notification(notification_value));
            }
            ThreadEvent::Approval(ask) => self.send_approval(thread_id, &ask),
            ThreadEvent::ApprovalObsolete { engine_request_id } => {
                let mut resolved = Vec::new();
                {
                    let mut state = self.state();
                    for connection in state.connections.values_mut() {
                        let ids: Vec<i64> = connection
                            .pending
                            .iter()
                            .filter(|(_, sent)| {
                                sent.thread_id == thread_id
                                    && sent.engine_request_id == engine_request_id
                            })
                            .map(|(id, _)| *id)
                            .collect();
                        for id in ids {
                            connection.pending.remove(&id);
                            resolved.push(id);
                        }
                    }
                }
                for id in resolved {
                    self.broadcast(thread_id, &request_resolved(thread_id, id));
                }
            }
        }
    }

    /// Put an approval to a subscribed client. With no client connected it
    /// stays open and is asked again when a client resumes the thread.
    fn send_approval(&self, thread_id: &str, ask: &ApprovalAsk) {
        let mut state = self.state();
        let already_out = state.connections.values().any(|connection| {
            connection.pending.values().any(|sent| {
                sent.thread_id == thread_id && sent.engine_request_id == ask.engine_request_id
            })
        });
        if already_out {
            return;
        }
        let Some(target) = state
            .subscribers
            .get(thread_id)
            .into_iter()
            .flatten()
            .copied()
            .find(|id| state.connections.contains_key(id))
        else {
            return;
        };
        let Some(connection) = state.connections.get_mut(&target) else {
            return;
        };
        connection.next_request_id += 1;
        let id = connection.next_request_id;
        let (method, params) = match &ask.request {
            ApprovalRequest::Command(params) => (
                "item/commandExecution/requestApproval",
                serde_json::to_value(params),
            ),
            ApprovalRequest::FileChange(params) => (
                "item/fileChange/requestApproval",
                serde_json::to_value(params),
            ),
        };
        let Ok(params) = params else {
            return;
        };
        connection.pending.insert(
            id,
            SentApproval {
                thread_id: thread_id.to_string(),
                engine_request_id: ask.engine_request_id.clone(),
            },
        );
        let _ = connection
            .outgoing
            .send(json!({ "id": id, "method": method, "params": params }));
    }

    /// A client answered one of the bridge's requests.
    fn handle_response(&self, connection: ConnectionId, id: &Value, result: Option<&Value>) {
        let Some(id) = id.as_i64() else {
            return;
        };
        let (sent, thread) = {
            let mut state = self.state();
            let Some(sent) = state
                .connections
                .get_mut(&connection)
                .and_then(|connection| connection.pending.remove(&id))
            else {
                return;
            };
            let thread = state.threads.get(&sent.thread_id).cloned();
            (sent, thread)
        };
        // An error reply, or one without a decision, is a refusal.
        let decision = match result.and_then(|result| result.get("decision")) {
            Some(Value::String(text)) => match text.as_str() {
                "accept" => Decision::Accept,
                "acceptForSession" => Decision::AcceptForSession,
                "cancel" => Decision::Cancel,
                _ => Decision::Decline,
            },
            // Codex-specific amendments (exec policy, network policy) have no
            // Claude Code equivalent: treat them as a one-off accept.
            Some(Value::Object(_)) => Decision::Accept,
            _ => Decision::Decline,
        };
        if let Some(thread) = thread {
            let _ = thread.commands.send(Command::Approval {
                engine_request_id: sent.engine_request_id,
                decision,
            });
        }
        self.broadcast(&sent.thread_id, &request_resolved(&sent.thread_id, id));
    }

    // ---- threads ----------------------------------------------------------

    fn loaded(&self, thread_id: &str) -> Option<ThreadHandle> {
        self.state().threads.get(thread_id).cloned()
    }

    /// The actor for a thread, started on first use. The Claude Code process
    /// itself starts with the thread's first turn.
    fn load(&self, record: &ThreadRecord) -> ThreadHandle {
        let mut state = self.state();
        if let Some(handle) = state.threads.get(&record.id) {
            return handle.clone();
        }
        let handle = session::spawn(
            record.id.clone(),
            record.cwd.to_string_lossy().into_owned(),
            SessionDeps {
                store: self.store.clone(),
                launcher: self.launcher.clone(),
                interrupt_timeout: Duration::from_millis(self.config.interrupt_timeout_ms),
                events: self.events.clone(),
            },
        );
        state.threads.insert(record.id.clone(), handle.clone());
        handle
    }

    fn require_thread(&self, thread_id: &str) -> Result<ThreadRecord, RpcError> {
        self.store
            .get(thread_id)
            .ok_or_else(|| RpcError::new(INVALID_REQUEST, format!("thread not found: {thread_id}")))
    }

    fn thread_object(
        &self,
        record: &ThreadRecord,
        include_turns: bool,
    ) -> Result<Thread, RpcError> {
        let live = self.loaded(&record.id).map(|handle| handle.live());
        let mut turns = Vec::new();
        if include_turns {
            turns = self.store.turns(&record.id);
            if let Some(turn) = live.as_ref().and_then(|live| live.active_turn.clone()) {
                turns.push(turn);
            }
        }
        Ok(Thread {
            id: record.id.clone(),
            environments: None,
            extra: None,
            session_id: record.claude_session_id.clone(),
            forked_from_id: None,
            attached_to_thread_id: None,
            parent_thread_id: None,
            preview: record.preview.clone(),
            ephemeral: false,
            section: None,
            section_entered_at: None,
            project_id: None,
            history_mode: ThreadHistoryMode::Legacy,
            model_provider: MODEL_PROVIDER.to_string(),
            model: record
                .model
                .clone()
                .or_else(|| self.config.default_model().map(|model| model.id.clone())),
            reasoning_effort: None,
            created_at: record.created_at,
            updated_at: record.updated_at,
            recency_at: None,
            status: live.map_or(ThreadStatus::NotLoaded, |live| live.status),
            path: None,
            cwd: absolute(&record.cwd)?,
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            originator: Some("cc-codex-bridge".to_string()),
            source: SessionSource::AppServer,
            can_accept_direct_input: None,
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: record.name.clone(),
            daybreak_enabled: None,
            turns,
        })
    }

    /// The model reported for a thread: the one it was started with, else
    /// the configured default, else Claude Code's own default.
    fn model_name(&self, record: &ThreadRecord) -> String {
        record
            .model
            .clone()
            .or_else(|| self.config.default_model().map(|model| model.id.clone()))
            .unwrap_or_else(|| "default".to_string())
    }

    fn resolve_model(&self, requested: Option<String>) -> Result<Option<String>, RpcError> {
        match requested {
            None => Ok(None),
            // Only models the user listed are passed on to Claude Code.
            Some(model) if self.config.models.iter().any(|known| known.id == model) => {
                Ok(Some(model))
            }
            Some(model) => Err(RpcError::new(
                INVALID_PARAMS,
                format!("model {model:?} is not in the bridge's configured model list"),
            )),
        }
    }

    // ---- requests ---------------------------------------------------------

    async fn handle_request(
        &self,
        connection: ConnectionId,
        method: &str,
        params: Option<Value>,
        after: &mut Vec<Value>,
    ) -> Result<Value, RpcError> {
        if method == "initialize" {
            let _: InitializeParams = parse(params)?;
            let mut state = self.state();
            let Some(connection) = state.connections.get_mut(&connection) else {
                return Err(RpcError::new(INVALID_REQUEST, "connection closed"));
            };
            if connection.initialized {
                return Err(RpcError::new(INVALID_REQUEST, "Already initialized"));
            }
            connection.initialized = true;
            return reply(InitializeResponse {
                user_agent: format!("cc-codex-bridge/{}", env!("CARGO_PKG_VERSION")),
                codex_home: absolute(&self.config.state_dir)?,
                platform_family: std::env::consts::FAMILY.to_string(),
                platform_os: std::env::consts::OS.to_string(),
            });
        }
        let initialized = self
            .state()
            .connections
            .get(&connection)
            .is_some_and(|connection| connection.initialized);
        if !initialized {
            return Err(RpcError::new(INVALID_REQUEST, "Not initialized"));
        }
        match method {
            "thread/start" => self.thread_start(connection, parse(params)?, after),
            "thread/resume" => self.thread_resume(connection, parse(params)?),
            "thread/list" => self.thread_list(parse(params)?),
            "thread/read" => {
                let params: ThreadReadParams = parse(params)?;
                let record = self.require_thread(&params.thread_id)?;
                reply(ThreadReadResponse {
                    thread: self.thread_object(&record, params.include_turns)?,
                    read_state: None,
                })
            }
            "thread/archive" => self.thread_archive(parse(params)?).await,
            "thread/loaded/list" => {
                let mut data: Vec<String> = self.state().threads.keys().cloned().collect();
                data.sort();
                reply(ThreadLoadedListResponse {
                    data,
                    next_cursor: None,
                })
            }
            "thread/unsubscribe" => {
                let params: ThreadUnsubscribeParams = parse(params)?;
                self.require_thread(&params.thread_id)?;
                let mut state = self.state();
                let status = if !state.threads.contains_key(&params.thread_id) {
                    ThreadUnsubscribeStatus::NotLoaded
                } else {
                    let subscribers = state.subscribers.entry(params.thread_id).or_default();
                    if subscribers.contains(&connection) {
                        subscribers.retain(|id| *id != connection);
                        ThreadUnsubscribeStatus::Unsubscribed
                    } else {
                        ThreadUnsubscribeStatus::NotSubscribed
                    }
                };
                reply(ThreadUnsubscribeResponse { status })
            }
            "thread/name/set" => {
                let params: ThreadSetNameParams = parse(params)?;
                let record = self.require_thread(&params.thread_id)?;
                let name = params.name.clone();
                self.store
                    .update(&record.id, |record| record.name = Some(name))
                    .map_err(RpcError::internal)?;
                self.broadcast(
                    &record.id,
                    &notification(ServerNotification::ThreadNameUpdated(
                        ThreadNameUpdatedNotification {
                            thread_id: record.id.clone(),
                            thread_name: Some(params.name),
                        },
                    )),
                );
                reply(ThreadSetNameResponse {})
            }
            "turn/start" => self.turn_start(connection, parse(params)?),
            "turn/interrupt" => {
                let params: TurnInterruptParams = parse(params)?;
                self.require_thread(&params.thread_id)?;
                if let Some(thread) = self.loaded(&params.thread_id) {
                    let _ = thread.commands.send(Command::Interrupt {
                        turn_id: params.turn_id,
                    });
                }
                reply(TurnInterruptResponse {})
            }
            "model/list" => reply(self.model_list()),
            // Fixed answers: the bridge has no Codex account and no Codex
            // configuration.
            "account/read" => reply(GetAccountResponse {
                account: None,
                user_id: None,
                requires_openai_auth: false,
                workspace_routing: None,
            }),
            "config/read" => reply(ConfigReadResponse {
                config: serde_json::from_value(json!({})).map_err(RpcError::internal)?,
                origins: HashMap::new(),
                layers: None,
            }),
            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        }
    }

    fn thread_start(
        &self,
        connection: ConnectionId,
        params: ThreadStartParams,
        after: &mut Vec<Value>,
    ) -> Result<Value, RpcError> {
        let cwd = match &params.cwd {
            Some(cwd) => std::path::PathBuf::from(cwd),
            None => std::env::current_dir().map_err(RpcError::internal)?,
        };
        let cwd = absolute(&cwd)?.into_path_buf();
        if !cwd.is_dir() {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("cwd is not a directory: {}", cwd.display()),
            ));
        }
        let model = self.resolve_model(params.model)?;
        let now = session::now_ms() / 1000;
        let record = ThreadRecord {
            id: uuid::Uuid::new_v4().to_string(),
            claude_session_id: uuid::Uuid::new_v4().to_string(),
            cwd,
            model,
            name: None,
            preview: String::new(),
            created_at: now,
            updated_at: now,
            archived: false,
            started: false,
        };
        self.store
            .insert(record.clone())
            .map_err(RpcError::internal)?;
        self.load(&record);
        self.subscribe(&record.id, connection);
        let thread = self.thread_object(&record, false)?;
        after.push(notification(ServerNotification::ThreadStarted(
            ThreadStartedNotification {
                thread: thread.clone(),
            },
        )));
        reply(ThreadStartResponse {
            thread,
            model: self.model_name(&record),
            model_provider: MODEL_PROVIDER.to_string(),
            service_tier: None,
            disabled_plugin_ids: Vec::new(),
            cwd: absolute(&record.cwd)?,
            runtime_workspace_roots: Vec::new(),
            instruction_sources: Vec::new(),
            // Claude Code's permission system, not Codex's, decides what
            // needs approval, and the bridge adds no sandbox of its own.
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox: SandboxPolicy::DangerFullAccess,
            active_permission_profile: None,
            reasoning_effort: None,
            multi_agent_mode: Default::default(),
        })
    }

    fn thread_resume(
        &self,
        connection: ConnectionId,
        params: ThreadResumeParams,
    ) -> Result<Value, RpcError> {
        let record = self.require_thread(&params.thread_id)?;
        if record.archived {
            return Err(RpcError::new(
                INVALID_REQUEST,
                format!("thread is archived: {}", record.id),
            ));
        }
        let handle = self.load(&record);
        self.subscribe(&record.id, connection);
        // An approval left open when the previous client went away is asked
        // again.
        for ask in handle.live().open_approvals {
            self.send_approval(&record.id, &ask);
        }
        reply(ThreadResumeResponse {
            thread: self.thread_object(&record, !params.exclude_turns)?,
            model: self.model_name(&record),
            model_provider: MODEL_PROVIDER.to_string(),
            service_tier: None,
            disabled_plugin_ids: Vec::new(),
            cwd: absolute(&record.cwd)?,
            runtime_workspace_roots: Vec::new(),
            instruction_sources: Vec::new(),
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox: SandboxPolicy::DangerFullAccess,
            active_permission_profile: None,
            reasoning_effort: None,
            collaboration_mode: None,
            multi_agent_mode: Default::default(),
            initial_turns_page: None,
            turns_backwards_cursor: None,
            items_backwards_cursor: None,
        })
    }

    fn thread_list(&self, params: ThreadListParams) -> Result<Value, RpcError> {
        // Filters are applied to the serialised params so that the cwd filter
        // accepts both forms the protocol allows (one path or several).
        let raw = serde_json::to_value(&params).map_err(RpcError::internal)?;
        let mut records = self.store.list(params.archived == Some(true));
        let cwds: Option<Vec<String>> = match raw.get("cwd") {
            Some(Value::String(cwd)) => Some(vec![cwd.clone()]),
            Some(Value::Array(cwds)) => Some(
                cwds.iter()
                    .filter_map(|cwd| cwd.as_str().map(ToString::to_string))
                    .collect(),
            ),
            _ => None,
        };
        if let Some(cwds) = cwds {
            records.retain(|record| cwds.iter().any(|cwd| Path::new(cwd) == record.cwd));
        }
        if let Some(term) = params.search_term.as_ref().map(|term| term.to_lowercase()) {
            records.retain(|record| {
                format!(
                    "{}\n{}",
                    record.name.clone().unwrap_or_default(),
                    record.preview
                )
                .to_lowercase()
                .contains(&term)
            });
        }
        if let Some(excluded) = &params.excluded_thread_ids {
            records.retain(|record| !excluded.contains(&record.id));
        }
        if raw.get("sortDirection").and_then(Value::as_str) == Some("asc") {
            records.reverse();
        }
        let offset: usize = params
            .cursor
            .as_deref()
            .and_then(|cursor| cursor.parse().ok())
            .unwrap_or(0);
        let limit = params.limit.map_or(50, |limit| limit.clamp(1, 200)) as usize;
        let total = records.len();
        let data = records
            .iter()
            .skip(offset)
            .take(limit)
            .map(|record| self.thread_object(record, false))
            .collect::<Result<Vec<_>, _>>()?;
        reply(ThreadListResponse {
            data,
            read_states: None,
            next_cursor: (offset + limit < total).then(|| (offset + limit).to_string()),
            backwards_cursor: None,
        })
    }

    async fn thread_archive(&self, params: ThreadArchiveParams) -> Result<Value, RpcError> {
        let record = self.require_thread(&params.thread_id)?;
        let handle = self.state().threads.remove(&record.id);
        // Archiving ends the thread's Claude Code process.
        if let Some(handle) = handle {
            let (done, finished) = oneshot::channel();
            if handle.commands.send(Command::Shutdown { done }).is_ok() {
                let _ = finished.await;
            }
        }
        self.store
            .update(&record.id, |record| record.archived = true)
            .map_err(RpcError::internal)?;
        self.broadcast(
            &record.id,
            &notification(ServerNotification::ThreadArchived(
                ThreadArchivedNotification {
                    thread_id: record.id.clone(),
                },
            )),
        );
        self.state().subscribers.remove(&record.id);
        reply(ThreadArchiveResponse {})
    }

    fn turn_start(
        &self,
        connection: ConnectionId,
        params: TurnStartParams,
    ) -> Result<Value, RpcError> {
        let record = self.require_thread(&params.thread_id)?;
        if record.archived {
            return Err(RpcError::new(
                INVALID_REQUEST,
                format!("thread is archived: {}", record.id),
            ));
        }
        if params.input.is_empty() {
            return Err(RpcError::new(INVALID_PARAMS, "input must not be empty"));
        }
        let handle = self.load(&record);
        self.subscribe(&record.id, connection);
        let turn_id = uuid::Uuid::new_v4().to_string();
        handle
            .commands
            .send(Command::StartTurn {
                turn_id: turn_id.clone(),
                input: params.input,
                client_user_message_id: params.client_user_message_id,
            })
            .map_err(|_| RpcError::new(INTERNAL_ERROR, "the thread is shutting down"))?;
        reply(TurnStartResponse {
            turn: session::queued_turn(&turn_id),
        })
    }

    fn model_list(&self) -> ModelListResponse {
        let default_id = self.config.default_model().map(|model| model.id.clone());
        ModelListResponse {
            data: self
                .config
                .models
                .iter()
                .map(|model| Model {
                    id: model.id.clone(),
                    model: model.id.clone(),
                    upgrade: None,
                    upgrade_info: None,
                    availability_nux: None,
                    display_name: model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| model.id.clone()),
                    description: model.description.clone().unwrap_or_default(),
                    model_specialty: None,
                    hidden: false,
                    supported_reasoning_efforts: Vec::new(),
                    default_reasoning_effort: Default::default(),
                    input_modalities: vec![InputModality::Text],
                    supports_personality: false,
                    multi_agent_version: None,
                    additional_speed_tiers: Vec::new(),
                    service_tiers: Vec::new(),
                    default_service_tier: None,
                    available_access_programs: None,
                    is_default: Some(&model.id) == default_id.as_ref(),
                })
                .collect(),
            next_cursor: None,
        }
    }
}

fn parse<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|err| RpcError::new(INVALID_PARAMS, format!("invalid params: {err}")))
}

fn reply<T: Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

fn notification(value: ServerNotification) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| {
        tracing::error!("could not serialise a notification: {err}");
        Value::Null
    })
}

fn request_resolved(thread_id: &str, request_id: i64) -> Value {
    notification(ServerNotification::ServerRequestResolved(
        ServerRequestResolvedNotification {
            thread_id: thread_id.to_string(),
            request_id: codex_app_server_protocol::RequestId::Integer(request_id),
        },
    ))
}

fn absolute(path: &Path) -> Result<AbsolutePathBuf, RpcError> {
    AbsolutePathBuf::relative_to_current_dir(path).map_err(RpcError::internal)
}
