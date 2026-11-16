use super::*;

#[tokio::test]
async fn boundary_fence_and_exact_return_allow_a_second_enter() {
    let fence = Arc::new(NativeTakeoverFence::default());
    let response = fence.begin_sampling().unwrap();
    let generation = fence.enter().unwrap();
    assert!(fence.admit().is_err());
    assert!(fence.begin_sampling().is_err());
    assert!(fence.activate(generation).is_err());
    response.complete();
    fence.wait_for_boundary(generation).await.unwrap();
    fence.activate(generation).unwrap();
    assert!(fence.finish_return(generation).is_err());
    fence.returning(generation).unwrap();
    assert!(fence.finish_return(generation + 1).is_err());
    assert!(fence.admit().is_err());
    fence.finish_return(generation).unwrap();
    drop(response);
    let next = fence.begin_sampling().unwrap();
    let second = fence.enter().unwrap();
    assert!(second > generation);
    next.complete();
    fence.wait_for_boundary(second).await.unwrap();
}

#[test]
fn admitted_work_blocks_enter_without_fencing_another_session() {
    let fence = Arc::new(NativeTakeoverFence::default());
    let other = Arc::new(NativeTakeoverFence::default());
    let permit = fence.admit().unwrap();
    assert!(fence.enter().is_err());
    drop(permit);
    let generation = fence.enter().unwrap();
    assert!(other.admit().is_ok());
    fence.deny_before_dispatch(generation).unwrap();
    assert!(fence.admit().is_ok());
}

#[tokio::test]
async fn configured_notify_does_not_count_as_live_hook_work() {
    let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
    let home = tempfile::tempdir().unwrap();
    let manager =
        crate::thread_manager::ThreadManager::with_models_provider_home_and_state_for_tests(
            codex_login::CodexAuth::from_api_key("fixture"),
            turn.config.model_provider.clone(),
            home.path().to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            None,
        );
    session.services.agent_control = Arc::new(manager.agent_control());
    let mut config = (*turn.config).clone();
    config.notify = Some(vec!["configured-not-executed".to_string()]);
    turn.config = Arc::new(config);
    let fence = Arc::new(NativeTakeoverFence::default());
    fence.begin_sampling().unwrap().complete();
    session
        .services
        .session_extension_data
        .insert_arc(Arc::clone(&fence));
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: Arc::new(session),
        step_context: crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "call".to_string(),
        tool_name: ToolName::plain("enter_matrix"),
        source: ToolCallSource::Direct,
        environments: Default::default(),
        payload: ToolPayload::Function {
            arguments: json!({"conversation_id":"target","task":"inspect"}).to_string(),
        },
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        NativeTakeoverHandler.handle(invocation),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output
            .log_output()
            .contains("developer control socket is not configured"),
        "{}",
        output.log_output()
    );
    assert!(!fence.is_fenced());
}

#[tokio::test]
async fn failed_response_cannot_activate_and_lost_receipt_keeps_fence() {
    let fence = Arc::new(NativeTakeoverFence::default());
    let response = fence.begin_sampling().unwrap();
    let generation = fence.enter().unwrap();
    drop(response);
    assert!(fence.wait_for_boundary(generation).await.is_err());
    assert!(fence.activate(generation).is_err());
    fence.recover(generation);
    assert!(fence.admit().is_err());
    assert!(fence.deny_before_dispatch(generation).is_err());
    fence.returning(generation).unwrap();
    fence.finish_return(generation).unwrap();
    assert!(fence.admit().is_ok());
}

