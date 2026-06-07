//! MCP surface for agent-facing compiler access.
//!
//! The command-line driver stays the human/stable shell surface. This module is
//! the structured sibling: the same compiler pipeline, exposed as typed MCP
//! tools so agents do not have to parse terminal prose.

use std::collections::{HashMap, HashSet};
#[cfg(not(test))]
use std::io::Read;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin};
#[cfg(not(test))]
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Condvar, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant};

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const EFFECT_CATALOG: &str = include_str!("effects.catalog");

#[derive(Debug, Clone)]
pub struct LocusMcpServer {
    tool_router: ToolRouter<Self>,
}

impl LocusMcpServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for LocusMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRequest {
    /// Path to a .locus source file. Mutually exclusive with source.
    pub file: Option<String>,
    /// Inline Locus source. Mutually exclusive with file.
    pub source: Option<String>,
    /// Additional boundary modules to trust for this request.
    pub boundary_modules: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRunRequest {
    /// Path to a .locus source file. Mutually exclusive with source.
    pub file: Option<String>,
    /// Inline Locus source. Mutually exclusive with file.
    pub source: Option<String>,
    /// Additional boundary modules to trust for this request.
    pub boundary_modules: Option<Vec<String>>,
    /// Queued text responses consumed in order by `agent_ask_text`.
    /// This is a replay model: inspect the transcript, then rerun with a longer queue.
    pub responses: Option<Vec<String>>,
    /// Response used when the queued responses are exhausted. Defaults to empty text.
    /// Ask events set `used_default: true` when this fallback was used.
    pub default_response: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionStartRequest {
    /// Path to a .locus source file. Mutually exclusive with source.
    pub file: Option<String>,
    /// Inline Locus source. Mutually exclusive with file.
    pub source: Option<String>,
    /// Additional boundary modules to trust for this request.
    pub boundary_modules: Option<Vec<String>>,
    /// How long to wait for the first ask/completion before returning. Defaults to 10000 ms.
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionReplyRequest {
    /// Live agent session id returned by agent_session_start.
    pub session_id: String,
    /// Response text to deliver to the currently waiting agent_ask_text call.
    pub response: String,
    /// If set, return events/asks/tells windows from this event index onward.
    /// Compact latest_* fields and total_event_count are still returned.
    pub since_event_index: Option<usize>,
    /// How long to wait for the next ask/completion before returning. Defaults to 10000 ms.
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionStatusRequest {
    /// Live agent session id returned by agent_session_start.
    pub session_id: String,
    /// If set, return events/asks/tells windows from this event index onward.
    /// Compact latest_* fields and total_event_count are still returned.
    pub since_event_index: Option<usize>,
    /// Optional long-poll wait. Defaults to 0 ms for immediate status.
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionCloseRequest {
    /// Live agent session id returned by agent_session_start.
    pub session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRequest {
    /// Path to a .locus source file. Mutually exclusive with source.
    pub file: Option<String>,
    /// Inline Locus source. Mutually exclusive with file.
    pub source: Option<String>,
    /// Additional boundary modules to trust for this request.
    pub boundary_modules: Option<Vec<String>>,
    /// Output executable path. Defaults to FILE with .exe for file input.
    pub out: Option<String>,
    /// Link the collector even when the program does not allocate.
    pub always_gc: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AsmRequest {
    /// Path to a .locus source file. Mutually exclusive with source.
    pub file: Option<String>,
    /// Inline Locus source. Mutually exclusive with file.
    pub source: Option<String>,
    /// Additional boundary modules to trust for this request.
    pub boundary_modules: Option<Vec<String>>,
    /// Emit optimized assembly, matching the shipped artifact.
    pub optimize: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterializeRequest {
    /// Target triple or alias. Supported today: host, windows-x86_64, x86_64-pc-windows-msvc.
    pub target: Option<String>,
    /// Artifact kind: exe or asm.
    pub artifact: Option<String>,
    /// Path to a .locus source file. Mutually exclusive with source.
    pub file: Option<String>,
    /// Inline Locus source. Mutually exclusive with file.
    pub source: Option<String>,
    /// Additional boundary modules to trust for this request.
    pub boundary_modules: Option<Vec<String>>,
    /// Output path. Required for inline-source exe materialization.
    pub out: Option<String>,
    /// Emit optimized assembly when artifact is asm.
    pub optimize: Option<bool>,
    /// Link the collector even when the program does not allocate.
    pub always_gc: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExplainRequest {
    /// Diagnostic code, for example RN-E0402.
    pub code: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelpSearchRequest {
    /// Search words, for example "ask agent text", "loop array", or "read file".
    pub query: String,
    /// Maximum number of results. Defaults to 8.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelpTopicRequest {
    /// Stable help id or title, for example syntax.loops, effects.rows, or agent.start.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelpServiceRequest {
    /// Service module or function name, for example Agent, agent_ask_text, or String.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelpRemindRequest {
    /// Topic words or stable id for a compact reminder.
    pub topic: String,
}

#[derive(Debug)]
struct SourceUnit {
    name: String,
    source: String,
    authorized: HashSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EffectEntry {
    label: String,
    category: String,
    description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DemandEntry {
    symbol: String,
    dll: String,
}

#[derive(Debug)]
struct PipelineReport {
    ty: String,
    effects: Vec<EffectEntry>,
    demanded: Vec<DemandEntry>,
    ir: locus::Ir,
}

#[derive(Debug, serde::Serialize)]
struct StdlibModuleEntry {
    platform: &'static str,
    layer: u8,
    name: &'static str,
    boundary: bool,
    bytes: usize,
    fnv1a64: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkerInfo {
    protocol: String,
    worker_version: String,
    worker_path: String,
    target_os: String,
    target_arch: String,
    abi_version: u32,
    diagnostic_schema: String,
    stdlib_hash: String,
    windows_stdlib_hash: String,
    linux_stdlib_hash: String,
    stdlib_module_count: usize,
    language_revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveStatus {
    Running,
    Waiting,
    Completed,
    Failed,
    Closed,
}

impl LiveStatus {
    fn as_str(self) -> &'static str {
        match self {
            LiveStatus::Running => "running",
            LiveStatus::Waiting => "waiting",
            LiveStatus::Completed => "completed",
            LiveStatus::Failed => "failed",
            LiveStatus::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug)]
enum LiveEvent {
    Ask {
        prompt: String,
        response: Option<String>,
        used_default: bool,
    },
    Tell {
        text: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LiveRunResult {
    result_i64: i64,
    exit_code: i32,
    ty: String,
    effects: Vec<EffectEntry>,
    demanded: Vec<DemandEntry>,
}

#[derive(Debug)]
struct LiveState {
    name: String,
    status: LiveStatus,
    events: Vec<LiveEvent>,
    pending_response: Option<String>,
    result: Option<LiveRunResult>,
    error: Option<String>,
}

struct LiveSession {
    state: Mutex<LiveState>,
    worker: Mutex<Option<LiveWorkerHandle>>,
    cv: Condvar,
}

struct LiveWorkerHandle {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    stderr_tail: Arc<Mutex<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerSource {
    name: String,
    source: String,
    authorized: Vec<String>,
}

impl From<SourceUnit> for WorkerSource {
    fn from(unit: SourceUnit) -> Self {
        let mut authorized: Vec<String> = unit.authorized.into_iter().collect();
        authorized.sort();
        authorized.dedup();
        Self {
            name: unit.name,
            source: unit.source,
            authorized,
        }
    }
}

impl From<WorkerSource> for SourceUnit {
    fn from(unit: WorkerSource) -> Self {
        Self {
            name: unit.name,
            source: unit.source,
            authorized: unit.authorized.into_iter().collect(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum WorkerJob {
    Hello,
    HelpOverview,
    HelpSearch {
        query: String,
        limit: Option<usize>,
    },
    HelpTopic {
        id: String,
    },
    HelpService {
        name: String,
    },
    HelpRemind {
        topic: String,
    },
    ListStdlibServices,
    Check {
        source: WorkerSource,
    },
    EmitIr {
        source: WorkerSource,
    },
    EmitAsm {
        source: WorkerSource,
        optimize: bool,
    },
    Build {
        source: WorkerSource,
        output: String,
        always_gc: bool,
    },
    Effects {
        source: WorkerSource,
    },
    Run {
        source: WorkerSource,
    },
    RunAgentText {
        source: WorkerSource,
        responses: Vec<String>,
        default_response: String,
    },
    LiveAgent {
        source: WorkerSource,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkerRunResult {
    name: String,
    result_i64: i64,
    exit_code: i32,
    ty: String,
    effects: Vec<EffectEntry>,
    demanded: Vec<DemandEntry>,
}

impl From<WorkerRunResult> for LiveRunResult {
    fn from(result: WorkerRunResult) -> Self {
        Self {
            result_i64: result.result_i64,
            exit_code: result.exit_code,
            ty: result.ty,
            effects: result.effects,
            demanded: result.demanded,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerEvent {
    Started {
        name: String,
    },
    ToolCompleted {
        value: serde_json::Value,
    },
    CompileStarted,
    CompileFinished {
        ty: String,
        effects: Vec<EffectEntry>,
        demanded: Vec<DemandEntry>,
    },
    RunStarted,
    Ask {
        prompt: String,
    },
    Tell {
        text: String,
    },
    Completed {
        result: WorkerRunResult,
        agent_transcript: Option<serde_json::Value>,
    },
    Failed {
        code: String,
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerCommand {
    Reply { response: String },
    Cancel,
}

#[derive(Debug)]
struct WorkerCompleted {
    result: WorkerRunResult,
    agent_transcript: Option<serde_json::Value>,
}

#[derive(Debug)]
struct WorkerFailure {
    code: String,
    message: String,
}

#[tool_router]
impl LocusMcpServer {
    #[tool(
        description = "Type-check and lower a Locus program without running it",
        annotations(title = "Check Locus", read_only_hint = true, destructive_hint = false)
    )]
    fn check(&self, Parameters(req): Parameters<SourceRequest>) -> CallToolResult {
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        match supervise_worker_tool(WorkerJob::Check {
            source: unit.into(),
        }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Return the agent-start help overview for Locus syntax, operations, and services",
        annotations(
            title = "Help Overview",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn help_overview(&self) -> CallToolResult {
        match supervise_worker_tool(WorkerJob::HelpOverview) {
            Ok(mut value) => {
                insert_object_field(&mut value, "supervisor", supervisor_info_value());
                tool_ok(value)
            }
            Err(err) => {
                let mut value = help_overview_value();
                insert_object_field(&mut value, "supervisor", supervisor_info_value());
                insert_object_field(
                    &mut value,
                    "worker_error",
                    json!({ "code": err.code, "message": err.message }),
                );
                tool_ok(value)
            }
        }
    }

    #[tool(
        description = "Search the embedded Locus help index for syntax, operations, services, examples, and reminders",
        annotations(title = "Help Search", read_only_hint = true, destructive_hint = false)
    )]
    fn help_search(&self, Parameters(req): Parameters<HelpSearchRequest>) -> CallToolResult {
        match supervise_worker_tool(WorkerJob::HelpSearch {
            query: req.query,
            limit: req.limit,
        }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Return an exact Locus help topic by stable id or title",
        annotations(title = "Help Topic", read_only_hint = true, destructive_hint = false)
    )]
    fn help_topic(&self, Parameters(req): Parameters<HelpTopicRequest>) -> CallToolResult {
        match supervise_worker_tool(WorkerJob::HelpTopic { id: req.id }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Return Locus stdlib service/module/function help by name",
        annotations(
            title = "Help Service",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn help_service(&self, Parameters(req): Parameters<HelpServiceRequest>) -> CallToolResult {
        match supervise_worker_tool(WorkerJob::HelpService { name: req.name }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Return a compact Locus reminder card for a syntax/service topic",
        annotations(
            title = "Help Reminder",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn help_remind(&self, Parameters(req): Parameters<HelpRemindRequest>) -> CallToolResult {
        match supervise_worker_tool(WorkerJob::HelpRemind { topic: req.topic }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Emit Locus ANF IR text for a program",
        annotations(title = "Emit IR", read_only_hint = true, destructive_hint = false)
    )]
    fn emit_ir(&self, Parameters(req): Parameters<SourceRequest>) -> CallToolResult {
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        match supervise_worker_tool(WorkerJob::EmitIr {
            source: unit.into(),
        }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Emit host x86-64 assembly for a Locus program",
        annotations(
            title = "Emit Assembly",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn emit_asm(&self, Parameters(req): Parameters<AsmRequest>) -> CallToolResult {
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        let optimize = req.optimize.unwrap_or(false);
        match supervise_worker_tool(WorkerJob::EmitAsm {
            source: unit.into(),
            optimize,
        }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Build a standalone Windows executable from a Locus program",
        annotations(title = "Build Executable", destructive_hint = true)
    )]
    fn build(&self, Parameters(req): Parameters<BuildRequest>) -> CallToolResult {
        let unit = match source_unit(req.file.clone(), req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        let exe = match output_exe(req.file.as_deref(), req.out.as_deref()) {
            Ok(exe) => exe,
            Err(err) => return tool_error("invalid_input", err),
        };
        let always_gc = req.always_gc.unwrap_or(false);
        match supervise_worker_tool(WorkerJob::Build {
            source: unit.into(),
            output: exe.display().to_string(),
            always_gc,
        }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "JIT-run a Locus program and return its i64 result",
        annotations(title = "Run Program", destructive_hint = true)
    )]
    fn run(&self, Parameters(req): Parameters<SourceRequest>) -> CallToolResult {
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        match supervise_worker_job(WorkerJob::Run {
            source: unit.into(),
        }) {
            Ok(done) => tool_ok(worker_completed_json(done)),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "JIT-run a Locus program with a queued MCP/agent ask-response channel; inspect the transcript and rerun with more responses when needed",
        annotations(title = "Run With Agent Text", destructive_hint = true)
    )]
    fn run_agent_text(&self, Parameters(req): Parameters<AgentRunRequest>) -> CallToolResult {
        let responses = req.responses.unwrap_or_default();
        let default_response = req.default_response.unwrap_or_default();
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        match supervise_worker_job(WorkerJob::RunAgentText {
            source: unit.into(),
            responses,
            default_response,
        }) {
            Ok(done) => tool_ok(worker_completed_json(done)),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Start a live Locus Agent session; the program runs until the first agent_ask_text, completion, failure, or wait timeout",
        annotations(title = "Start Live Agent Session", destructive_hint = true)
    )]
    fn agent_session_start(
        &self,
        Parameters(req): Parameters<AgentSessionStartRequest>,
    ) -> CallToolResult {
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        let session_id = next_live_session_id();
        let session = Arc::new(LiveSession {
            state: Mutex::new(LiveState {
                name: unit.name.clone(),
                status: LiveStatus::Running,
                events: Vec::new(),
                pending_response: None,
                result: None,
                error: None,
            }),
            worker: Mutex::new(None),
            cv: Condvar::new(),
        });
        live_sessions()
            .lock()
            .expect("live session registry poisoned")
            .insert(session_id.clone(), session.clone());

        #[cfg(test)]
        let start_result = start_in_process_live_session(&session_id, unit, session.clone());
        #[cfg(not(test))]
        let start_result = start_worker_live_session(&session_id, unit, session.clone());

        if let Err(err) = start_result {
            let mut state = session.state.lock().expect("live session state poisoned");
            state.status = LiveStatus::Failed;
            state.error = Some(err);
            session.cv.notify_all();
        }

        tool_ok(live_session_wait_json(
            &session_id,
            &session,
            req.wait_ms.unwrap_or(10_000),
            None,
        ))
    }

    #[tool(
        description = "Reply to the current live agent_ask_text prompt and wait for the next ask/completion; pass since_event_index to receive only new transcript events",
        annotations(title = "Reply To Live Agent Session", destructive_hint = true)
    )]
    fn agent_session_reply(
        &self,
        Parameters(req): Parameters<AgentSessionReplyRequest>,
    ) -> CallToolResult {
        let Some(session) = live_session(&req.session_id) else {
            return tool_error(
                "unknown_agent_session",
                format!("unknown live agent session `{}`", req.session_id),
            );
        };
        {
            let response = req.response.clone();
            let mut state = session.state.lock().expect("live session state poisoned");
            if state.status != LiveStatus::Waiting {
                return tool_error(
                    "agent_session_not_waiting",
                    format!(
                        "session `{}` is `{}`; no agent_ask_text is waiting",
                        req.session_id,
                        state.status.as_str()
                    ),
                );
            }
            if let Some(LiveEvent::Ask { response: slot, .. }) = state
                .events
                .iter_mut()
                .rev()
                .find(|event| matches!(event, LiveEvent::Ask { response: None, .. }))
            {
                *slot = Some(response.clone());
            }
            state.pending_response = Some(response);
            state.status = LiveStatus::Running;
            session.cv.notify_all();
        }
        #[cfg(not(test))]
        if let Err(err) = live_worker_reply(&session, &req.response) {
            let mut state = session.state.lock().expect("live session state poisoned");
            state.status = LiveStatus::Failed;
            state.error = Some(err);
            session.cv.notify_all();
        }
        tool_ok(live_session_wait_json(
            &req.session_id,
            &session,
            req.wait_ms.unwrap_or(10_000),
            req.since_event_index,
        ))
    }

    #[tool(
        description = "Return or long-poll the current live Agent session status, transcript window, compact latest fields, current ask, and result",
        annotations(
            title = "Live Agent Session Status",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn agent_session_status(
        &self,
        Parameters(req): Parameters<AgentSessionStatusRequest>,
    ) -> CallToolResult {
        let Some(session) = live_session(&req.session_id) else {
            return tool_error(
                "unknown_agent_session",
                format!("unknown live agent session `{}`", req.session_id),
            );
        };
        tool_ok(live_session_wait_json(
            &req.session_id,
            &session,
            req.wait_ms.unwrap_or(0),
            req.since_event_index,
        ))
    }

    #[tool(
        description = "Close a live Agent session and release it from the MCP server",
        annotations(title = "Close Live Agent Session", destructive_hint = true)
    )]
    fn agent_session_close(
        &self,
        Parameters(req): Parameters<AgentSessionCloseRequest>,
    ) -> CallToolResult {
        let Some(session) = live_sessions()
            .lock()
            .expect("live session registry poisoned")
            .remove(&req.session_id)
        else {
            return tool_error(
                "unknown_agent_session",
                format!("unknown live agent session `{}`", req.session_id),
            );
        };
        {
            let mut state = session.state.lock().expect("live session state poisoned");
            if !matches!(
                state.status,
                LiveStatus::Completed | LiveStatus::Failed | LiveStatus::Closed
            ) {
                state.status = LiveStatus::Closed;
            }
            session.cv.notify_all();
        }
        close_live_worker(&session);
        tool_ok(live_session_snapshot_json(&req.session_id, &session, None))
    }

    #[tool(
        description = "Return the effect manifest for a Locus program",
        annotations(
            title = "Effect Manifest",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn effects(&self, Parameters(req): Parameters<SourceRequest>) -> CallToolResult {
        let unit = match source_unit(req.file, req.source, req.boundary_modules) {
            Ok(unit) => unit,
            Err(err) => return tool_error("invalid_input", err),
        };
        match supervise_worker_tool(WorkerJob::Effects {
            source: unit.into(),
        }) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "List the embedded stdlib services and boundary modules",
        annotations(
            title = "List Stdlib Services",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn list_stdlib_services(&self) -> CallToolResult {
        match supervise_worker_tool(WorkerJob::ListStdlibServices) {
            Ok(value) => tool_ok(value),
            Err(err) => tool_error(err.code, err.message),
        }
    }

    #[tool(
        description = "Explain a stable Locus diagnostic code",
        annotations(
            title = "Explain Diagnostic",
            read_only_hint = true,
            destructive_hint = false
        )
    )]
    fn explain_diagnostic(&self, Parameters(req): Parameters<ExplainRequest>) -> CallToolResult {
        let code = req.code.trim().to_ascii_uppercase();
        let (family, slug, summary, action) = diagnostic_explanation(&code);
        tool_ok(json!({
            "code": code,
            "family": family,
            "slug": slug,
            "summary": summary,
            "action": action,
        }))
    }

    #[tool(
        description = "Materialize a target artifact for a Locus program",
        annotations(title = "Materialize Target", destructive_hint = true)
    )]
    fn materialize_target(
        &self,
        Parameters(req): Parameters<MaterializeRequest>,
    ) -> CallToolResult {
        let target = req.target.as_deref().unwrap_or("host");
        if !matches!(target, "host" | "windows-x86_64" | "x86_64-pc-windows-msvc") {
            return tool_error(
                "unsupported_target",
                format!(
                    "target `{target}` is not wired in this driver yet; supported: host, windows-x86_64, x86_64-pc-windows-msvc"
                ),
            );
        }

        match req.artifact.as_deref().unwrap_or("exe") {
            "asm" => self.emit_asm(Parameters(AsmRequest {
                file: req.file,
                source: req.source,
                boundary_modules: req.boundary_modules,
                optimize: req.optimize,
            })),
            "exe" => self.build(Parameters(BuildRequest {
                file: req.file,
                source: req.source,
                boundary_modules: req.boundary_modules,
                out: req.out,
                always_gc: req.always_gc,
            })),
            other => tool_error(
                "unsupported_artifact",
                format!("artifact `{other}` is not supported; use `exe` or `asm`"),
            ),
        }
    }
}

#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for LocusMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Locus compiler MCP server. Call help_overview first if you need to learn or recall syntax/services and workspace_cwd. Tools accept either `file` or `source`; relative file paths resolve from the server current working directory. build/run may execute program effects. Use agent_session_start and agent_session_reply for live Agent ask/tell I/O; use run_agent_text for deterministic queued replay.",
            )
            .with_server_info(rmcp::model::Implementation::new(
                "locus-llvm",
                env!("CARGO_PKG_VERSION"),
            ))
    }
}

pub async fn serve_stdio() -> Result<(), String> {
    let service = LocusMcpServer::new()
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

pub fn serve_blocking_stdio() -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting MCP runtime: {e}"))?;
    rt.block_on(serve_stdio())?;
    Ok(0)
}

fn source_unit(
    file: Option<String>,
    source: Option<String>,
    extra_boundary_modules: Option<Vec<String>>,
) -> Result<SourceUnit, String> {
    let mut extra = extra_boundary_modules.unwrap_or_default();
    extra.sort();
    extra.dedup();

    match (file, source) {
        (Some(_), Some(_)) => Err("provide either `file` or `source`, not both".into()),
        (None, None) => Err("provide `file` or `source`".into()),
        (Some(file), None) => {
            let path = Path::new(&file);
            let mut authorized = read_boundary_manifest(path);
            authorized.extend(extra);
            let source =
                std::fs::read_to_string(path).map_err(|e| format!("reading `{file}`: {e}"))?;
            Ok(SourceUnit {
                name: file,
                source,
                authorized,
            })
        }
        (None, Some(source)) => Ok(SourceUnit {
            name: "<source>".into(),
            source,
            authorized: extra.into_iter().collect(),
        }),
    }
}

fn output_exe(file: Option<&str>, out: Option<&str>) -> Result<PathBuf, String> {
    if let Some(out) = out {
        return Ok(PathBuf::from(out));
    }
    match file {
        Some(file) => Ok(PathBuf::from(file).with_extension("exe")),
        None => Err("`out` is required when building from inline `source`".into()),
    }
}

fn compile_pipeline(source: String, authorized: HashSet<String>) -> Result<PipelineReport, String> {
    on_pipeline_stack("locus-mcp-compile", move || {
        guard_layer2(&source, &authorized)?;
        let (term, user_modules) = locus::program_with_modules(&source).map_err(|e| e.msg)?;
        let (term, demanded) = crate::winapi_resolve::resolve(term)?;
        let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
            .map_err(|e| e.to_string())?;
        let ty = tree.ty.to_string();
        let effects = tree.row.labels().map(effect_entry).collect();

        let mut all_modules = locus::stdlib_module_decls();
        all_modules.extend(user_modules);
        locus::check_module_seals(&all_modules, &tree).map_err(|e| e.to_string())?;

        let tree = locus::stage_reduce(&tree)?;
        if tree.has_unknown_layout() {
            return Err(locus::TypeErr::RepresentationPolymorphicLayout.to_string());
        }
        Ok(PipelineReport {
            ty,
            effects,
            demanded: demanded
                .into_iter()
                .map(|(symbol, dll)| DemandEntry { symbol, dll })
                .collect(),
            ir: locus::lower(&tree),
        })
    })
}

fn effect_manifest(
    source: String,
    authorized: HashSet<String>,
) -> Result<(String, Vec<EffectEntry>), String> {
    on_pipeline_stack("locus-mcp-effects", move || {
        guard_layer2(&source, &authorized)?;
        let term = locus::program(&source).map_err(|e| e.msg)?;
        let (term, _demanded) = crate::winapi_resolve::resolve(term)?;
        let tree = locus::elaborate(&locus::prelude::sig(), &locus::Ctx::new(), 0, &term)
            .map_err(|e| e.to_string())?;
        Ok((
            tree.ty.to_string(),
            tree.row.labels().map(effect_entry).collect(),
        ))
    })
}

fn on_pipeline_stack<T, F>(name: &'static str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(f)
        .map_err(|e| format!("spawning compiler worker: {e}"))?
        .join()
        .map_err(|_| "compiler worker panicked".to_string())?
}

fn worker_tool_value(job: WorkerJob) -> Result<serde_json::Value, WorkerFailure> {
    match job {
        WorkerJob::Hello => Ok(json!({
            "kind": "worker_hello",
            "worker": worker_info_value(),
        })),
        WorkerJob::HelpOverview => Ok(help_overview_value()),
        WorkerJob::HelpSearch { query, limit } => Ok(help_search_value(&query, limit)),
        WorkerJob::HelpTopic { id } => help_topic_value(&id),
        WorkerJob::HelpService { name } => help_service_value(&name),
        WorkerJob::HelpRemind { topic } => Ok(help_remind_value(&topic)),
        WorkerJob::ListStdlibServices => Ok(list_stdlib_services_value()),
        WorkerJob::Check { source } => {
            let (name, report) = worker_compile(source.into())?;
            Ok(json!({
                "name": name,
                "type": report.ty,
                "pure": report.effects.is_empty(),
                "effect_count": report.effects.len(),
                "effects": report.effects,
                "demanded_apis": report.demanded,
            }))
        }
        WorkerJob::EmitIr { source } => {
            let (name, report) = worker_compile(source.into())?;
            let ir = report.ir.to_text();
            Ok(json!({
                "name": name,
                "type": report.ty,
                "pure": report.effects.is_empty(),
                "effects": report.effects,
                "demanded_apis": report.demanded,
                "ir": ir,
            }))
        }
        WorkerJob::EmitAsm { source, optimize } => {
            let (name, report) = worker_compile(source.into())?;
            let asm = if optimize {
                crate::emit_asm_opt(&report.ir)
            } else {
                crate::emit_asm(&report.ir)
            }
            .map_err(|message| WorkerFailure {
                code: "asm_failed".into(),
                message,
            })?;
            Ok(json!({
                "name": name,
                "type": report.ty,
                "pure": report.effects.is_empty(),
                "effects": report.effects,
                "demanded_apis": report.demanded,
                "optimized": optimize,
                "asm": asm,
            }))
        }
        WorkerJob::Build {
            source,
            output,
            always_gc,
        } => {
            let (name, report) = worker_compile(source.into())?;
            let demanded = demanded_map(&report.demanded);
            let import_libs = crate::winapi_resolve::import_libs(&demanded);
            let exe = PathBuf::from(&output);
            crate::build_exe(&report.ir, &exe, &import_libs, always_gc).map_err(|message| {
                WorkerFailure {
                    code: "build_failed".into(),
                    message,
                }
            })?;
            Ok(json!({
                "name": name,
                "output": exe.display().to_string(),
                "type": report.ty,
                "pure": report.effects.is_empty(),
                "effects": report.effects,
                "demanded_apis": report.demanded,
                "import_libs": import_libs,
                "always_gc": always_gc,
            }))
        }
        WorkerJob::Effects { source } => {
            let unit: SourceUnit = source.into();
            let name = unit.name;
            let (ty, effects) =
                effect_manifest(unit.source, unit.authorized).map_err(|message| WorkerFailure {
                    code: "effects_failed".into(),
                    message,
                })?;
            Ok(json!({
                "name": name,
                "type": ty,
                "pure": effects.is_empty(),
                "effect_count": effects.len(),
                "effects": effects,
            }))
        }
        WorkerJob::Run { .. } | WorkerJob::RunAgentText { .. } | WorkerJob::LiveAgent { .. } => {
            Err(WorkerFailure {
                code: "worker_mode_unsupported".into(),
                message: "this worker tool path does not run programs".into(),
            })
        }
    }
}

fn worker_tool_emit(job: WorkerJob) -> Result<(), String> {
    match worker_tool_value(job) {
        Ok(value) => worker_emit(&WorkerEvent::ToolCompleted { value }),
        Err(err) => worker_emit(&WorkerEvent::Failed {
            code: err.code,
            message: err.message,
        }),
    }
}

pub fn worker_blocking_stdio() -> Result<i32, String> {
    let command_reader = Arc::new(Mutex::new(BufReader::new(std::io::stdin())));
    let mut first = String::new();
    {
        let mut reader = command_reader.lock().expect("worker stdin reader poisoned");
        let n = reader
            .read_line(&mut first)
            .map_err(|e| format!("reading worker job: {e}"))?;
        if n == 0 {
            return Err("worker expected one JSON job on stdin".into());
        }
    }
    let job: WorkerJob = serde_json::from_str(&first)
        .map_err(|e| format!("invalid worker job JSON: {e}: {first}"))?;
    match job {
        WorkerJob::Hello
        | WorkerJob::HelpOverview
        | WorkerJob::HelpSearch { .. }
        | WorkerJob::HelpTopic { .. }
        | WorkerJob::HelpService { .. }
        | WorkerJob::HelpRemind { .. }
        | WorkerJob::ListStdlibServices
        | WorkerJob::Check { .. }
        | WorkerJob::EmitIr { .. }
        | WorkerJob::EmitAsm { .. }
        | WorkerJob::Build { .. }
        | WorkerJob::Effects { .. } => {
            worker_tool_emit(job)?;
        }
        WorkerJob::Run { source } => {
            emit_worker_started(&source.name)?;
            worker_emit(&WorkerEvent::CompileStarted)?;
            match worker_compile(source.into()) {
                Ok((name, report)) => {
                    worker_emit(&WorkerEvent::CompileFinished {
                        ty: report.ty.clone(),
                        effects: report.effects.clone(),
                        demanded: report.demanded.clone(),
                    })?;
                    worker_emit(&WorkerEvent::RunStarted)?;
                    match worker_run_compiled(name, report) {
                        Ok(done) => worker_emit(&WorkerEvent::Completed {
                            result: done.result,
                            agent_transcript: None,
                        })?,
                        Err(err) => worker_emit(&WorkerEvent::Failed {
                            code: err.code,
                            message: err.message,
                        })?,
                    }
                }
                Err(err) => worker_emit(&WorkerEvent::Failed {
                    code: err.code,
                    message: err.message,
                })?,
            }
        }
        WorkerJob::RunAgentText {
            source,
            responses,
            default_response,
        } => {
            emit_worker_started(&source.name)?;
            worker_emit(&WorkerEvent::CompileStarted)?;
            match worker_compile(source.into()) {
                Ok((name, report)) => {
                    worker_emit(&WorkerEvent::CompileFinished {
                        ty: report.ty.clone(),
                        effects: report.effects.clone(),
                        demanded: report.demanded.clone(),
                    })?;
                    worker_emit(&WorkerEvent::RunStarted)?;
                    match worker_run_compiled_agent_text(name, report, responses, default_response)
                    {
                        Ok(done) => worker_emit(&WorkerEvent::Completed {
                            result: done.result,
                            agent_transcript: done.agent_transcript,
                        })?,
                        Err(err) => worker_emit(&WorkerEvent::Failed {
                            code: err.code,
                            message: err.message,
                        })?,
                    }
                }
                Err(err) => worker_emit(&WorkerEvent::Failed {
                    code: err.code,
                    message: err.message,
                })?,
            }
        }
        WorkerJob::LiveAgent { source } => {
            emit_worker_started(&source.name)?;
            worker_run_live_agent(source.into(), command_reader)?;
        }
    }
    Ok(0)
}

fn emit_worker_started(name: &str) -> Result<(), String> {
    worker_emit(&WorkerEvent::Started {
        name: name.to_string(),
    })
}

fn worker_compile(unit: SourceUnit) -> Result<(String, PipelineReport), WorkerFailure> {
    let name = unit.name;
    compile_pipeline(unit.source, unit.authorized)
        .map(|report| (name, report))
        .map_err(|message| WorkerFailure {
            code: "compile_failed".into(),
            message,
        })
}

fn worker_run_compiled(
    name: String,
    report: PipelineReport,
) -> Result<WorkerCompleted, WorkerFailure> {
    let demanded = demanded_map(&report.demanded);
    let value = crate::jit_run_i64(&report.ir, &demanded).map_err(|message| WorkerFailure {
        code: "run_failed".into(),
        message,
    })?;
    Ok(WorkerCompleted {
        result: worker_run_result(name, report, value),
        agent_transcript: None,
    })
}

fn worker_run_compiled_agent_text(
    name: String,
    report: PipelineReport,
    responses: Vec<String>,
    default_response: String,
) -> Result<WorkerCompleted, WorkerFailure> {
    let demanded = demanded_map(&report.demanded);
    let (run_result, transcript) =
        crate::runtime::with_agent_text_session(responses, default_response, || {
            crate::jit_run_i64(&report.ir, &demanded)
        });
    let value = run_result.map_err(|message| WorkerFailure {
        code: "run_failed".into(),
        message,
    })?;
    Ok(WorkerCompleted {
        result: worker_run_result(name, report, value),
        agent_transcript: Some(agent_transcript_json(&transcript)),
    })
}

fn worker_run_live_agent(
    unit: SourceUnit,
    command_reader: Arc<Mutex<BufReader<std::io::Stdin>>>,
) -> Result<(), String> {
    worker_emit(&WorkerEvent::CompileStarted)?;
    match worker_compile(unit) {
        Ok((name, report)) => {
            worker_emit(&WorkerEvent::CompileFinished {
                ty: report.ty.clone(),
                effects: report.effects.clone(),
                demanded: report.demanded.clone(),
            })?;
            worker_emit(&WorkerEvent::RunStarted)?;
            let demanded = demanded_map(&report.demanded);
            let callback_reader = command_reader.clone();
            let callback = move |event| worker_live_callback(&callback_reader, event);
            let (run_result, _transcript) =
                crate::runtime::with_agent_live_session(callback, || {
                    crate::jit_run_i64(&report.ir, &demanded)
                });
            match run_result {
                Ok(value) => worker_emit(&WorkerEvent::Completed {
                    result: worker_run_result(name, report, value),
                    agent_transcript: None,
                })?,
                Err(message) => worker_emit(&WorkerEvent::Failed {
                    code: "run_failed".into(),
                    message,
                })?,
            }
        }
        Err(err) => worker_emit(&WorkerEvent::Failed {
            code: err.code,
            message: err.message,
        })?,
    }
    Ok(())
}

fn worker_live_callback(
    command_reader: &Arc<Mutex<BufReader<std::io::Stdin>>>,
    event: crate::runtime::AgentHostEvent,
) -> Option<String> {
    match event {
        crate::runtime::AgentHostEvent::Tell { text } => {
            let _ = worker_emit(&WorkerEvent::Tell { text });
            None
        }
        crate::runtime::AgentHostEvent::Ask { prompt } => {
            let _ = worker_emit(&WorkerEvent::Ask { prompt });
            let mut line = String::new();
            let read = command_reader
                .lock()
                .expect("worker stdin reader poisoned")
                .read_line(&mut line)
                .ok()?;
            if read == 0 {
                return Some(String::new());
            }
            match serde_json::from_str::<WorkerCommand>(&line) {
                Ok(WorkerCommand::Reply { response }) => Some(response),
                Ok(WorkerCommand::Cancel) | Err(_) => Some(String::new()),
            }
        }
    }
}

fn worker_run_result(name: String, report: PipelineReport, value: i64) -> WorkerRunResult {
    WorkerRunResult {
        name,
        result_i64: value,
        exit_code: value as i32,
        ty: report.ty,
        effects: report.effects,
        demanded: report.demanded,
    }
}

fn worker_emit(event: &WorkerEvent) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    write_json_line(&mut stdout, event)?;
    stdout
        .flush()
        .map_err(|e| format!("flushing worker event: {e}"))
}

fn worker_completed_json(done: WorkerCompleted) -> serde_json::Value {
    let WorkerCompleted {
        result,
        agent_transcript,
    } = done;
    let mut value = json!({
        "name": result.name,
        "result_i64": result.result_i64,
        "exit_code": result.exit_code,
        "type": result.ty,
        "effects": result.effects,
        "demanded_apis": result.demanded,
    });
    if let Some(transcript) = agent_transcript {
        value["agent_io_model"] = json!("queued_replay");
        value["agent_io_hint"] = json!(
            "agent_ask_text consumes `responses` in order. If an ask has used_default=true, rerun with a longer response queue."
        );
        value["agent_transcript"] = transcript;
    }
    value
}

#[cfg(test)]
fn supervise_worker_tool(job: WorkerJob) -> Result<serde_json::Value, WorkerFailure> {
    worker_tool_value(job)
}

#[cfg(not(test))]
fn supervise_worker_tool(job: WorkerJob) -> Result<serde_json::Value, WorkerFailure> {
    supervise_worker_tool_process(job)
}

#[cfg(not(test))]
fn supervise_worker_tool_process(job: WorkerJob) -> Result<serde_json::Value, WorkerFailure> {
    let mut worker = spawn_worker_process().map_err(|message| WorkerFailure {
        code: "worker_spawn_failed".into(),
        message,
    })?;
    write_json_line(&mut worker.stdin, &job).map_err(|message| WorkerFailure {
        code: "worker_protocol_failed".into(),
        message,
    })?;
    drop(worker.stdin);

    let mut reader = BufReader::new(worker.stdout);
    let mut last_event = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| WorkerFailure {
            code: "worker_protocol_failed".into(),
            message: format!("reading worker event: {e}"),
        })?;
        if n == 0 {
            let status = wait_child_or_kill(&mut worker.child, Duration::from_secs(1)).ok();
            return Err(worker_crashed(
                "worker closed stdout before tool completion",
                status,
                &worker.stderr_tail,
                &last_event,
            ));
        }
        last_event = line.trim_end().to_string();
        match serde_json::from_str::<WorkerEvent>(&line) {
            Ok(WorkerEvent::ToolCompleted { value }) => {
                let _ = wait_child_or_kill(&mut worker.child, Duration::from_secs(1));
                return Ok(value);
            }
            Ok(WorkerEvent::Failed { code, message }) => {
                let _ = wait_child_or_kill(&mut worker.child, Duration::from_secs(1));
                return Err(WorkerFailure { code, message });
            }
            Ok(WorkerEvent::Completed { .. }) => {
                let _ = wait_child_or_kill(&mut worker.child, Duration::from_secs(1));
                return Err(WorkerFailure {
                    code: "worker_protocol_failed".into(),
                    message: "worker returned a run completion for a tool request".into(),
                });
            }
            Ok(_) => {}
            Err(err) => {
                let status = wait_child_or_kill(&mut worker.child, Duration::from_secs(1)).ok();
                return Err(worker_crashed(
                    &format!("invalid worker event JSON: {err}"),
                    status,
                    &worker.stderr_tail,
                    &last_event,
                ));
            }
        }
    }
}

#[cfg(test)]
fn supervise_worker_job(job: WorkerJob) -> Result<WorkerCompleted, WorkerFailure> {
    match job {
        WorkerJob::Run { source } => {
            let (name, report) = worker_compile(source.into())?;
            worker_run_compiled(name, report)
        }
        WorkerJob::RunAgentText {
            source,
            responses,
            default_response,
        } => {
            let (name, report) = worker_compile(source.into())?;
            worker_run_compiled_agent_text(name, report, responses, default_response)
        }
        WorkerJob::LiveAgent { .. } => Err(WorkerFailure {
            code: "worker_mode_unsupported".into(),
            message: "live sessions are started with agent_session_start".into(),
        }),
        WorkerJob::Hello
        | WorkerJob::HelpOverview
        | WorkerJob::HelpSearch { .. }
        | WorkerJob::HelpTopic { .. }
        | WorkerJob::HelpService { .. }
        | WorkerJob::HelpRemind { .. }
        | WorkerJob::ListStdlibServices
        | WorkerJob::Check { .. }
        | WorkerJob::EmitIr { .. }
        | WorkerJob::EmitAsm { .. }
        | WorkerJob::Build { .. }
        | WorkerJob::Effects { .. } => Err(WorkerFailure {
            code: "worker_mode_unsupported".into(),
            message: "use the worker tool path for compiler/help requests".into(),
        }),
    }
}

#[cfg(not(test))]
fn supervise_worker_job(job: WorkerJob) -> Result<WorkerCompleted, WorkerFailure> {
    supervise_worker_job_process(job)
}

#[cfg(not(test))]
fn supervise_worker_job_process(job: WorkerJob) -> Result<WorkerCompleted, WorkerFailure> {
    let mut worker = spawn_worker_process().map_err(|message| WorkerFailure {
        code: "worker_spawn_failed".into(),
        message,
    })?;
    write_json_line(&mut worker.stdin, &job).map_err(|message| WorkerFailure {
        code: "worker_protocol_failed".into(),
        message,
    })?;
    drop(worker.stdin);

    let mut reader = BufReader::new(worker.stdout);
    let mut last_event = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| WorkerFailure {
            code: "worker_protocol_failed".into(),
            message: format!("reading worker event: {e}"),
        })?;
        if n == 0 {
            let status = wait_child_or_kill(&mut worker.child, Duration::from_secs(1)).ok();
            return Err(worker_crashed(
                "worker closed stdout before completion",
                status,
                &worker.stderr_tail,
                &last_event,
            ));
        }
        last_event = line.trim_end().to_string();
        match serde_json::from_str::<WorkerEvent>(&line) {
            Ok(WorkerEvent::Completed {
                result,
                agent_transcript,
            }) => {
                let _ = wait_child_or_kill(&mut worker.child, Duration::from_secs(1));
                return Ok(WorkerCompleted {
                    result,
                    agent_transcript,
                });
            }
            Ok(WorkerEvent::Failed { code, message }) => {
                let _ = wait_child_or_kill(&mut worker.child, Duration::from_secs(1));
                return Err(WorkerFailure { code, message });
            }
            Ok(WorkerEvent::ToolCompleted { .. }) => {
                let _ = wait_child_or_kill(&mut worker.child, Duration::from_secs(1));
                return Err(WorkerFailure {
                    code: "worker_protocol_failed".into(),
                    message: "worker returned a tool completion for a run request".into(),
                });
            }
            Ok(_) => {}
            Err(err) => {
                let status = wait_child_or_kill(&mut worker.child, Duration::from_secs(1)).ok();
                return Err(worker_crashed(
                    &format!("invalid worker event JSON: {err}"),
                    status,
                    &worker.stderr_tail,
                    &last_event,
                ));
            }
        }
    }
}

#[cfg(not(test))]
struct SpawnedWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_tail: Arc<Mutex<String>>,
}

#[cfg(not(test))]
fn spawn_worker_process() -> Result<SpawnedWorker, String> {
    let mut child = Command::new(worker_exe_path()?)
        .arg("worker")
        .current_dir(std::env::current_dir().map_err(|e| e.to_string())?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting worker process: {e}"))?;
    let stdin = child.stdin.take().ok_or("worker stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("worker stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("worker stderr unavailable")?;
    let stderr_tail = spawn_stderr_tail(stderr);
    Ok(SpawnedWorker {
        child,
        stdin,
        stdout,
        stderr_tail,
    })
}

#[cfg(not(test))]
fn spawn_stderr_tail(mut stderr: ChildStderr) -> Arc<Mutex<String>> {
    let tail = Arc::new(Mutex::new(String::new()));
    let thread_tail = tail.clone();
    let _ = thread::Builder::new()
        .name("locus-worker-stderr".into())
        .spawn(move || {
            let mut buf = [0_u8; 1024];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        push_text_tail(&thread_tail, &text);
                    }
                }
            }
        });
    tail
}

#[cfg(not(test))]
fn push_text_tail(tail: &Arc<Mutex<String>>, text: &str) {
    const MAX_TAIL_BYTES: usize = 8192;
    let mut guard = tail.lock().expect("worker stderr tail poisoned");
    guard.push_str(text);
    if guard.len() > MAX_TAIL_BYTES {
        let keep_from = guard.len() - MAX_TAIL_BYTES;
        let trimmed = guard[keep_from..].to_string();
        *guard = trimmed;
    }
}

fn write_json_line<W, T>(writer: &mut W, value: &T) -> Result<(), String>
where
    W: Write,
    T: Serialize,
{
    serde_json::to_writer(&mut *writer, value).map_err(|e| format!("encoding JSON: {e}"))?;
    writeln!(writer).map_err(|e| format!("writing JSON line: {e}"))
}

#[cfg(not(test))]
fn wait_child_or_kill(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return child.wait().map_err(|e| e.to_string());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(test))]
fn worker_crashed(
    message: &str,
    status: Option<ExitStatus>,
    stderr_tail: &Arc<Mutex<String>>,
    last_event: &str,
) -> WorkerFailure {
    let mut full = format!(
        "{message}; worker exit status: {}",
        status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "<unknown>".into())
    );
    if !last_event.is_empty() {
        full.push_str("; last worker event: ");
        full.push_str(last_event);
    }
    let stderr = stderr_tail
        .lock()
        .expect("worker stderr tail poisoned")
        .trim()
        .to_string();
    if !stderr.is_empty() {
        full.push_str("; stderr tail: ");
        full.push_str(&stderr);
    }
    WorkerFailure {
        code: "worker_crashed".into(),
        message: full,
    }
}

fn live_sessions() -> &'static Mutex<HashMap<String, Arc<LiveSession>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<LiveSession>>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_session(session_id: &str) -> Option<Arc<LiveSession>> {
    live_sessions()
        .lock()
        .expect("live session registry poisoned")
        .get(session_id)
        .cloned()
}

fn next_live_session_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("agent-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
fn start_in_process_live_session(
    session_id: &str,
    unit: SourceUnit,
    session: Arc<LiveSession>,
) -> Result<(), String> {
    let run_session_id = session_id.to_string();
    thread::Builder::new()
        .name(format!("locus-agent-session-{run_session_id}"))
        .stack_size(locus::PIPELINE_STACK_BYTES)
        .spawn(move || run_live_agent_session(unit, session))
        .map(|_| ())
        .map_err(|err| format!("spawning live session worker: {err}"))
}

#[cfg(not(test))]
fn start_worker_live_session(
    session_id: &str,
    unit: SourceUnit,
    session: Arc<LiveSession>,
) -> Result<(), String> {
    let mut worker = spawn_worker_process()?;
    write_json_line(
        &mut worker.stdin,
        &WorkerJob::LiveAgent {
            source: unit.into(),
        },
    )?;
    let child = Arc::new(Mutex::new(worker.child));
    let stderr_tail = worker.stderr_tail.clone();
    {
        let mut slot = session.worker.lock().expect("live worker slot poisoned");
        *slot = Some(LiveWorkerHandle {
            child: child.clone(),
            stdin: Some(worker.stdin),
            stderr_tail: stderr_tail.clone(),
        });
    }
    let reader_session = session.clone();
    let reader_session_id = session_id.to_string();
    thread::Builder::new()
        .name(format!("locus-agent-session-{reader_session_id}"))
        .spawn(move || {
            live_worker_stdout_loop(
                reader_session_id,
                reader_session,
                worker.stdout,
                child,
                stderr_tail,
            )
        })
        .map(|_| ())
        .map_err(|err| format!("spawning live session monitor: {err}"))
}

#[cfg(not(test))]
fn live_worker_stdout_loop(
    _session_id: String,
    session: Arc<LiveSession>,
    stdout: ChildStdout,
    child: Arc<Mutex<Child>>,
    stderr_tail: Arc<Mutex<String>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut last_event = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let status = child
                    .lock()
                    .expect("live worker child poisoned")
                    .try_wait()
                    .ok()
                    .flatten();
                fail_live_session_if_running(
                    &session,
                    worker_crashed(
                        "live worker closed stdout before completion",
                        status,
                        &stderr_tail,
                        &last_event,
                    )
                    .message,
                );
                break;
            }
            Ok(_) => {
                last_event = line.trim_end().to_string();
                match serde_json::from_str::<WorkerEvent>(&line) {
                    Ok(WorkerEvent::Started { .. })
                    | Ok(WorkerEvent::CompileStarted)
                    | Ok(WorkerEvent::CompileFinished { .. })
                    | Ok(WorkerEvent::RunStarted) => {
                        session.cv.notify_all();
                    }
                    Ok(WorkerEvent::Tell { text }) => {
                        let mut state = session.state.lock().expect("live session state poisoned");
                        if state.status != LiveStatus::Closed {
                            state.events.push(LiveEvent::Tell { text });
                            session.cv.notify_all();
                        }
                    }
                    Ok(WorkerEvent::Ask { prompt }) => {
                        let mut state = session.state.lock().expect("live session state poisoned");
                        if state.status != LiveStatus::Closed {
                            state.events.push(LiveEvent::Ask {
                                prompt,
                                response: None,
                                used_default: false,
                            });
                            state.status = LiveStatus::Waiting;
                            session.cv.notify_all();
                        }
                    }
                    Ok(WorkerEvent::Completed {
                        result,
                        agent_transcript: _,
                    }) => {
                        let mut state = session.state.lock().expect("live session state poisoned");
                        if state.status != LiveStatus::Closed {
                            state.status = LiveStatus::Completed;
                            state.result = Some(result.into());
                        }
                        session.cv.notify_all();
                        break;
                    }
                    Ok(WorkerEvent::ToolCompleted { .. }) => {
                        fail_live_session_if_running(
                            &session,
                            "live worker returned a tool completion for a live session".into(),
                        );
                        break;
                    }
                    Ok(WorkerEvent::Failed { code: _, message }) => {
                        fail_live_session_if_running(&session, message);
                        break;
                    }
                    Err(err) => {
                        let failure = worker_crashed(
                            &format!("invalid live worker event JSON: {err}"),
                            None,
                            &stderr_tail,
                            &last_event,
                        );
                        fail_live_session_if_running(&session, failure.message);
                        break;
                    }
                }
            }
            Err(err) => {
                fail_live_session_if_running(&session, format!("reading live worker event: {err}"));
                break;
            }
        }
    }
    let _ = wait_child_or_kill(
        &mut child.lock().expect("live worker child poisoned"),
        Duration::from_secs(1),
    );
}

#[cfg(not(test))]
fn fail_live_session_if_running(session: &Arc<LiveSession>, message: String) {
    let mut state = session.state.lock().expect("live session state poisoned");
    if !matches!(
        state.status,
        LiveStatus::Completed | LiveStatus::Failed | LiveStatus::Closed
    ) {
        state.status = LiveStatus::Failed;
        state.error = Some(message);
    }
    session.cv.notify_all();
}

#[cfg(not(test))]
fn live_worker_reply(session: &Arc<LiveSession>, response: &str) -> Result<(), String> {
    let mut worker_slot = session.worker.lock().expect("live worker slot poisoned");
    let Some(worker) = worker_slot.as_mut() else {
        return Err("live session has no worker process".into());
    };
    let Some(stdin) = worker.stdin.as_mut() else {
        return Err("live session worker input is closed".into());
    };
    write_json_line(
        stdin,
        &WorkerCommand::Reply {
            response: response.to_string(),
        },
    )
    .and_then(|_| {
        stdin
            .flush()
            .map_err(|e| format!("flushing live worker reply: {e}"))
    })
}

fn close_live_worker(session: &Arc<LiveSession>) {
    let mut worker_slot = session.worker.lock().expect("live worker slot poisoned");
    let Some(mut worker) = worker_slot.take() else {
        return;
    };
    if let Some(mut stdin) = worker.stdin.take() {
        let _ = write_json_line(&mut stdin, &WorkerCommand::Cancel);
        let _ = stdin.flush();
    }
    let mut child = worker.child.lock().expect("live worker child poisoned");
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    let _ = worker
        .stderr_tail
        .lock()
        .expect("worker stderr tail poisoned")
        .len();
}

#[cfg(test)]
fn run_live_agent_session(unit: SourceUnit, session: Arc<LiveSession>) {
    let result = compile_pipeline(unit.source, unit.authorized).and_then(|report| {
        let demanded = demanded_map(&report.demanded);
        let callback_session = session.clone();
        let callback = move |event| live_agent_callback(&callback_session, event);
        let (run_result, _transcript) = crate::runtime::with_agent_live_session(callback, || {
            crate::jit_run_i64(&report.ir, &demanded)
        });
        let value = run_result?;
        Ok(LiveRunResult {
            result_i64: value,
            exit_code: value as i32,
            ty: report.ty,
            effects: report.effects,
            demanded: report.demanded,
        })
    });

    let mut state = session.state.lock().expect("live session state poisoned");
    if state.status != LiveStatus::Closed {
        match result {
            Ok(result) => {
                state.status = LiveStatus::Completed;
                state.result = Some(result);
            }
            Err(err) => {
                state.status = LiveStatus::Failed;
                state.error = Some(err);
            }
        }
    }
    session.cv.notify_all();
}

#[cfg(test)]
fn live_agent_callback(
    session: &Arc<LiveSession>,
    event: crate::runtime::AgentHostEvent,
) -> Option<String> {
    match event {
        crate::runtime::AgentHostEvent::Tell { text } => {
            let mut state = session.state.lock().expect("live session state poisoned");
            if state.status != LiveStatus::Closed {
                state.events.push(LiveEvent::Tell { text });
                session.cv.notify_all();
            }
            None
        }
        crate::runtime::AgentHostEvent::Ask { prompt } => {
            let mut state = session.state.lock().expect("live session state poisoned");
            if state.status == LiveStatus::Closed {
                return Some(String::new());
            }
            let event_index = state.events.len();
            state.events.push(LiveEvent::Ask {
                prompt,
                response: None,
                used_default: false,
            });
            state.status = LiveStatus::Waiting;
            session.cv.notify_all();

            loop {
                if let Some(response) = state.pending_response.take() {
                    if let Some(LiveEvent::Ask { response: slot, .. }) =
                        state.events.get_mut(event_index)
                    {
                        *slot = Some(response.clone());
                    }
                    state.status = LiveStatus::Running;
                    session.cv.notify_all();
                    return Some(response);
                }
                if matches!(state.status, LiveStatus::Closed | LiveStatus::Failed) {
                    if let Some(LiveEvent::Ask {
                        response,
                        used_default,
                        ..
                    }) = state.events.get_mut(event_index)
                    {
                        *response = Some(String::new());
                        *used_default = true;
                    }
                    session.cv.notify_all();
                    return Some(String::new());
                }
                state = session.cv.wait(state).expect("live session state poisoned");
            }
        }
    }
}

fn live_session_wait_json(
    session_id: &str,
    session: &Arc<LiveSession>,
    wait_ms: u64,
    since_event_index: Option<usize>,
) -> serde_json::Value {
    if wait_ms == 0 {
        return live_session_snapshot_json(session_id, session, since_event_index);
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut state = session.state.lock().expect("live session state poisoned");
    while state.status == LiveStatus::Running {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let timeout = deadline.saturating_duration_since(now);
        let (next_state, wait_result) = session
            .cv
            .wait_timeout(state, timeout)
            .expect("live session state poisoned");
        state = next_state;
        if wait_result.timed_out() {
            break;
        }
    }
    live_session_json_from_state(session_id, &state, since_event_index)
}

fn live_session_snapshot_json(
    session_id: &str,
    session: &Arc<LiveSession>,
    since_event_index: Option<usize>,
) -> serde_json::Value {
    let state = session.state.lock().expect("live session state poisoned");
    live_session_json_from_state(session_id, &state, since_event_index)
}

fn live_session_json_from_state(
    session_id: &str,
    state: &LiveState,
    since_event_index: Option<usize>,
) -> serde_json::Value {
    let mut asks = Vec::new();
    let mut tells = Vec::new();
    let mut current_ask = serde_json::Value::Null;
    let mut latest_ask = serde_json::Value::Null;
    let mut latest_tell = serde_json::Value::Null;
    let mut latest_score = serde_json::Value::Null;
    let mut latest_move_result = serde_json::Value::Null;
    let total_event_count = state.events.len();
    let window_start = since_event_index.unwrap_or(0).min(total_event_count);
    let latest_event_index = total_event_count.checked_sub(1);

    for (index, event) in state.events.iter().enumerate() {
        match event {
            LiveEvent::Ask {
                prompt,
                response,
                used_default,
            } => {
                let ask = json!({
                    "event_index": index,
                    "prompt": prompt,
                    "response": response,
                    "used_default": used_default,
                });
                latest_ask = ask.clone();
                if response.is_none() {
                    current_ask = ask;
                }
            }
            LiveEvent::Tell { text } => {
                let tell = json!({
                    "event_index": index,
                    "text": text,
                });
                latest_tell = tell.clone();
                if text.starts_with("score ") {
                    latest_score = tell.clone();
                }
                if text.starts_with("accepted ")
                    || text.starts_with("illegal or missing ")
                    || text.starts_with("Black plays ")
                    || text.starts_with("Black passes")
                {
                    latest_move_result = tell;
                }
            }
        }
    }

    let events: Vec<serde_json::Value> = state
        .events
        .iter()
        .enumerate()
        .filter(|(index, _)| *index >= window_start)
        .map(|(index, event)| match event {
            LiveEvent::Ask {
                prompt,
                response,
                used_default,
            } => {
                let ask = json!({
                    "event_index": index,
                    "prompt": prompt,
                    "response": response,
                    "used_default": used_default,
                });
                asks.push(ask);
                json!({
                    "kind": "ask",
                    "prompt": prompt,
                    "response": response,
                    "used_default": used_default,
                })
            }
            LiveEvent::Tell { text } => {
                let tell = json!({
                    "event_index": index,
                    "text": text,
                });
                tells.push(tell);
                json!({
                    "kind": "tell",
                    "text": text,
                })
            }
        })
        .collect();
    let window_event_count = events.len();

    let result = state.result.as_ref().map(|result| {
        json!({
            "result_i64": result.result_i64,
            "exit_code": result.exit_code,
            "type": result.ty,
            "effects": result.effects,
            "demanded_apis": result.demanded,
        })
    });

    json!({
        "session_id": session_id,
        "name": &state.name,
        "status": state.status.as_str(),
        "agent_io_model": "live_session",
        "agent_io_hint": "If status is waiting, call agent_session_reply with the current_ask response. Pass since_event_index = latest_event_index + 1 to receive only new transcript events. Relative file paths resolve from workspace_cwd.",
        "workspace_cwd": workspace_cwd(),
        "current_ask": current_ask,
        "latest_ask": latest_ask,
        "latest_tell": latest_tell,
        "latest_score": latest_score,
        "latest_move_result": latest_move_result,
        "events": events,
        "event_count": window_event_count,
        "event_window_start": window_start,
        "total_event_count": total_event_count,
        "latest_event_index": latest_event_index,
        "asks": asks,
        "tells": tells,
        "result": result,
        "error": &state.error,
    })
}

fn workspace_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into())
}

fn read_boundary_manifest(source_file: &Path) -> HashSet<String> {
    let mut dir = source_file.parent();
    while let Some(d) = dir {
        let candidate = d.join("locus.toml");
        if candidate.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                return parse_boundary_modules(&text);
            }
            break;
        }
        dir = d.parent();
    }
    HashSet::new()
}

fn parse_boundary_modules(toml: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(start) = toml.find("[boundary]") else {
        return out;
    };
    let after = &toml[start + "[boundary]".len()..];
    let section = match after.find("\n[") {
        Some(i) => &after[..i],
        None => after,
    };
    let Some(m) = section.find("modules") else {
        return out;
    };
    let rest = &section[m..];
    let Some(lb) = rest.find('[') else {
        return out;
    };
    let Some(rb_off) = rest[lb..].find(']') else {
        return out;
    };
    let array = &rest[lb + 1..lb + rb_off];
    let mut cur = String::new();
    let mut in_str = false;
    for ch in array.chars() {
        match ch {
            '"' if in_str => {
                out.insert(std::mem::take(&mut cur));
                in_str = false;
            }
            '"' => in_str = true,
            _ if in_str => cur.push(ch),
            _ => {}
        }
    }
    out
}

fn guard_layer2(src: &str, authorized: &HashSet<String>) -> Result<(), String> {
    let prog = locus::parse_program(src).map_err(|e| e.msg)?;
    locus::mint_gate(&prog.entry, &prog.modules, authorized)
        .map_err(|e| format!("[{}] {e}", e.code()))
}

fn demanded_map(demanded: &[DemandEntry]) -> crate::winapi_resolve::Demanded {
    demanded
        .iter()
        .map(|d| (d.symbol.clone(), d.dll.clone()))
        .collect()
}

fn agent_transcript_json(transcript: &crate::runtime::AgentTranscript) -> serde_json::Value {
    let mut asks = Vec::new();
    let mut tells = Vec::new();
    let events: Vec<serde_json::Value> = transcript
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| match event {
            crate::runtime::AgentEvent::Ask {
                prompt,
                response,
                used_default,
            } => {
                asks.push(json!({
                    "event_index": index,
                    "prompt": prompt,
                    "response": response,
                    "used_default": used_default,
                }));
                json!({
                    "kind": "ask",
                    "prompt": prompt,
                    "response": response,
                    "used_default": used_default,
                })
            }
            crate::runtime::AgentEvent::Tell { text } => {
                tells.push(json!({
                    "event_index": index,
                    "text": text,
                }));
                json!({
                    "kind": "tell",
                    "text": text,
                })
            }
        })
        .collect();
    let event_count = events.len();
    json!({
        "mode": "queued_replay",
        "events": events,
        "event_count": event_count,
        "asks": asks,
        "tells": tells,
        "remaining_responses": transcript.remaining_responses,
    })
}

fn help_card_json_value(card: &locus::help::HelpCard) -> serde_json::Value {
    json!({
        "id": card.id,
        "kind": card.kind,
        "title": card.title,
        "summary": card.summary,
        "syntax": card.syntax,
        "example": card.example,
        "details": card.details,
        "related": card.related,
        "keywords": card.keywords,
    })
}

fn help_overview_value() -> serde_json::Value {
    let topics: Vec<serde_json::Value> = locus::help::TOPICS
        .iter()
        .map(help_card_json_value)
        .collect();
    let services: Vec<serde_json::Value> = locus::help::SERVICES
        .iter()
        .filter(|card| !card.id.matches('.').nth(1).is_some())
        .map(help_card_json_value)
        .collect();
    json!({
        "schema": "locus-help/1",
        "kind": "overview",
        "default_format": "json",
        "human_flag": "--human",
        "workspace_cwd": workspace_cwd(),
        "agent_start": "locusc help agent",
        "topics": topics,
        "services": services,
        "worker": worker_info_value(),
    })
}

fn help_search_value(query: &str, limit: Option<usize>) -> serde_json::Value {
    let hits = locus::help::search(query, limit.unwrap_or(8));
    let results: Vec<serde_json::Value> = hits
        .iter()
        .map(|hit| {
            json!({
                "score": hit.score,
                "card": help_card_json_value(hit.card),
            })
        })
        .collect();
    json!({
        "schema": "locus-help/1",
        "kind": "search",
        "query": query,
        "results": results,
        "worker": worker_info_value(),
    })
}

fn help_topic_value(id: &str) -> Result<serde_json::Value, WorkerFailure> {
    let Some(card) = locus::help::find(id) else {
        return Err(WorkerFailure {
            code: "help_not_found".into(),
            message: format!("unknown help topic `{id}` (try help_search)"),
        });
    };
    Ok(json!({
        "schema": "locus-help/1",
        "kind": "topic",
        "card": help_card_json_value(card),
        "worker": worker_info_value(),
    }))
}

fn help_service_value(name: &str) -> Result<serde_json::Value, WorkerFailure> {
    let Some(card) = locus::help::service(name) else {
        return Err(WorkerFailure {
            code: "help_not_found".into(),
            message: format!("unknown service `{name}` (try help_search)"),
        });
    };
    Ok(json!({
        "schema": "locus-help/1",
        "kind": "service",
        "card": help_card_json_value(card),
        "worker": worker_info_value(),
    }))
}

fn help_remind_value(topic: &str) -> serde_json::Value {
    if let Some(card) = locus::help::find(topic)
        .or_else(|| locus::help::search(topic, 1).first().map(|hit| hit.card))
    {
        json!({
            "schema": "locus-help/1",
            "kind": "reminder",
            "query": topic,
            "card": help_card_json_value(card),
            "worker": worker_info_value(),
        })
    } else {
        json!({
            "schema": "locus-help/1",
            "kind": "reminder",
            "query": topic,
            "error": "no matching help card",
            "worker": worker_info_value(),
        })
    }
}

fn list_stdlib_services_value() -> serde_json::Value {
    json!({
        "schema": "locus-stdlib-services/1",
        "workspace_cwd": workspace_cwd(),
        "worker": worker_info_value(),
        "windows": stdlib_entries("windows", locus::stdlib_modules()),
        "linux": stdlib_entries("linux", locus::linux_stdlib_modules()),
    })
}

fn worker_info_value() -> serde_json::Value {
    serde_json::to_value(worker_info()).unwrap_or_else(|_| {
        json!({
            "protocol": "locus-worker/1",
            "worker_version": env!("CARGO_PKG_VERSION"),
            "worker_path": "<unavailable>",
        })
    })
}

fn worker_info() -> WorkerInfo {
    let windows_hash = stdlib_hash("windows", locus::stdlib_modules());
    let linux_hash = stdlib_hash("linux", locus::linux_stdlib_modules());
    let stdlib_hash = format!(
        "{:#018x}",
        fnv1a(&format!("windows:{windows_hash};linux:{linux_hash}"))
    );
    let stdlib_module_count = locus::stdlib_modules().len() + locus::linux_stdlib_modules().len();
    WorkerInfo {
        protocol: "locus-worker/1".into(),
        worker_version: env!("CARGO_PKG_VERSION").into(),
        worker_path: current_worker_path_display(),
        target_os: std::env::consts::OS.into(),
        target_arch: std::env::consts::ARCH.into(),
        abi_version: locus::ABI_VERSION,
        diagnostic_schema: locus::SCHEMA.into(),
        stdlib_hash: stdlib_hash.clone(),
        windows_stdlib_hash: windows_hash,
        linux_stdlib_hash: linux_hash,
        stdlib_module_count,
        language_revision: format!("abi{}-stdlib-{stdlib_hash}", locus::ABI_VERSION),
    }
}

fn stdlib_hash(platform: &'static str, modules: &'static [locus::stdlib::ModuleSource]) -> String {
    let mut text = String::new();
    for (layer, name, source) in modules {
        text.push_str(platform);
        text.push('\0');
        text.push_str(&layer.to_string());
        text.push('\0');
        text.push_str(name);
        text.push('\0');
        text.push_str(&source.len().to_string());
        text.push('\0');
        text.push_str(source);
        text.push('\0');
    }
    format!("{:#018x}", fnv1a(&text))
}

fn supervisor_info_value() -> serde_json::Value {
    json!({
        "protocol": "locus-mcp-supervisor/1",
        "supervisor_version": env!("CARGO_PKG_VERSION"),
        "supervisor_path": current_exe_display(),
        "worker_env_var": "LOCUS_WORKER_EXE",
        "configured_worker_path": configured_worker_path_display(),
        "worker_update_hint": "Rebuild or replace the worker binary, or set LOCUS_WORKER_EXE to a fresh locusc-compatible worker; new MCP requests use that worker without restarting the supervisor.",
    })
}

fn insert_object_field(value: &mut serde_json::Value, key: &str, field: serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            fields.insert(key.to_string(), field);
        }
        other => {
            let original = std::mem::replace(other, serde_json::Value::Null);
            *other = json!({
                "value": original,
                key: field,
            });
        }
    }
}

#[cfg(not(test))]
fn worker_exe_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("LOCUS_WORKER_EXE") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    std::env::current_exe().map_err(|e| e.to_string())
}

#[cfg(test)]
fn worker_exe_path() -> Result<PathBuf, String> {
    Ok(PathBuf::from("<in-process-worker>"))
}

fn configured_worker_path_display() -> String {
    match worker_exe_path() {
        Ok(path) => path.display().to_string(),
        Err(err) => format!("<unavailable: {err}>"),
    }
}

fn current_worker_path_display() -> String {
    #[cfg(test)]
    {
        "<in-process-worker>".into()
    }
    #[cfg(not(test))]
    {
        configured_worker_path_display()
    }
}

fn current_exe_display() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|err| format!("<unavailable: {err}>"))
}

fn effect_entry(label: &locus::Label) -> EffectEntry {
    EffectEntry {
        label: format!("{label}"),
        category: category(label).to_string(),
        description: describe(label).to_string(),
    }
}

struct Catalog {
    by_label: HashMap<String, (String, String)>,
    by_kind: HashMap<String, (String, String)>,
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut by_label = HashMap::new();
        let mut by_kind = HashMap::new();
        for line in EFFECT_CATALOG.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let tok: Vec<&str> = line.split_whitespace().collect();
            if tok[0] == "order" || tok.len() < 3 {
                continue;
            }
            let entry = (tok[1].to_string(), tok[2..].join(" "));
            if let Some(kind) = tok[0].strip_prefix("kind:") {
                by_kind.insert(kind.to_string(), entry);
            } else {
                by_label.insert(tok[0].to_string(), entry);
            }
        }
        Catalog { by_label, by_kind }
    })
}

fn label_kind(l: &locus::Label) -> &'static str {
    use locus::Label::*;
    match l {
        World(_) => "world",
        User(_) => "user",
        Exn(_) => "exn",
        Gc => "gc",
        St => "state",
        Insert => "staging",
    }
}

fn lookup(l: &locus::Label) -> (&'static str, &'static str) {
    let catalog = catalog();
    let name = format!("{l}");
    if let Some((category, gloss)) = catalog.by_label.get(&name) {
        return (category.as_str(), gloss.as_str());
    }
    if let Some((category, gloss)) = catalog.by_kind.get(label_kind(l)) {
        return (category.as_str(), gloss.as_str());
    }
    ("user", "effect")
}

fn category(l: &locus::Label) -> &'static str {
    lookup(l).0
}

fn describe(l: &locus::Label) -> &'static str {
    lookup(l).1
}

fn stdlib_entries(
    platform: &'static str,
    modules: &'static [locus::stdlib::ModuleSource],
) -> Vec<StdlibModuleEntry> {
    modules
        .iter()
        .map(|(layer, name, source)| StdlibModuleEntry {
            platform,
            layer: *layer,
            name,
            boundary: *layer == 0,
            bytes: source.len(),
            fnv1a64: format!("{:#018x}", fnv1a(source)),
        })
        .collect()
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn diagnostic_explanation(code: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    match code {
        "RN-E0101" => (
            "scope",
            "scope.unbound",
            "A name is not in scope at the point it is used.",
            "Import or define the name, or check that a stdlib service exposing it was triggered.",
        ),
        "RN-E0201" => (
            "type",
            "type.not-a-function",
            "A value was applied as a function, but its type is not an arrow.",
            "Check the callee expression and add an annotation if inference picked the wrong shape.",
        ),
        "RN-E0203" => (
            "type",
            "type.mismatch",
            "Two types that must be equal do not unify.",
            "Read the expected/actual types in the diagnostic and adapt the expression or annotation.",
        ),
        "RN-E0227" => (
            "type",
            "kind.wide-into-traced-slot",
            "A representation-polymorphic value would enter a traced storage slot.",
            "Monomorphize the value or store it in a representation-known container.",
        ),
        "RN-E0230" => (
            "traits",
            "trait.no-instance",
            "No trait instance can satisfy a required constraint.",
            "Import or write the needed instance, or add a type annotation that selects one.",
        ),
        "RN-E0231" => (
            "traits",
            "trait.overlapping-instances",
            "Two trait instances overlap for the same obligation.",
            "Remove or narrow one instance; v1 requires coherence.",
        ),
        "RN-E0232" => (
            "traits",
            "trait.orphan-instance",
            "An instance is declared outside both the trait's module and the type's module.",
            "Move it to an owning module or wrap the type in a local newtype.",
        ),
        "RN-E0233" => (
            "traits",
            "trait.resolution-diverges",
            "Trait resolution is not structurally decreasing.",
            "Make each required constraint smaller than the instance head.",
        ),
        "RN-E0234" => (
            "traits",
            "trait.ambiguous",
            "A trait constraint has an undetermined type variable.",
            "Add a type annotation; Locus does not default ambiguous constraints.",
        ),
        "RN-E0235" => (
            "traits",
            "trait.no-method",
            "A name was used as a trait method, but no in-scope trait declares it.",
            "Import the trait or check the method spelling.",
        ),
        "RN-E0236" => (
            "traits",
            "trait.superclass-unsatisfied",
            "A superclass obligation for an instance is not satisfied.",
            "Provide the superclass instance before relying on the child trait.",
        ),
        "RN-E0237" => (
            "traits",
            "trait.duplicate-instance",
            "The same trait instance is declared twice.",
            "Delete or merge the duplicate instance.",
        ),
        "RN-E0238" => (
            "traits",
            "trait.method-row-violation",
            "An instance method performs effects beyond the trait method row.",
            "Widen the trait method row or remove the effect from the implementation.",
        ),
        "RN-E0239" => (
            "traits",
            "trait.missing-method",
            "An instance is missing a method or declares one the trait does not have.",
            "Implement exactly the trait's declared methods.",
        ),
        "RN-E0241" => (
            "mutability",
            "mut.escapes",
            "A mutable local would escape its scope.",
            "Keep the mutable cell local or move the state into an explicit managed structure.",
        ),
        "RN-E0244" => (
            "mutability",
            "mut.non-scalar",
            "A v1 mutable local was requested for a non-scalar value.",
            "Use scalar mutability today, or model the value through arrays/refs.",
        ),
        "RN-E0245" => (
            "mutability",
            "mut.assign-immutable",
            "An assignment targets a binding that is not mutable.",
            "Declare it with `let mut`, or stop assigning to it.",
        ),
        "RN-E0246" => (
            "traits",
            "trait.v1-unsupported",
            "A well-typed trait construct is outside the v1 lowering scope.",
            "Monomorphize or simplify the construct, or defer until recursive/generic instance lowering lands.",
        ),
        "RN-E0247" => (
            "mutability",
            "ref.pointer-content",
            "A Ref would contain GC-managed pointer data that needs a write barrier.",
            "Keep Ref contents scalar for now, or use a managed container surface.",
        ),
        "RN-E0301" => (
            "staging",
            "stage.escape",
            "A staged value escapes the phase where it is valid.",
            "Keep generated code inside the staging boundary that created it.",
        ),
        "RN-E0302" => (
            "staging",
            "stage.misuse",
            "A staging construct was used at the wrong phase or on the wrong kind of value.",
            "Move the quote/splice/genlet boundary or change the value to `Code`.",
        ),
        "RN-E0401" => (
            "capability",
            "extern.bare",
            "A bare extern reached the core checker without an oracle signature.",
            "Use `locusc` so the oracle can resolve it, or write an explicit extern type.",
        ),
        "RN-E0402" => (
            "capability",
            "cap.mint-outside-boundary",
            "Raw capability minting appeared outside an authorized boundary module.",
            "Move extern/raw-memory code to layer 0 and authorize that module in `locus.toml`.",
        ),
        "RN-E0403" => (
            "capability",
            "cap.seal-leak",
            "A sealed effect label escapes a seal/module boundary.",
            "Handle or discharge the effect before exposing the value.",
        ),
        "RN-E0404" => (
            "capability",
            "cap.unauthorized-boundary",
            "A module claims `at boundary` but is not authorized by the manifest.",
            "List the module under `[boundary].modules` or remove the boundary claim.",
        ),
        "RN-E0405" => (
            "capability",
            "cap.asm-gc-type / capability.level-out-of-layer",
            "Either: an extern-asm signature crosses a GC-managed type; OR a \
             level-visibility OUT-OF-LAYER reference (a use site names a binding \
             bound only at a layer it cannot reach — e.g. an app naming a boundary \
             binding two layers down, or any upward reference).",
            "For asm: keep signatures GC-blind (scalars/raw pointers). For level: \
             reach the binding through a sealed service that exposes a capability.",
        ),
        "RN-E0406" => (
            "capability",
            "capability.level-not-exposed",
            "A level-visibility NOT-EXPOSED reference: the named binding IS at a \
             reachable layer but is private (not in its module's `exposing`).",
            "Add the name to its module's `exposing (…)`, or reach it through an \
             exposed capability.",
        ),
        "RN-E0407" => (
            "capability",
            "capability.non-sealable-effect",
            "A never-sealable effect (`gc`, `exn`, or `Insert`) was named in a \
             module `seals (…)` clause or a region `seal L { … }` — the inverted \
             denylist; sealing one would hide a fault or break let-insertion.",
            "Drop `gc`/`exn`/`insert` from the seal; seal only native powers \
             (`winapi`/`mem`/…) you actually discharge.",
        ),
        "RN-E0600" => (
            "module",
            "module.stale-interface",
            "An interface hash or shape does not match the consumer's expectation.",
            "Regenerate the interface and rebuild dependents.",
        ),
        "RN-E0601" => (
            "module",
            "module.missing-export",
            "A module import requests a name the interface does not export.",
            "Export the name or fix the import list.",
        ),
        "RN-E0603" => (
            "module",
            "module.abi-version",
            "An interface was built for a different ABI/representation version.",
            "Rebuild the producer interface with this compiler.",
        ),
        "RN-E0604" => (
            "module",
            "module.import-cycle",
            "Module imports form a cycle.",
            "Break the cycle by moving shared definitions to a lower module.",
        ),
        _ => (
            "unknown",
            "unknown",
            "This diagnostic code is not in the MCP server's compact explanation table.",
            "Run `check` to get the full compiler message, then add this code to the catalog if it should be stable.",
        ),
    }
}

fn tool_ok(value: serde_json::Value) -> CallToolResult {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".into(), json!(true));
    match value {
        serde_json::Value::Object(fields) => obj.extend(fields),
        other => {
            obj.insert("value".into(), other);
        }
    }
    CallToolResult::structured(serde_json::Value::Object(obj))
}

fn tool_error(code: impl Into<String>, message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "error": {
            "code": code.into(),
            "message": message.into(),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_request_requires_one_source() {
        assert!(source_unit(None, None, None).is_err());
        assert!(source_unit(Some("x.locus".into()), Some("1".into()), None).is_err());
        assert!(source_unit(None, Some("1".into()), None).is_ok());
    }

    #[test]
    fn diagnostic_explain_knows_capability_codes() {
        let (_, slug, _, _) = diagnostic_explanation("RN-E0402");
        assert_eq!(slug, "cap.mint-outside-boundary");
    }

    #[test]
    fn stdlib_listing_includes_both_platform_boundaries() {
        let modules = stdlib_entries("windows", locus::stdlib_modules());
        assert!(modules.iter().any(|m| m.name == "winapi" && m.boundary));
        let linux = stdlib_entries("linux", locus::linux_stdlib_modules());
        assert!(linux.iter().any(|m| m.name == "libc" && m.boundary));
    }

    #[test]
    fn server_handler_exposes_the_tool_router() {
        let server = LocusMcpServer::new();
        assert_eq!(server.get_info().server_info.name, "locus-llvm");
        assert!(server.get_tool("check").is_some());
        assert!(server.get_tool("help_overview").is_some());
        assert!(server.get_tool("help_search").is_some());
        assert!(server.get_tool("help_service").is_some());
        assert!(server.get_tool("run_agent_text").is_some());
        assert!(server.get_tool("agent_session_start").is_some());
        assert!(server.get_tool("agent_session_reply").is_some());
        assert!(server.get_tool("agent_session_status").is_some());
        assert!(server.get_tool("agent_session_close").is_some());
        assert!(server.get_tool("list_stdlib_services").is_some());
    }

    #[test]
    fn help_search_finds_agent_channel() {
        let server = LocusMcpServer::new();
        let result = server.help_search(Parameters(HelpSearchRequest {
            query: "ask agent text".into(),
            limit: Some(5),
        }));
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.expect("structured help result");
        let results = value["results"].as_array().expect("results array");
        assert!(
            results
                .iter()
                .any(|r| r["card"]["id"] == json!("service.Agent.agent_ask_text")),
            "expected agent_ask_text in help results: {value}"
        );
    }

    #[test]
    fn help_service_returns_exact_card() {
        let server = LocusMcpServer::new();
        let result = server.help_service(Parameters(HelpServiceRequest {
            name: "Agent".into(),
        }));
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.expect("structured help result");
        assert_eq!(value["card"]["id"], json!("service.Agent"));
    }

    #[test]
    fn help_overview_reports_worker_and_supervisor_metadata() {
        let server = LocusMcpServer::new();
        let result = server.help_overview();
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.expect("structured help overview");
        assert_eq!(value["worker"]["protocol"], json!("locus-worker/1"));
        assert_eq!(
            value["supervisor"]["protocol"],
            json!("locus-mcp-supervisor/1")
        );
        assert!(value["worker"]["stdlib_hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("0x")));
        assert_eq!(value["worker"]["abi_version"], json!(locus::ABI_VERSION));
    }

    #[test]
    fn worker_tool_check_sees_worker_stdlib_services() {
        let server = LocusMcpServer::new();
        let result = server.check(Parameters(SourceRequest {
            file: None,
            source: Some("random_next_seed 12345".into()),
            boundary_modules: None,
        }));
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.expect("structured check result");
        assert_eq!(value["type"], json!("Int"));
        assert_eq!(value["pure"], json!(true));
    }

    #[test]
    fn run_agent_text_executes_and_returns_transcript() {
        let server = LocusMcpServer::new();
        let result = server.run_agent_text(Parameters(AgentRunRequest {
            file: None,
            source: Some(
                r#"let answer = agent_ask_text "move?" in
                   let _ = agent_tell_text answer in
                   if string_equals answer "d3" then 7 else 0"#
                    .into(),
            ),
            boundary_modules: None,
            responses: Some(vec!["d3".into()]),
            default_response: None,
        }));
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.expect("structured tool result");
        assert_eq!(value["result_i64"], json!(7));
        assert_eq!(value["agent_io_model"], json!("queued_replay"));
        assert_eq!(value["agent_transcript"]["mode"], json!("queued_replay"));
        assert_eq!(value["agent_transcript"]["events"][0]["kind"], json!("ask"));
        assert_eq!(
            value["agent_transcript"]["events"][0]["prompt"],
            json!("move?")
        );
        assert_eq!(
            value["agent_transcript"]["events"][0]["used_default"],
            json!(false)
        );
        assert_eq!(
            value["agent_transcript"]["asks"][0]["response"],
            json!("d3")
        );
        assert_eq!(
            value["agent_transcript"]["events"][1]["kind"],
            json!("tell")
        );
    }

    #[test]
    fn live_agent_session_round_trips_replies() {
        let server = LocusMcpServer::new();
        let source = r#"let a = agent_ask_text "first?" in
                       let _ = agent_tell_text a in
                       let b = agent_ask_text "second?" in
                       if string_equals (string_concat a b) "ab" then 42 else 0"#;

        let start = server.agent_session_start(Parameters(AgentSessionStartRequest {
            file: None,
            source: Some(source.into()),
            boundary_modules: None,
            wait_ms: Some(10_000),
        }));
        assert_eq!(start.is_error, Some(false));
        let start_value = start.structured_content.expect("start result");
        assert_eq!(start_value["status"], json!("waiting"));
        assert_eq!(start_value["agent_io_model"], json!("live_session"));
        assert_eq!(start_value["current_ask"]["prompt"], json!("first?"));
        let session_id = start_value["session_id"]
            .as_str()
            .expect("session id")
            .to_string();

        let first = server.agent_session_reply(Parameters(AgentSessionReplyRequest {
            session_id: session_id.clone(),
            response: "a".into(),
            since_event_index: Some(1),
            wait_ms: Some(10_000),
        }));
        assert_eq!(first.is_error, Some(false));
        let first_value = first.structured_content.expect("first reply result");
        assert_eq!(first_value["status"], json!("waiting"));
        assert_eq!(first_value["current_ask"]["prompt"], json!("second?"));
        assert_eq!(first_value["event_window_start"], json!(1));
        assert_eq!(first_value["total_event_count"], json!(3));
        assert_eq!(first_value["event_count"], json!(2));
        assert_eq!(first_value["latest_score"], serde_json::Value::Null);
        assert_eq!(first_value["asks"][0]["prompt"], json!("second?"));
        assert_eq!(first_value["tells"][0]["text"], json!("a"));

        let second = server.agent_session_reply(Parameters(AgentSessionReplyRequest {
            session_id: session_id.clone(),
            response: "b".into(),
            since_event_index: None,
            wait_ms: Some(10_000),
        }));
        assert_eq!(second.is_error, Some(false));
        let second_value = second.structured_content.expect("second reply result");
        assert_eq!(second_value["status"], json!("completed"));
        assert_eq!(second_value["result"]["result_i64"], json!(42));
        assert_eq!(second_value["asks"][1]["response"], json!("b"));

        let close = server.agent_session_close(Parameters(AgentSessionCloseRequest { session_id }));
        assert_eq!(close.is_error, Some(false));
    }
}
