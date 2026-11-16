//! Session-owned admission. A dropped or disconnected takeover never releases its fence.

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::Notify;

use crate::function_tool::FunctionCallError;
use crate::tools::context::{
    FunctionToolOutput, ToolCallSource, ToolInvocation, ToolPayload, boxed_tool_output,
};
use crate::tools::registry::{CoreToolRuntime, ToolExecutor};
use codex_tools::{JsonSchema, ResponsesApiTool, ToolName, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) struct NativeTakeoverHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnterArguments {
    conversation_id: String,
    task: String,
}

fn denied(call_id: &str, reason: &str) -> Box<dyn crate::tools::context::ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(
        json!({
            "protocol_version":1,"phase":"return","request_id":call_id,
            "target_closed":true,"outcome":"denied_before_dispatch","reason":reason,
        })
        .to_string(),
        Some(false),
    ))
}

impl ToolExecutor<ToolInvocation> for NativeTakeoverHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("enter_matrix")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "enter_matrix".to_string(),
            description: "Temporarily use an authorized XVision context and tools in this same origin session/model. Enter returns after activation; call exit_matrix to reconcile and restore the origin scope. Direct calls only.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(std::collections::BTreeMap::from([
                ("conversation_id".to_string(), JsonSchema::string(Some("Exact XVision debug communication selected by its owner.".to_string()))),
                ("task".to_string(), JsonSchema::string(Some("Debug task, without credentials or a supplied solution.".to_string()))),
            ]), Some(vec!["conversation_id".to_string(), "task".to_string()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            if invocation.source != ToolCallSource::Direct
                || invocation.turn.session_source.is_non_root_agent()
                || invocation.turn.realtime_active
            {
                return Ok(denied(
                    &invocation.call_id,
                    "requires a direct root developer call outside Realtime",
                ));
            }
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Ok(denied(&invocation.call_id, "expected structured arguments"));
            };
            let args: EnterArguments = match super::parse_arguments(arguments) {
                Ok(args) => args,
                Err(_) => return Ok(denied(&invocation.call_id, "invalid enter arguments")),
            };
            if args.conversation_id.is_empty()
                || args.conversation_id.len() > 128
                || args.task.trim().is_empty()
                || args.task.len() > 8000
            {
                return Ok(denied(&invocation.call_id, "invalid target or task"));
            }
            let fence = invocation
                .session
                .services
                .session_extension_data
                .get::<NativeTakeoverFence>();
            let Some(fence) = fence else {
                return Ok(denied(
                    &invocation.call_id,
                    "developer takeover is disabled",
                ));
            };
            let generation = match fence.enter() {
                Ok(generation) => generation,
                Err(_) => {
                    return Ok(denied(
                        &invocation.call_id,
                        "origin has admitted work or another takeover",
                    ));
                }
            };
            // Detached from a cancelled tool waiter, but still owned by this exact fence.
            // Losing the waiter must not release the origin while target work can remain live.
            let worker = tokio::spawn(async move {
                if let Err(error) = fence.wait_for_boundary(generation).await {
                    fence.deny_before_dispatch(generation)?;
                    return Ok(denied(&invocation.call_id, &error.to_string()));
                }
                if invocation.cancellation_token.is_cancelled() {
                    fence.deny_before_dispatch(generation)?;
                    return Ok(denied(
                        &invocation.call_id,
                        "cancelled before target dispatch",
                    ));
                }
                let session = &invocation.session;
                let agents = session
                    .services
                    .agent_control
                    .list(
                        session.thread_id,
                        None,
                        &invocation.turn.session_source,
                        None,
                    )
                    .await;
                let agents = match agents {
                    Ok(agents) => agents,
                    Err(_) => {
                        fence.deny_before_dispatch(generation)?;
                        return Ok(denied(
                            &invocation.call_id,
                            "owned agent inventory is unavailable",
                        ));
                    }
                };
                if session.services.code_mode_service.has_active_cells()
                    || !session
                        .services
                        .unified_exec_manager
                        .list_processes()
                        .await
                        .is_empty()
                    || session.hooks().has_running_tasks()
                    || session.has_background_compaction().await
                    || agents.iter().any(|agent| {
                        agent.thread_id != session.thread_id
                            && matches!(
                                agent.status,
                                codex_protocol::protocol::AgentStatus::Running
                                    | codex_protocol::protocol::AgentStatus::PendingInit
                            )
                    })
                {
                    fence.deny_before_dispatch(generation)?;
                    return Ok(denied(
                        &invocation.call_id,
                        "origin still has owned background work; finish it before entering",
                    ));
                }
                let socket = invocation.turn.config.dev_native_takeover_socket.as_ref();
                let Some(socket) = socket else {
                    fence.deny_before_dispatch(generation)?;
                    return Ok(denied(
                        &invocation.call_id,
                        "developer control socket is not configured",
                    ));
                };
                let result = exchange(
                    &invocation,
                    &args,
                    socket.as_path(),
                    &fence,
                    generation,
                    false,
                )
                .await;
                if result.is_err() && fence.is_fenced() {
                    fence.recover(generation);
                }
                result
            });
            worker.await.map_err(|_| {
                unavailable("control task failed; origin remains fenced for reconciliation")
            })?
        })
    }
}

impl CoreToolRuntime for NativeTakeoverHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
    fn is_native_takeover_control(&self) -> bool {
        true
    }
    fn finishes_on_cancellation(&self) -> bool {
        true
    }
}

#[cfg(unix)]
async fn connect_private(path: &std::path::Path) -> std::io::Result<tokio::net::UnixStream> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use tokio::net::UnixStream;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing socket parent"))?;
    let directory = tokio::fs::symlink_metadata(parent).await?;
    let socket = tokio::fs::symlink_metadata(path).await?;
    // SAFETY: geteuid has no arguments or memory preconditions.
    let uid = unsafe { libc::geteuid() };
    if !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o077 != 0
        || !socket.file_type().is_socket()
        || socket.uid() != uid
        || socket.mode() & 0o777 != 0o600
    {
        return Err(std::io::Error::other("control socket is not private"));
    }
    let stream = UnixStream::connect(path).await?;
    if stream.peer_cred()?.uid() != uid {
        return Err(std::io::Error::other("control peer owner mismatch"));
    }
    Ok::<_, std::io::Error>(stream)
}