#[test]
fn restored_unconfirmed_enter_cannot_run_origin_tools() {
    use codex_history::RolloutItem;
    use codex_protocol::models::ResponseItem;
    let call: ResponseItem = serde_json::from_value(json!({
        "type":"function_call","name":"enter_matrix","call_id":"c","arguments":"{}"
    }))
    .unwrap();
    let mut history = vec![RolloutItem::ResponseItem(call.into())];
    let empty_alias: ResponseItem = serde_json::from_value(json!({
        "type":"function_call","name":"enter_matrix","namespace":"","call_id":"empty","arguments":"{}"
    })).unwrap();
    assert!(
        Arc::new(NativeTakeoverFence::from_history(&[
            RolloutItem::ResponseItem(empty_alias.into())
        ]))
        .admit()
        .is_err()
    );
    assert!(
        Arc::new(NativeTakeoverFence::from_history(&history))
            .admit()
            .is_err()
    );
    let output: ResponseItem = serde_json::from_value(json!({"type":"function_call_output","call_id":"c",
        "output":json!({"protocol_version":1,"phase":"return","request_id":"c","target_closed":true}).to_string()
    })).unwrap();
    history.push(RolloutItem::ResponseItem(output.into()));
    assert!(
        Arc::new(NativeTakeoverFence::from_history(&history))
            .admit()
            .is_ok()
    );
}

#[tokio::test]
async fn native_context_without_flag_keeps_ordinary_admission() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    assert!(session.native_takeover_admission().unwrap().is_none());
}

#[tokio::test]
async fn native_takeover_inventory_observes_yielded_cells_without_stopping_them() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let cells = &session.services.code_mode_service;
    assert!(!cells.has_active_cells());
    let id = codex_code_mode::CellId::new("cell".to_string());
    cells.mark_cell_ready_for_dispatch(&id, None);
    assert!(cells.has_active_cells());
    assert!(cells.has_active_cells());
    cells.finish_cell_dispatch(&id);
    assert!(!cells.has_active_cells());
}