#[cfg(unix)]
async fn exchange(
    invocation: &ToolInvocation,
    args: &EnterArguments,
    path: &std::path::Path,
    fence: &NativeTakeoverFence,
    generation: u64,
    recovery: bool,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    use tokio::io::{AsyncWriteExt, BufReader};
    let connected = connect_private(path).await;
    let stream = match connected {
        Ok(stream) => stream,
        Err(_) => {
            if !recovery {
                fence.deny_before_dispatch(generation)?;
            }
            return Err(unavailable("private control is unavailable"));
        }
    };
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let request_id = invocation.call_id.clone();
    let request = json!({"protocol_version":2,"phase":if recovery { "recover" } else { "enter" },"request_id":request_id,
        "origin_generation":generation,
        "origin_ref":{"thread_id":invocation.session.thread_id.to_string(),"turn_id":invocation.turn.sub_id,"call_id":invocation.call_id},
        "target":{"conversation_id":args.conversation_id},"task":args.task,
        "origin_inference_identity":{"model":invocation.step_context.settings.model_info.slug,
            "provider":invocation.turn.config.model_provider_id}});
    let request = if recovery {
        let mut saved = fence
            .lock()
            .recovery_request
            .clone()
            .ok_or_else(|| unavailable("origin recovery checkpoint unavailable"))?;
        saved["phase"] = json!("recover");
        saved["origin_ref"]["thread_id"] = json!(invocation.session.thread_id.to_string());
        saved
    } else {
        request
    };
    fence.lock().recovery_request = Some(request.clone());
    let mut encoded =
        serde_json::to_vec(&request).map_err(|_| unavailable("cannot encode control request"))?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .map_err(|_| unavailable("enter delivery uncertain; origin remains fenced"))?;
    let mut cancelled = recovery;
    loop {
        let frame = {
            let reading = read_control_frame(&mut reader);
            tokio::pin!(reading);
            tokio::select! {
                frame=&mut reading=>frame?,
                _=invocation.cancellation_token.cancelled(),if !cancelled=>{
                    cancelled=true;
                    write_control_frame(&mut writer,&json!({"phase":"cancel","request_id":request["request_id"],
                        "origin_generation":request["origin_generation"]})).await?;
                    reading.await?
                }
            }
        };
        validate_frame(&frame, &request)?;
        match frame["phase"].as_str() {
            Some("authorization_required") => {}
            Some("prepared") if !recovery && !cancelled => {
                let id = frame["binding_id"]
                    .as_str()
                    .ok_or_else(|| unavailable("missing binding"))?;
                fence.lock().binding_id = Some(id.to_string());
                write_control_frame(&mut writer, &json!({
                    "phase":"activate","request_id":request_id,"origin_generation":generation,"binding_id":id
                })).await?;
            }
            Some("prepared") if cancelled => {}
            Some("active") if !recovery => {
                let expected = fence.lock().binding_id.clone();
                if expected.as_deref() != frame["binding_id"].as_str()
                    || frame["scope"]["binding_id"] != frame["binding_id"]
                {
                    return Err(unavailable("activation binding mismatch"));
                }
                let scope =
                    MatrixScope::prepare(invocation, frame["scope"].clone(), request.clone())
                        .await?;
                if invocation.cancellation_token.is_cancelled() {
                    if !cancelled {
                        cancelled = true;
                        write_control_frame(&mut writer,&json!({"phase":"cancel","request_id":request["request_id"],
                        "origin_generation":request["origin_generation"],"binding_id":scope.binding_id})).await?;
                    }
                    continue;
                }
                let (commands, receiver) = tokio::sync::mpsc::channel(1);
                scope
                    .commands
                    .set(commands)
                    .map_err(|_| unavailable("duplicate scope activation"))?;
                fence.install(generation, Arc::clone(&scope))?;
                let observed = Arc::downgrade(
                    &invocation
                        .session
                        .services
                        .session_extension_data
                        .get::<NativeTakeoverFence>()
                        .ok_or_else(|| unavailable("missing fence"))?,
                );
                tokio::spawn(control_actor(
                    reader, writer, receiver, request, observed, generation,
                ));
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    json!({"protocol_version":2,"phase":"active","binding_id":scope.binding_id,
                         "origin_ref":scope.request["origin_ref"],"origin_inference_identity":scope.request["origin_inference_identity"],
                        "recovery_request":scope.request,
                        "instruction":"Continue this same session using the supplied XVision tools; call exit_matrix when finished."}).to_string(),
                    Some(true))));
            }
            Some("return")
                if frame["target_closed"] == true
                    && fence
                        .lock()
                        .binding_id
                        .as_ref()
                        .is_none_or(|id| frame["binding_id"] == *id) =>
            {
                fence.returning(generation)?;
                write_control_frame(
                    &mut writer,
                    &json!({ "phase":"returned","request_id":request["request_id"],
                    "origin_generation":request["origin_generation"],"binding_id":frame["binding_id"]}),
                )
                .await?;
                let closed = read_control_frame(&mut reader).await?;
                validate_frame(&closed, &request)?;
                if closed["phase"] != "returned" || closed["binding_id"] != frame["binding_id"] {
                    return Err(unavailable("return acknowledgement unconfirmed"));
                }
                fence.finish_return(generation)?;
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    frame.to_string(),
                    Some(matches!(
                        frame["outcome"].as_str(),
                        Some("exited" | "reconciled")
                    )),
                )));
            }
            _ => return Err(unavailable("unexpected scope activation phase")),
        }
    }
}

#[cfg(not(unix))]
async fn exchange(
    _invocation: &ToolInvocation,
    _args: &EnterArguments,
    _path: &std::path::Path,
    fence: &NativeTakeoverFence,
    generation: u64,
    _recovery: bool,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    fence.deny_before_dispatch(generation)?;
    Err(unavailable(
        "private Unix control is unsupported on this platform",
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Origin,
    Entering,
    Active,
    Returning,
    Recovering,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    generation: u64,
    admitted: usize,
    response: Option<bool>,
    scope: Option<Arc<MatrixScope>>,
    recovery_request: Option<Value>,
    binding_id: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            phase: Phase::Origin,
            generation: 0,
            admitted: 0,
            response: None,
            scope: None,
            recovery_request: None,
            binding_id: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct NativeTakeoverFence {
    state: Mutex<State>,
    boundary: Notify,
}

fn unavailable(reason: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("Native takeover unavailable: {reason}"))
}

impl NativeTakeoverFence {
    pub(crate) fn from_history(items: &[codex_history::RolloutItem]) -> Self {
        use codex_protocol::models::ResponseItem;
        let mut pending = std::collections::HashSet::new();
        let mut exit_calls = std::collections::HashSet::new();
        let mut checkpoint = None;
        let mut checkpoint_binding = None;
        let mut original = None;
        for item in items {
            let codex_history::RolloutItem::ResponseItem(envelope) = item else {
                continue;
            };
            match &envelope.item {
                ResponseItem::FunctionCall {
                    name,
                    call_id,
                    namespace,
                    ..
                } if name == "enter_matrix"
                    && namespace.as_deref().is_none_or(|ns| {
                        ns.is_empty() || ns == codex_protocol::DEFAULT_FUNCTION_NAMESPACE
                    }) =>
                {
                    pending.insert(call_id.clone());
                    if let ResponseItem::FunctionCall { arguments, .. } = &envelope.item
                        && let Ok(args) = serde_json::from_str::<Value>(arguments)
                    {
                        original = Some(
                            json!({"protocol_version":2,"phase":"recover","request_id":call_id,
                            "origin_generation":1,"origin_ref":{"thread_id":null,"turn_id":null,"call_id":call_id},
                            "target":{"conversation_id":args["conversation_id"]},"task":args["task"],
                            "origin_inference_identity":{"model":"unknown","provider":"unknown"}}),
                        );
                    }
                }
                ResponseItem::FunctionCall {
                    name,
                    call_id,
                    namespace,
                    ..
                } if name == "exit_matrix"
                    && namespace.as_deref().is_none_or(|ns| {
                        ns.is_empty() || ns == codex_protocol::DEFAULT_FUNCTION_NAMESPACE
                    }) =>
                {
                    exit_calls.insert(call_id.clone());
                }
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    output,
                    ..
                } => {
                    if let Some(text) = output.body.to_text()
                        && let Ok(receipt) = serde_json::from_str::<Value>(&text)
                    {
                        if receipt["protocol_version"] == 2
                            && receipt["phase"] == "active"
                            && let Some(saved) = receipt.get("recovery_request")
                        {
                            checkpoint = Some(saved.clone());
                            checkpoint_binding = receipt["binding_id"].as_str().map(str::to_string);
                        }
                        if receipt["target_closed"] == true
                            && receipt["phase"] == "return"
                            && (pending.contains(call_id) || exit_calls.contains(call_id))
                        {
                            if receipt["protocol_version"] == 1 && receipt["request_id"] == *call_id
                            {
                                pending.remove(call_id);
                            } else if receipt["protocol_version"] == 2
                                && checkpoint.as_ref().or(original.as_ref()).is_some_and(
                                    |saved: &Value| saved["request_id"] == receipt["request_id"],
                                )
                            {
                                pending.clear();
                                checkpoint = None;
                                original = None;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let fence = Self::default();
        if !pending.is_empty() {
            let mut state = fence.lock();
            state.phase = Phase::Recovering;
            state.recovery_request = checkpoint.or(original);
            state.binding_id = checkpoint_binding;
            state.generation = state
                .recovery_request
                .as_ref()
                .and_then(|request| request["origin_generation"].as_u64())
                .unwrap_or_default();
        }
        fence
    }

    pub(crate) fn is_fenced(&self) -> bool {
        self.lock().phase != Phase::Origin
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Poison cannot authorize dispatch after an unconfirmed takeover.
        self.state.lock().expect("native takeover state poisoned")
    }

    #[cfg(test)]
    pub(crate) fn admit(self: &Arc<Self>) -> Result<DispatchPermit, FunctionCallError> {
        let mut state = self.lock();
        if state.phase != Phase::Origin {
            return Err(unavailable("origin session is fenced"));
        }
        state.admitted += 1;
        Ok(DispatchPermit(Arc::clone(self)))
    }

    pub(crate) fn begin_sampling(self: &Arc<Self>) -> Result<ResponseBoundary, FunctionCallError> {
        let mut state = self.lock();
        if !matches!(state.phase, Phase::Origin | Phase::Active)
            && !(state.phase == Phase::Recovering && state.recovery_request.is_some())
        {
            return Err(unavailable("origin inference is fenced"));
        }
        state.response = None;
        Ok(ResponseBoundary(Arc::clone(self)))
    }

    /// Reserve atomically against all common-registry dispatch, including Code Mode callbacks.
    pub(crate) fn enter(&self) -> Result<u64, FunctionCallError> {
        let mut state = self.lock();
        if state.phase != Phase::Origin || state.admitted != 0 {
            return Err(unavailable("origin has admitted work or another takeover"));
        }
        state.generation += 1;
        state.phase = Phase::Entering;
        Ok(state.generation)
    }

    pub(crate) async fn wait_for_boundary(&self, generation: u64) -> Result<(), FunctionCallError> {
        loop {
            let notified = self.boundary.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.lock();
                if state.generation != generation || state.phase != Phase::Entering {
                    return Err(unavailable("stale takeover generation"));
                }
                match state.response {
                    Some(true) => return Ok(()),
                    Some(false) => return Err(unavailable("origin response did not complete")),
                    None => {}
                }
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn activate(&self, generation: u64) -> Result<(), FunctionCallError> {
        let mut state = self.lock();
        if state.generation != generation
            || state.phase != Phase::Entering
            || state.response != Some(true)
        {
            return Err(unavailable("origin boundary is not confirmed"));
        }
        state.phase = Phase::Active;
        Ok(())
    }

    pub(crate) fn returning(&self, generation: u64) -> Result<(), FunctionCallError> {
        let mut state = self.lock();
        if state.generation != generation || state.phase == Phase::Origin || state.admitted != 0 {
            return Err(unavailable(
                "stale return generation or outstanding scoped calls",
            ));
        }
        state.phase = Phase::Returning;
        Ok(())
    }

    /// Called only on an authenticated receipt proving the target binding and work are closed.
    pub(crate) fn finish_return(&self, generation: u64) -> Result<(), FunctionCallError> {
        let mut state = self.lock();
        if state.generation != generation || state.phase != Phase::Returning {
            return Err(unavailable("return is not reconciled"));
        }
        state.phase = Phase::Origin;
        state.scope = None;
        state.recovery_request = None;
        state.binding_id = None;
        Ok(())
    }

    /// Before any target request has been sent, a denied enter can safely release its reservation.
    pub(crate) fn deny_before_dispatch(&self, generation: u64) -> Result<(), FunctionCallError> {
        let mut state = self.lock();
        if state.generation != generation || state.phase != Phase::Entering {
            return Err(unavailable("enter is no longer locally reversible"));
        }
        state.phase = Phase::Origin;
        Ok(())
    }

    pub(crate) fn recover(&self, generation: u64) {
        let mut state = self.lock();
        if state.generation == generation && state.phase != Phase::Origin {
            state.phase = Phase::Recovering;
        }
    }
}

pub(crate) struct DispatchPermit(Arc<NativeTakeoverFence>);

impl Drop for DispatchPermit {
    fn drop(&mut self) {
        self.0.lock().admitted -= 1;
    }
}

pub(crate) struct ResponseBoundary(Arc<NativeTakeoverFence>);

impl ResponseBoundary {
    pub(crate) fn complete(&self) {
        self.0.lock().response = Some(true);
        self.0.boundary.notify_waiters();
    }
}

impl Drop for ResponseBoundary {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        if state.response.is_none() {
            state.response = Some(false);
        }
        drop(state);
        self.0.boundary.notify_waiters();
    }
}

type ControlReply = tokio::sync::oneshot::Sender<Result<Value, FunctionCallError>>;
struct ScopeCommand {
    frame: Value,
    reply: ControlReply,
}

/// Only owner-issued non-secret context is copied into model prompts.
pub(crate) struct MatrixScope {
    pub(crate) binding_id: String,
    pub(crate) generation: u64,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) context: Value,
    pub(crate) instructions: String,
    pub(crate) tools: Vec<ResponsesApiTool>,
    mcp_tools: std::collections::HashMap<String, (String, String)>,
    pub(crate) environments: crate::environment_selection::TurnEnvironmentSnapshot,
    request: Value,
    socket: std::path::PathBuf,
    fence: std::sync::Weak<NativeTakeoverFence>,
    commands: std::sync::OnceLock<tokio::sync::mpsc::Sender<ScopeCommand>>,
}

impl std::fmt::Debug for MatrixScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MatrixScope")
            .field("binding_id", &self.binding_id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl MatrixScope {
    async fn prepare(
        invocation: &ToolInvocation,
        value: Value,
        request: Value,
    ) -> Result<Arc<Self>, FunctionCallError> {
        use crate::environment_selection::{
            EnvironmentConfigOrigin, TurnEnvironmentSnapshot, TurnEnvironmentState,
        };
        use codex_exec_server::RemoteEnvironmentOptions;
        use codex_protocol::protocol::{EnvironmentConfigState, TurnEnvironmentSelection};
        use codex_utils_path_uri::PathUri;
        let text = |key: &str| {
            value[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| unavailable("invalid scope descriptor"))
        };
        if value["target_ref"] != request["target"] {
            return Err(unavailable("scope target mismatch"));
        }
        let binding_id = text("binding_id")?;
        let generation = value["generation"]
            .as_u64()
            .ok_or_else(|| unavailable("missing binding generation"))?;
        if value["origin_inference_identity"] != request["origin_inference_identity"] {
            return Err(unavailable("origin inference identity mismatch"));
        }
        let guest = &value["guest"];
        let environment_id = guest["environment_id"]
            .as_str()
            .filter(|id| id.starts_with("xvision-"))
            .ok_or_else(|| unavailable("missing scoped guest environment"))?
            .to_string();
        let url = guest["url"]
            .as_str()
            .ok_or_else(|| unavailable("missing guest executor"))?
            .to_string();
        let token = guest["token"]
            .as_str()
            .ok_or_else(|| unavailable("missing guest authority"))?;
        let manager = invocation
            .session
            .services
            .turn_environments
            .environment_manager();
        manager
            .upsert_environment_with_options(
                environment_id.clone(),
                RemoteEnvironmentOptions {
                    exec_server_url: url,
                    connect_timeout: Some(std::time::Duration::from_secs(10)),
                    http_headers: std::collections::HashMap::from([(
                        "Authorization".to_string(),
                        format!("Bearer {token}"),
                    )]),
                },
                Default::default(),
            )
            .map_err(|_| unavailable("guest registration failed"))?;
        let environment = manager
            .get_environment(&environment_id)
            .ok_or_else(|| unavailable("guest unavailable"))?;
        environment
            .wait_until_ready()
            .await
            .map_err(|_| unavailable("guest not ready"))?;
        let info = environment
            .info()
            .await
            .map_err(|_| unavailable("guest identity unavailable"))?;
        let mut config = invocation
            .step_context
            .environments
            .primary()
            .ok_or_else(|| unavailable("origin environment unavailable"))?
            .config()
            .clone();
        let cwd: PathUri = serde_json::from_value(json!("/workspace"))
            .map_err(|_| unavailable("invalid guest cwd"))?;
        config.workspace_roots = vec![cwd.clone()];
        // Managed hooks belong to their environment owner, not the borrowed host attachment.
        // Session-wide trusted origin hooks remain in the existing Hooks owner.
        config.environment_hooks = serde_json::from_value(guest["environment_hooks"].clone())
            .map_err(|_| unavailable("missing guest-owned managed hook configuration"))?;
        config.allow_login_shell = false;
        config.shell_environment_policy.inherit =
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::None;
        config.shell_environment_policy.use_profile = false;
        let expected = [
            "HOME",
            "PATH",
            "TMPDIR",
            "UV_PYTHON_INSTALL_DIR",
            "UV_CACHE_DIR",
        ];
        if guest["shell_environment"].as_object().is_none_or(|values| {
            values.len() != expected.len()
                || values.iter().any(|(name, value)| {
                    !expected.contains(&name.as_str()) || value.as_str().is_none_or(str::is_empty)
                })
        }) {
            return Err(unavailable(
                "guest shell environment must be session-scoped",
            ));
        }
        config.shell_environment_policy.set =
            serde_json::from_value(guest["shell_environment"].clone())
                .map_err(|_| unavailable("invalid guest environment"))?;
        config.selected_capability_roots.clear();
        let selection = TurnEnvironmentSelection {
            environment_id,
            cwd: cwd.clone(),
            workspace_roots: vec![cwd],
            config: EnvironmentConfigState::Ready(config),
            selected_capability_roots: Default::default(),
        };
        let shell = Some(
            crate::shell::Shell::from_environment_shell_info(info.shell)
                .map_err(|_| unavailable("guest shell unavailable"))?,
        );
        let resolved = crate::session::turn_context::TurnEnvironment::new(
            selection,
            EnvironmentConfigOrigin::Owner,
            environment,
            shell,
        );
        let mut tools = Vec::new();
        for item in value["tools"]
            .as_array()
            .ok_or_else(|| unavailable("missing tool definitions"))?
        {
            let name = item["name"]
                .as_str()
                .ok_or_else(|| unavailable("invalid tool name"))?;
            if [
                "enter_matrix",
                "exit_matrix",
                "exec_command",
                "write_stdin",
                "apply_patch",
            ]
            .contains(&name)
            {
                return Err(unavailable("scope tool collision"));
            }
            let parameters =
                codex_tools::parse_tool_input_schema_without_compaction(&item["inputSchema"])
                    .map_err(|_| unavailable("invalid tool schema"))?;
            let mut description = item["description"].as_str().unwrap_or_default().to_string();
            if serde_json::to_value(&parameters)
                .map_err(|_| unavailable("cannot inspect tool schema"))?
                != item["inputSchema"]
            {
                description
                    .push_str("\nAuthoritative input schema (validated by the tool owner): ");
                description.push_str(&item["inputSchema"].to_string());
            }
            tools.push(ResponsesApiTool {
                name: name.to_string(),
                description,
                strict: false,
                defer_loading: None,
                parameters,
                output_schema: None,
            });
        }
        let mut mcp_tools = std::collections::HashMap::new();
        if let Some(catalog) = value["mcp_tools"].as_object() {
            for (alias, identity) in catalog {
                let server = identity["server"]
                    .as_str()
                    .ok_or_else(|| unavailable("invalid MCP server identity"))?;
                let tool = identity["tool"]
                    .as_str()
                    .ok_or_else(|| unavailable("invalid MCP tool identity"))?;
                if !tools.iter().any(|spec| spec.name == *alias) {
                    return Err(unavailable("MCP source has no callable declaration"));
                }
                mcp_tools.insert(alias.clone(), (server.to_string(), tool.to_string()));
            }
        }
        Ok(Arc::new(Self {
            binding_id,
            generation,
            model: request["origin_inference_identity"]["model"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            provider: request["origin_inference_identity"]["provider"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            context: value["context"].clone(),
            instructions: text("instructions")?,
            tools,
            mcp_tools,
            environments: TurnEnvironmentSnapshot {
                environments: vec![TurnEnvironmentState::Ready(resolved)],
            },
            request,
            socket: invocation
                .turn
                .config
                .dev_native_takeover_socket
                .as_ref()
                .ok_or_else(|| unavailable("scope socket missing"))?
                .as_path()
                .to_path_buf(),
            fence: Arc::downgrade(
                &invocation
                    .session
                    .services
                    .session_extension_data
                    .get::<NativeTakeoverFence>()
                    .ok_or_else(|| unavailable("missing scope fence"))?,
            ),
            commands: Default::default(),
        }))
    }

    pub(crate) async fn authorize_native(
        self: &Arc<Self>,
        invocation: &ToolInvocation,
    ) -> Result<(), FunctionCallError> {
        if !["exec_command", "write_stdin", "apply_patch"]
            .contains(&invocation.tool_name.name.as_str())
        {
            return Ok(());
        }
        let arguments = match &invocation.payload {
            ToolPayload::Function { arguments } => serde_json::from_str::<Value>(arguments)
                .map_err(|_| unavailable("invalid native arguments"))?,
            ToolPayload::Custom { input } => json!({"input":input}),
            _ => return Err(unavailable("unsupported native payload")),
        };
        let response = self
            .command(json!({"phase":"native_admit","call_id":invocation.call_id,
            "tool":invocation.tool_name.name,"arguments":arguments,
            "environment_id":self.environments.primary().unwrap().selection.environment_id}))
            .await?;
        if response["success"] != true || response["output"]["authorized"] != true {
            return Err(FunctionCallError::RespondToModel(
                response["output"].to_string(),
            ));
        }
        Ok(())
    }

    fn cancel_owned(self: &Arc<Self>) {
        let generation = self.request["origin_generation"]
            .as_u64()
            .unwrap_or_default();
        if let Some(fence) = self.fence.upgrade() {
            fence.recover(generation);
        }
        #[cfg(unix)]
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let scope = Arc::clone(self);
            runtime.spawn(async move {
                let cancel = async {
                    let stream = connect_private(&scope.socket)
                        .await
                        .map_err(|_| unavailable("cancel control unavailable"))?;
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = tokio::io::BufReader::new(reader);
                    let mut frame = scope.request.clone();
                    frame["phase"] = json!("cancel_bound");
                    frame["binding_id"] = json!(scope.binding_id);
                    write_control_frame(&mut writer, &frame).await?;
                    let reply = read_control_frame(&mut reader).await?;
                    validate_frame(&reply, &scope.request)
                };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(10), cancel).await;
            });
        }
    }

    async fn command(self: &Arc<Self>, mut frame: Value) -> Result<Value, FunctionCallError> {
        let mut completion = ScopedCallGuard {
            scope: Arc::clone(self),
            finished: false,
        };
        frame["request_id"] = self.request["request_id"].clone();
        frame["origin_generation"] = self.request["origin_generation"].clone();
        frame["binding_id"] = json!(self.binding_id);
        let (send, receive) = tokio::sync::oneshot::channel();
        self.commands
            .get()
            .ok_or_else(|| unavailable("scope transport not installed"))?
            .send(ScopeCommand { frame, reply: send })
            .await
            .map_err(|_| unavailable("scope disconnected; reconcile before return"))?;
        let result = receive
            .await
            .map_err(|_| unavailable("scope result uncertain"))??;
        completion.finished = true;
        Ok(result)
    }
}

impl NativeTakeoverFence {
    pub(crate) fn origin_turn_ended(&self, turn_id: &str) {
        let scope = self
            .overlay()
            .filter(|scope| scope.request["origin_ref"]["turn_id"] == turn_id);
        if let Some(scope) = scope {
            scope.cancel_owned();
        }
    }

    pub(crate) fn needs_recovery(&self) -> bool {
        let state = self.lock();
        state.phase == Phase::Recovering && state.recovery_request.is_some()
    }

    pub(crate) fn admit_task(self: &Arc<Self>) -> Result<DispatchPermit, FunctionCallError> {
        let mut state = self.lock();
        if state.phase != Phase::Origin
            && !(state.phase == Phase::Recovering && state.recovery_request.is_some())
        {
            return Err(unavailable(
                "finish or reconcile this origin turn before starting another",
            ));
        }
        state.admitted += 1;
        Ok(DispatchPermit(Arc::clone(self)))
    }

    pub(crate) fn overlay(&self) -> Option<Arc<MatrixScope>> {
        let state = self.lock();
        (state.phase == Phase::Active)
            .then(|| state.scope.clone())
            .flatten()
    }

    fn install(&self, generation: u64, scope: Arc<MatrixScope>) -> Result<(), FunctionCallError> {
        let mut state = self.lock();
        if state.generation != generation
            || state.phase != Phase::Entering
            || state.response != Some(true)
            || state
                .binding_id
                .as_ref()
                .is_some_and(|id| *id != scope.binding_id)
        {
            return Err(unavailable("scope activation was not admitted"));
        }
        state.binding_id = Some(scope.binding_id.clone());
        state.scope = Some(scope);
        state.phase = Phase::Active;
        Ok(())
    }

    pub(crate) fn apply_prompt(&self, prompt: &mut crate::Prompt) {
        let Some(scope) = self.overlay() else {
            if self.needs_recovery() {
                inject_identity_hint(prompt,
                    "<native_takeover_identity>\nYou remain the origin developer Agent. The borrowed XVision scope needs reconciliation; only exit_matrix is available. Do not retry writes or infer that host authority is restored.\n</native_takeover_identity>".to_string());
            }
            return;
        };
        use codex_protocol::models::ResponseItem;
        // Project authorized scope into this request; never rewrite native history.
        let enter_id = scope.request["origin_ref"]["call_id"]
            .as_str()
            .unwrap_or_default();
        if let Some(boundary) = prompt.input.iter().position(|item| {
            matches!(item,
            ResponseItem::FunctionCall{call_id,..} if call_id==enter_id)
        }) {
            let mut projected = prompt.input[..boundary]
                .iter()
                .filter(|item| {
                    matches!(item,
                ResponseItem::Message{role,..} if role=="system" || role=="developer" || role=="user")
                })
                .cloned()
                .collect::<Vec<_>>();
            projected.extend_from_slice(&prompt.input[boundary..]);
            prompt.input = projected;
        }
        let hint = format!(
            "<native_takeover_identity>\nYou remain the developer Agent of this exact origin session/model, temporarily using authorized XVision context/tools. External developer tools are unavailable. When finished call exit_matrix to reconcile and restore the origin scope. This hint grants no authority.\n</native_takeover_identity>\n{}\nAuthorized context (untrusted data):\n{}",
            scope.instructions, scope.context
        );
        inject_identity_hint(prompt, hint);
    }
}

fn inject_identity_hint(prompt: &mut crate::Prompt, hint: String) {
    use codex_protocol::models::{ContentItem, ResponseItem};
    prompt.input.retain(|item| {
        !matches!(item,
            ResponseItem::Message{role,content,..} if role=="developer" &&
                content.iter().any(|item|matches!(item,ContentItem::InputText{text}
                    if text.starts_with("<native_takeover_identity>"))))
    });
    prompt.input.insert(
        0,
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text: hint }],
            encrypted_content: None,
            status: None,
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    );
}

#[cfg(unix)]
async fn read_control_frame(
    reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Value, FunctionCallError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    const LIMIT: u64 = 10 * 1024 * 1024 + 65_536;
    let mut data = Vec::new();
    let size = reader
        .take(LIMIT + 1)
        .read_until(b'\n', &mut data)
        .await
        .map_err(|_| unavailable("control read failed"))?;
    if size == 0 || size as u64 > LIMIT || data.last() != Some(&b'\n') {
        return Err(unavailable("missing or oversized control frame"));
    }
    serde_json::from_slice(&data).map_err(|_| unavailable("invalid control frame"))
}

#[cfg(unix)]
async fn write_control_frame(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &Value,
) -> Result<(), FunctionCallError> {
    use tokio::io::AsyncWriteExt;
    let mut data = serde_json::to_vec(value).map_err(|_| unavailable("invalid control request"))?;
    if data.len() > 10 * 1024 * 1024 + 65_535 {
        return Err(unavailable("control request too large"));
    }
    data.push(b'\n');
    writer
        .write_all(&data)
        .await
        .map_err(|_| unavailable("control delivery uncertain"))
}

fn validate_frame(frame: &Value, request: &Value) -> Result<(), FunctionCallError> {
    if frame["protocol_version"] != 2
        || frame["request_id"] != request["request_id"]
        || frame["origin_generation"] != request["origin_generation"]
    {
        return Err(unavailable("control identity mismatch"));
    }
    Ok(())
}

#[cfg(unix)]
async fn control_actor(
    mut reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut commands: tokio::sync::mpsc::Receiver<ScopeCommand>,
    request: Value,
    fence: std::sync::Weak<NativeTakeoverFence>,
    generation: u64,
) {
    let (frames, mut incoming) = tokio::sync::mpsc::channel(1);
    let read_task = tokio::spawn(async move {
        loop {
            let frame = read_control_frame(&mut reader).await;
            let failed = frame.is_err();
            if frames.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    let _reader_guard = tokio_util::task::AbortOnDropHandle::new(read_task);
    loop {
        tokio::select! {
            command=commands.recv()=>{
                let Some(command)=command else {break};
                let result=async {
                    write_control_frame(&mut writer,&command.frame).await?;
                    let result=incoming.recv().await.ok_or_else(||unavailable("scope disconnected"))??;
                    validate_frame(&result,&request)?;
                    if result["binding_id"]!=command.frame["binding_id"] {return Err(unavailable("stale scope result"));}
                    if command.frame["phase"]=="exit" {
                        if result["phase"]!="return" || result["target_closed"]!=true {return Err(unavailable("scope closure unconfirmed"));}
                        write_control_frame(&mut writer,&json!({"phase":"returned","request_id":request["request_id"],
                            "origin_generation":generation,"binding_id":result["binding_id"]})).await?;
                        let closed=incoming.recv().await.ok_or_else(||unavailable("return ACK lost"))??;
                        validate_frame(&closed,&request)?;
                        if closed["phase"]!="returned" || closed["binding_id"]!=result["binding_id"] {
                            return Err(unavailable("return acknowledgement unconfirmed"));
                        }
                    } else if result["phase"]!="tool_result" || result["call_id"]!=command.frame["call_id"] {
                        return Err(unavailable("tool result identity mismatch"));
                    }
                    Ok(result)
                }.await;
                let terminal=command.frame["phase"]=="exit" || result.is_err();
                if result.is_err() && let Some(fence)=fence.upgrade() {fence.recover(generation);}
                if command.reply.send(result).is_err() {
                    if let Some(fence)=fence.upgrade() {fence.recover(generation);}
                    return;
                }
                if terminal {return}
            }
            _=incoming.recv()=>break,
        }
    }
    if let Some(fence) = fence.upgrade() {
        fence.recover(generation);
    }
}

pub(crate) struct MatrixRecoveryTool;
impl ToolExecutor<ToolInvocation> for MatrixRecoveryTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("exit_matrix")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(MatrixTool::exit_spec())
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let fence = invocation
                .session
                .services
                .session_extension_data
                .get::<NativeTakeoverFence>()
                .ok_or_else(|| unavailable("missing recovery owner"))?;
            let saved = fence
                .lock()
                .recovery_request
                .clone()
                .ok_or_else(|| unavailable("missing recovery checkpoint"))?;
            let socket = invocation
                .turn
                .config
                .dev_native_takeover_socket
                .as_ref()
                .ok_or_else(|| {
                    unavailable("recovery socket must be configured; host tools remain fenced")
                })?;
            let args = EnterArguments {
                conversation_id: saved["target"]["conversation_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                task: saved["task"].as_str().unwrap_or_default().to_string(),
            };
            let generation = saved["origin_generation"].as_u64().unwrap_or(1);
            exchange(
                &invocation,
                &args,
                socket.as_path(),
                &fence,
                generation,
                true,
            )
            .await
        })
    }
}
impl CoreToolRuntime for MatrixRecoveryTool {
    fn is_native_takeover_control(&self) -> bool {
        true
    }
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

pub(crate) struct MatrixStep {
    pub(crate) scope: Arc<MatrixScope>,
}

struct ScopedCallGuard {
    scope: Arc<MatrixScope>,
    finished: bool,
}
impl Drop for ScopedCallGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.scope.cancel_owned();
    }
}

pub(crate) struct MatrixTool {
    pub(crate) scope: Arc<MatrixScope>,
    pub(crate) spec: ResponsesApiTool,
    pub(crate) exit: bool,
}

impl MatrixTool {
    pub(crate) fn exit_spec() -> ResponsesApiTool {
        ResponsesApiTool {
            name:"exit_matrix".to_string(),
            description:"Reconcile this XVision binding and restore this same origin's context/tool scope. Finish all owned terminals first.".to_string(),
            strict:false,defer_loading:None,output_schema:None,
            parameters:JsonSchema::object(std::collections::BTreeMap::from([
                ("findings".to_string(),JsonSchema::string(Some("Concise evidence-backed findings.".to_string())))
            ]),Some(vec!["findings".to_string()]),Some(false.into())),
        }
    }
}
impl NativeTakeoverFence {
    pub(crate) fn check_resolved_environment(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<(), FunctionCallError> {
        if let Some(scope) = self.overlay()
            && ["exec_command", "write_stdin", "apply_patch"]
                .contains(&invocation.tool_name.name.as_str())
            && invocation.environments.single_id()
                != scope
                    .environments
                    .primary()
                    .map(|env| env.selection.environment_id.as_str())
        {
            return Err(unavailable("tool resolved outside the authorized guest"));
        }
        Ok(())
    }

    pub(crate) fn admit_tool(
        self: &Arc<Self>,
        invocation: &ToolInvocation,
    ) -> Result<DispatchPermit, FunctionCallError> {
        let mut state = self.lock();
        let step = invocation.step_context.extension_data.get::<MatrixStep>();
        match (state.phase, step) {
            (Phase::Origin, None) => {}
            (Phase::Active, Some(step))
                if state
                    .scope
                    .as_ref()
                    .is_some_and(|scope| Arc::ptr_eq(scope, &step.scope))
                    && invocation
                        .step_context
                        .environments
                        .primary()
                        .is_some_and(|env| {
                            env.selection.environment_id
                                == step
                                    .scope
                                    .environments
                                    .primary()
                                    .unwrap()
                                    .selection
                                    .environment_id
                        }) => {}
            _ => {
                return Err(unavailable(
                    "tool belongs to a stale or unauthorized execution scope",
                ));
            }
        }
        state.admitted += 1;
        Ok(DispatchPermit(Arc::clone(self)))
    }
}

impl ToolExecutor<ToolInvocation> for MatrixTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.spec.name.clone())
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(self.spec.clone())
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let fence = invocation
                .session
                .services
                .session_extension_data
                .get::<NativeTakeoverFence>()
                .ok_or_else(|| unavailable("missing scope owner"))?;
            if fence
                .overlay()
                .is_none_or(|active| !Arc::ptr_eq(&active, &self.scope))
            {
                return Err(unavailable("scope is stale or recovering"));
            }
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(unavailable("expected structured tool arguments"));
            };
            let arguments: Value = serde_json::from_str(arguments)
                .map_err(|_| unavailable("invalid tool arguments"))?;
            if self.exit {
                if !invocation
                    .session
                    .services
                    .unified_exec_manager
                    .list_processes()
                    .await
                    .is_empty()
                    || invocation.session.has_background_compaction().await
                    || invocation.session.hooks().has_running_tasks()
                {
                    return Err(unavailable(
                        "finish owned terminals/hooks/compaction before exit",
                    ));
                }
                fence.returning(
                    self.scope.request["origin_generation"]
                        .as_u64()
                        .unwrap_or_default(),
                )?;
            }
            let mut completion = ScopedCallGuard {
                scope: Arc::clone(&self.scope),
                finished: false,
            };
            let started = std::time::Instant::now();
            let result = self
                .scope
                .command(json!({"phase":if self.exit {"exit"}else{"tool"},
                "call_id":invocation.call_id,"tool":self.spec.name,"arguments":arguments.clone(),
                "execution_idle":invocation.session.services.unified_exec_manager.list_processes().await.is_empty()
                    && !invocation.session.hooks().has_running_tasks()}))
                .await;
            match result {
                Ok(value) => {
                    if let Some(update) = value.get("scope_update") {
                        let next = MatrixScope::prepare(
                            &invocation,
                            update.clone(),
                            self.scope.request.clone(),
                        )
                        .await?;
                        next.commands
                            .set(
                                self.scope
                                    .commands
                                    .get()
                                    .ok_or_else(|| unavailable("missing scope transport"))?
                                    .clone(),
                            )
                            .map_err(|_| unavailable("duplicate scope transport"))?;
                        let mut state = fence.lock();
                        if state.phase != Phase::Active
                            || state
                                .scope
                                .as_ref()
                                .is_none_or(|scope| !Arc::ptr_eq(scope, &self.scope))
                        {
                            return Err(unavailable("scope changed during worker completion"));
                        }
                        state.scope = Some(next);
                    }
                    if self.exit {
                        fence.finish_return(
                            self.scope.request["origin_generation"]
                                .as_u64()
                                .unwrap_or_default(),
                        )?;
                    }
                    let success = self.exit || value["success"] == true;
                    let output = if self.exit {
                        value
                    } else {
                        value["output"].clone()
                    };
                    completion.finished = true;
                    if self.scope.mcp_tools.contains_key(&self.spec.name)
                        && output.get("content").is_some()
                    {
                        let result = serde_json::from_value(output)
                            .map_err(|_| unavailable("invalid confirmed MCP result"))?;
                        return Ok(boxed_tool_output(crate::tools::context::McpToolOutput {
                            result,
                            tool_input: arguments,
                            result_metadata_capture_allowed: false,
                            wall_time: started.elapsed(),
                            original_image_detail_supported:
                                crate::original_image_detail::can_request_original_image_detail(
                                    &invocation.step_context.settings.model_info,
                                ),
                            truncation_policy: invocation
                                .step_context
                                .settings
                                .model_info
                                .truncation_policy
                                .into(),
                        }));
                    }
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        output.to_string(),
                        Some(success),
                    )))
                }
                Err(error) => {
                    fence.recover(
                        self.scope.request["origin_generation"]
                            .as_u64()
                            .unwrap_or_default(),
                    );
                    Err(error)
                }
            }
        })
    }
}
impl CoreToolRuntime for MatrixTool {
    fn hook_tool_name(
        &self,
        invocation: &ToolInvocation,
    ) -> crate::tools::hook_names::HookToolName {
        crate::tools::hook_names::HookToolName::new(
            self.scope
                .mcp_tools
                .get(&self.spec.name)
                .map(|(server, tool)| {
                    format!(
                        "{}{tool}",
                        codex_mcp::qualified_mcp_tool_name_prefix(server)
                    )
                })
                .unwrap_or_else(|| invocation.tool_name.name.clone()),
        )
    }
    fn is_native_takeover_control(&self) -> bool {
        self.exit
    }
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "native_takeover_tests.rs"]
mod tests;