#[cfg(unix)]
#[tokio::test]
async fn private_ipc_round_trip_returns_to_same_call_and_rejects_a_stale_generation() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    for (stale, wrong_binding) in [(false, false), (true, false), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fence = Arc::new(NativeTakeoverFence::default());
        let boundary = fence.begin_sampling().unwrap();
        let generation = fence.enter().unwrap();
        boundary.complete();
        let observed = Arc::clone(&fence);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let enter: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(enter["origin_ref"]["call_id"], "call");
            assert!(observed.admit().is_err());
            let prepared = json!({"protocol_version":2,"phase":"prepared","request_id":"call",
                "origin_generation":generation,"binding_id":"binding"});
            write
                .write_all(format!("{prepared}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["phase"],
                "activate"
            );
            let receipt = json!({"protocol_version":2,"phase":"return","request_id":"call",
                "origin_generation":if stale { generation + 1 } else { generation },
                "binding_id":if wrong_binding {"foreign"}else{"binding"},"target_closed":true,"outcome":"exited"});
            write
                .write_all(format!("{receipt}\n").as_bytes())
                .await
                .unwrap();
            if !stale && !wrong_binding {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["phase"],
                    "returned"
                );
                let closed = json!({"protocol_version":2,"phase":"returned","request_id":"call",
                    "origin_generation":generation,"binding_id":"binding","target_closed":true});
                write
                    .write_all(format!("{closed}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let turn = Arc::new(turn);
        let invocation = ToolInvocation {
            session: Arc::new(session),
            step_context: crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            call_id: "call".to_string(),
            tool_name: ToolName::plain("enter_matrix"),
            source: ToolCallSource::Direct,
            environments: Default::default(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        let args = EnterArguments {
            conversation_id: "target".to_string(),
            task: "inspect".to_string(),
        };
        let result = exchange(&invocation, &args, &path, &fence, generation, false).await;
        assert_eq!(result.is_err(), stale || wrong_binding);
        assert_eq!(fence.admit().is_err(), stale || wrong_binding);
        server.await.unwrap();
    }
}

fn scope_fixture() -> Arc<MatrixScope> {
    Arc::new(MatrixScope {
        binding_id: "binding".to_string(),
        generation: 1,
        model: "origin-model".to_string(),
        provider: "origin-provider".to_string(),
        context: json!({"task":"Inspect authorized source","knowledge_context":"generic"}),
        instructions: "Scoped authoring guidance".to_string(),
        tools: Vec::new(),
        mcp_tools: Default::default(),
        environments: Default::default(),
        request: json!({"protocol_version":2,"phase":"enter","request_id":"enter",
            "origin_generation":1,"origin_ref":{"thread_id":"origin","turn_id":"turn","call_id":"enter"},
            "target":{"conversation_id":"target"},"task":"Inspect authorized source",
            "origin_inference_identity":{"model":"origin-model","provider":"origin-provider"}}),
        socket: std::path::PathBuf::from("/unavailable/test-control.sock"),
        fence: std::sync::Weak::new(),
        commands: Default::default(),
    })
}

#[tokio::test]
async fn borrowed_mcp_preserves_source_identity_in_pre_and_post_hooks() {
    let mut scope = scope_fixture();
    Arc::get_mut(&mut scope).unwrap().mcp_tools.insert(
        "mcp__transport_hash".to_string(),
        ("source".to_string(), "read".to_string()),
    );
    let mut spec = MatrixTool::exit_spec();
    spec.name = "mcp__transport_hash".to_string();
    let handler = MatrixTool {
        scope,
        spec,
        exit: false,
    };
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: Arc::new(session),
        step_context: crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "mcp-call".to_string(),
        tool_name: ToolName::plain("mcp__transport_hash"),
        source: ToolCallSource::Direct,
        environments: Default::default(),
        payload: ToolPayload::Function {
            arguments: "{\"key\":\"x\"}".to_string(),
        },
    };
    assert_eq!(
        handler
            .pre_tool_use_payload(&invocation)
            .unwrap()
            .tool_name
            .name(),
        "mcp__source__read"
    );
    let output = FunctionToolOutput::from_text("source data".to_string(), Some(true));
    assert_eq!(
        handler
            .post_tool_use_payload(&invocation, &output)
            .unwrap()
            .tool_name
            .name(),
        "mcp__source__read"
    );
}

fn active_scope() -> (Arc<NativeTakeoverFence>, Arc<MatrixScope>) {
    let fence = Arc::new(NativeTakeoverFence::default());
    let boundary = fence.begin_sampling().unwrap();
    let generation = fence.enter().unwrap();
    boundary.complete();
    let scope = scope_fixture();
    fence.install(generation, Arc::clone(&scope)).unwrap();
    (fence, scope)
}

#[test]
fn active_scope_keeps_origin_sampling_and_blocks_host_admission() {
    let (fence, _scope) = active_scope();
    assert!(fence.begin_sampling().is_ok());
    assert!(fence.admit().is_err());
    fence.returning(1).unwrap();
    assert!(fence.begin_sampling().is_err());
    fence.finish_return(1).unwrap();
    assert!(fence.overlay().is_none());
    assert!(fence.begin_sampling().is_ok());
}

#[test]
fn prompt_hint_is_single_ephemeral_and_scoped_after_compact() {
    use codex_protocol::models::ResponseItem;
    let (fence, _scope) = active_scope();
    let message = |role: &str, text: &str| {
        serde_json::from_value::<ResponseItem>(json!({
            "type":"message","role":role,"content":[{"type":"input_text","text":text}]
        }))
        .unwrap()
    };
    let mut prompt=crate::Prompt{input:vec![message("developer","Preserved platform instructions"),
        message("user","Original user constraint: do not publish"),
        message("assistant","Host-only source"),
        serde_json::from_value(json!({"type":"function_call","name":"enter_matrix","call_id":"enter","arguments":"{}"})).unwrap(),
        message("user","Authorized target work")],..Default::default()};
    fence.apply_prompt(&mut prompt);
    fence.apply_prompt(&mut prompt);
    let text = serde_json::to_string(&prompt.input).unwrap();
    assert_eq!(text.matches("<native_takeover_identity>").count(), 1);
    assert!(text.contains("Preserved platform instructions"));
    assert!(!text.contains("Host-only source"));
    assert!(text.contains("Original user constraint: do not publish"));
    // A compacted authorized summary still gets the harness hint on its next request.
    prompt.input = vec![message("user", "Compacted target summary")];
    fence.apply_prompt(&mut prompt);
    assert_eq!(
        serde_json::to_string(&prompt.input)
            .unwrap()
            .matches("<native_takeover_identity>")
            .count(),
        1
    );
    fence.returning(1).unwrap();
    fence.finish_return(1).unwrap();
    prompt.input.clear();
    fence.apply_prompt(&mut prompt);
    assert!(prompt.input.is_empty());
}

#[test]
fn restart_after_activation_enters_cleanup_only_scope_and_rejects_forged_receipt() {
    use codex_history::RolloutItem;
    use codex_protocol::models::ResponseItem;
    let item = |value| {
        RolloutItem::ResponseItem(
            serde_json::from_value::<ResponseItem>(value)
                .unwrap()
                .into(),
        )
    };
    let saved = scope_fixture().request.clone();
    let mut history = vec![
        item(
            json!({"type":"function_call","name":"enter_matrix","call_id":"enter",
        "arguments":r#"{"conversation_id":"target","task":"Inspect authorized source"}"#}),
        ),
        item(
            json!({"type":"function_call_output","call_id":"enter","output":json!({
            "protocol_version":2,"phase":"active","recovery_request":saved}).to_string()}),
        ),
    ];
    let restored = Arc::new(NativeTakeoverFence::from_history(&history));
    assert!(restored.needs_recovery());
    assert!(restored.overlay().is_none());
    assert!(restored.admit().is_err());
    assert!(restored.begin_sampling().is_ok()); // Tools are exit-only, never host.
    history.push(item(json!({"type":"function_call","name":"inspect_dashboard","call_id":"read","arguments":"{}"})));
    history.push(item(json!({"type":"function_call_output","call_id":"read","output":json!({
        "protocol_version":2,"phase":"return","target_closed":true,"request_id":"enter"}).to_string()})));
    assert!(NativeTakeoverFence::from_history(&history).needs_recovery());
    history.push(item(
        json!({"type":"function_call","name":"exit_matrix","call_id":"exit","arguments":"{}"}),
    ));
    history.push(item(json!({"type":"function_call_output","call_id":"exit","output":json!({
        "protocol_version":2,"phase":"return","target_closed":true,"request_id":"enter"}).to_string()})));
    assert!(!NativeTakeoverFence::from_history(&history).is_fenced());
}

#[test]
fn response_scope_does_not_leak_into_another_session() {
    let (active, _scope) = active_scope();
    let other = Arc::new(NativeTakeoverFence::default());
    assert!(active.overlay().is_some());
    assert!(other.overlay().is_none());
    assert!(other.admit().is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn scoped_control_actor_preserves_partial_frames_and_waits_for_return_ack() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    for confirm_return in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("actor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (fence, scope) = active_scope();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let call: Value = serde_json::from_str(&line).unwrap();
            let output = json!({"protocol_version":2,"request_id":"enter","origin_generation":1,
                "binding_id":"binding","phase":"tool_result","call_id":call["call_id"],
                "output":{"rows":[1,2]},"success":true})
            .to_string()
                + "\n";
            write.write_all(&output.as_bytes()[..5]).await.unwrap();
            tokio::task::yield_now().await;
            write.write_all(&output.as_bytes()[5..]).await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let receipt = json!({"protocol_version":2,"request_id":"enter","origin_generation":1,
                "binding_id":"binding","phase":"return","target_closed":true});
            write
                .write_all(format!("{receipt}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["phase"],
                "returned"
            );
            if confirm_return {
                let ack = json!({"protocol_version":2,"request_id":"enter","origin_generation":1,
                    "binding_id":"binding","phase":"returned","target_closed":true});
                write
                    .write_all(format!("{ack}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (read, write) = stream.into_split();
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        scope.commands.set(commands).unwrap();
        let actor = tokio::spawn(control_actor(
            BufReader::new(read),
            write,
            receiver,
            scope.request.clone(),
            Arc::downgrade(&fence),
            1,
        ));
        let result = scope
            .command(
                json!({"phase":"tool","call_id":"read","tool":"inspect_dashboard","arguments":{}}),
            )
            .await
            .unwrap();
        assert_eq!(result["output"]["rows"], json!([1, 2]));
        fence.returning(1).unwrap();
        let result = scope
            .command(json!({"phase":"exit","arguments":{"findings":"Refs only"}}))
            .await;
        assert_eq!(result.is_ok(), confirm_return);
        if confirm_return {
            fence.finish_return(1).unwrap();
        } else {
            assert!(fence.is_fenced());
        }
        actor.await.unwrap();
        server.await.unwrap();
    }
}
