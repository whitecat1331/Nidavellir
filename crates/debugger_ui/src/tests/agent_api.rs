#![expect(clippy::result_large_err)]
use crate::tests::{init_test, init_test_workspace, start_debug_session, start_debug_session_with};
use dap::transport::RequestHandling;
use dap::{
    ErrorResponse, Message, Scope, StackFrame, Variable,
    adapters::DebugTaskDefinition,
    requests::{
        Attach, Continue, DataBreakpointInfo, Disconnect, Evaluate, Initialize, LoadedSources,
        Modules, Pause, ReadMemory, Restart, RestartFrame, Scopes, SetBreakpoints,
        SetDataBreakpoints, SetVariable, StackTrace, StepBack, Threads, Variables,
    },
};
use gpui::{BackgroundExecutor, TestAppContext};
use project::debugger::{
    agent_api::{
        AgentDataBreakpointInput, AgentDebuggerApi, AgentDebuggerSessionStatus,
        AgentDebuggerSnapshotLimits, AgentDebuggerStepKind, AgentDebuggerThreadStatus,
        AgentDebuggerWaitStatus, AgentExceptionBreakpointInput, AgentSourceBreakpointInput,
    },
    session::{ThreadId, ThreadStatus},
};
use project::{FakeFs, Project, WorktreeId};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use task::SharedTaskContext;
use unindent::Unindent as _;
use util::path;

fn agent_api(project: &gpui::Entity<Project>, cx: &mut TestAppContext) -> AgentDebuggerApi {
    project.read_with(cx, |project, _| {
        AgentDebuggerApi::new(project.dap_store(), project.breakpoint_store())
    })
}

fn error_response(message: &str) -> ErrorResponse {
    ErrorResponse {
        error: Some(Message {
            id: 1,
            format: message.into(),
            variables: None,
            send_telemetry: None,
            show_user: None,
            url: None,
            url_label: None,
        }),
    }
}

#[gpui::test]
async fn test_agent_api_breakpoint_editing(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({
            "src": {
                "main.js": "let a = 1;\nlet b = 2;\nlet c = 3;\n",
            }
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let api = agent_api(&project, cx);
    let path = PathBuf::from(path!("/project/src/main.js"));

    let input = AgentSourceBreakpointInput {
        path: path.clone(),
        line: 2,
        enabled: true,
        condition: None,
        hit_condition: None,
        log_message: None,
    };

    // Adding a new breakpoint reports a change.
    let result = cx
        .update(|cx| api.set_source_breakpoint(input.clone(), cx))
        .await
        .unwrap();
    assert!(result.changed);
    assert_eq!(result.line, 2);

    // Setting an identical breakpoint is idempotent.
    let result = cx
        .update(|cx| api.set_source_breakpoint(input.clone(), cx))
        .await
        .unwrap();
    assert!(!result.changed);

    let breakpoints = cx.update(|cx| api.list_breakpoints(cx));
    assert_eq!(breakpoints.len(), 1);
    assert_eq!(breakpoints[0].path, path);
    assert_eq!(breakpoints[0].line, 2);
    assert!(breakpoints[0].enabled);
    assert_eq!(breakpoints[0].condition, None);

    // Setting the same line with a condition updates the breakpoint in place
    // rather than toggling it off.
    let mut conditional = input.clone();
    conditional.condition = Some("a > 0".to_string());
    let result = cx
        .update(|cx| api.set_source_breakpoint(conditional, cx))
        .await
        .unwrap();
    assert!(result.changed);

    let breakpoints = cx.update(|cx| api.list_breakpoints(cx));
    assert_eq!(breakpoints.len(), 1);
    assert_eq!(breakpoints[0].condition.as_deref(), Some("a > 0"));

    // Lines outside the file are rejected instead of being clipped.
    let mut out_of_range = input.clone();
    out_of_range.line = 1000;
    let result = cx
        .update(|cx| api.set_source_breakpoint(out_of_range, cx))
        .await;
    assert!(result.is_err());

    // Removing reports a change the first time and is a no-op afterwards.
    let result = cx
        .update(|cx| api.remove_source_breakpoint(path.clone(), 2, cx))
        .await
        .unwrap();
    assert!(result.changed);
    let result = cx
        .update(|cx| api.remove_source_breakpoint(path.clone(), 2, cx))
        .await
        .unwrap();
    assert!(!result.changed);
    assert!(cx.update(|cx| api.list_breakpoints(cx)).is_empty());
}

#[gpui::test]
async fn test_agent_api_snapshot_is_bounded(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());

    let test_file_content = r#"
        const variable1 = "Value 1";
        const variable2 = "Value 2";
        const variable3 = "Value 3";
        console.log(variable1);
    "#
    .unindent();

    fs.insert_tree(
        path!("/project"),
        json!({
            "src": {
                "test.js": test_file_content,
            }
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());

    client.on_request::<Threads, _>(move |_, _| {
        Ok(dap::ThreadsResponse {
            threads: vec![dap::Thread {
                id: 1,
                name: "Thread 1".into(),
            }],
        })
    });

    let stack_frames = (1..=3)
        .map(|id| StackFrame {
            id,
            name: format!("Stack Frame {id}"),
            source: Some(dap::Source {
                name: Some("test.js".into()),
                path: Some(path!("/project/src/test.js").into()),
                source_reference: None,
                presentation_hint: None,
                origin: None,
                sources: None,
                adapter_data: None,
                checksums: None,
            }),
            line: id,
            column: 1,
            end_line: None,
            end_column: None,
            can_restart: None,
            instruction_pointer_reference: None,
            module_id: None,
            presentation_hint: None,
        })
        .collect::<Vec<_>>();

    client.on_request::<StackTrace, _>({
        let stack_frames = Arc::new(stack_frames);
        move |_, args| {
            assert_eq!(1, args.thread_id);
            Ok(dap::StackTraceResponse {
                stack_frames: (*stack_frames).clone(),
                total_frames: None,
            })
        }
    });

    client.on_request::<Scopes, _>(move |_, _| {
        Ok(dap::ScopesResponse {
            scopes: vec![Scope {
                name: "Locals".into(),
                presentation_hint: None,
                variables_reference: 2,
                named_variables: None,
                indexed_variables: None,
                expensive: false,
                source: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
            }],
        })
    });

    client.on_request::<Variables, _>(move |_, args| {
        assert_eq!(2, args.variables_reference);
        let variable = |name: &str, value: &str| Variable {
            name: name.into(),
            value: value.into(),
            type_: None,
            presentation_hint: None,
            evaluate_name: None,
            variables_reference: 0,
            named_variables: None,
            indexed_variables: None,
            memory_reference: None,
            declaration_location_reference: None,
            value_location_reference: None,
        };
        Ok(dap::VariablesResponse {
            variables: vec![
                variable("variable1", "this value is too long to fit"),
                variable("variable2", "v2"),
                variable("variable3", "v3"),
            ],
        })
    });

    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: None,
            hit_breakpoint_ids: None,
        }))
        .await;

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let session_id = session.read_with(cx, |session, _| session.session_id());

    let limits = AgentDebuggerSnapshotLimits {
        max_frames: 2,
        max_variables_per_scope: 2,
        max_variable_value_length: 8,
        max_output_events: 10,
        max_output_bytes: 1024,
        max_source_context_lines: 3,
    };
    let snapshot = cx
        .update(|cx| api.snapshot(session_id, limits, None, cx))
        .await
        .unwrap();

    assert_eq!(snapshot.session.session_id, session_id);
    assert_eq!(snapshot.session.status, AgentDebuggerSessionStatus::Stopped);
    assert!(snapshot.session.has_ever_stopped);

    assert_eq!(snapshot.threads.len(), 1);
    let thread = &snapshot.threads[0];
    assert_eq!(thread.name, "Thread 1");
    assert_eq!(thread.status, AgentDebuggerThreadStatus::Stopped);

    // Stack frames are bounded by max_frames.
    assert_eq!(thread.frames.len(), 2);
    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note.contains("Stack frames truncated")),
        "expected a stack frame truncation note, got {:?}",
        snapshot.notes
    );

    let frame = &thread.frames[0];
    assert_eq!(frame.name, "Stack Frame 1");
    assert_eq!(
        frame.source_path.as_deref(),
        Some(std::path::Path::new(path!("/project/src/test.js")))
    );

    // Source context is bounded by max_source_context_lines and centered on
    // the frame's line.
    let source_context = frame.source_context.as_ref().unwrap();
    assert!(source_context.lines.len() <= 3);
    assert!(
        source_context
            .lines
            .iter()
            .any(|line| line.line == frame.line as u32)
    );

    // Variables are bounded by max_variables_per_scope and their values by
    // max_variable_value_length.
    assert_eq!(frame.scopes.len(), 1);
    let scope = &frame.scopes[0];
    assert_eq!(scope.name, "Locals");
    assert_eq!(scope.variables.len(), 2);
    assert!(scope.variables_truncated);
    let long_variable = &scope.variables[0];
    assert_eq!(long_variable.name, "variable1");
    assert_eq!(long_variable.value, "this val");
    assert!(long_variable.value_truncated);
    assert!(!scope.variables[1].value_truncated);
}

#[gpui::test]
async fn test_agent_api_snapshot_skips_dap_requests_for_terminated_session(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(path!("/project"), json!({ "main.js": "" }))
        .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());
    let thread_requests = Arc::new(Mutex::new(0));

    client.on_request::<Threads, _>({
        let thread_requests = thread_requests.clone();
        move |_, _| {
            *thread_requests.lock().unwrap() += 1;
            Err(error_response(
                "threads should not be requested for terminated sessions",
            ))
        }
    });

    client
        .fake_event(dap::messages::Events::Output(dap::OutputEvent {
            category: Some(dap::OutputEventCategory::Stdout),
            output: "session finished\n".to_string(),
            data: None,
            variables_reference: None,
            source: None,
            line: None,
            column: None,
            group: None,
            location_reference: None,
        }))
        .await;
    client
        .fake_event(dap::messages::Events::Terminated(Some(
            dap::TerminatedEvent { restart: None },
        )))
        .await;

    for _ in 0..100 {
        if session.read_with(cx, |session, _| session.is_terminated()) {
            break;
        }
        assert!(cx.dispatcher.tick(false));
    }
    assert!(session.read_with(cx, |session, _| session.is_terminated()));

    let api = agent_api(&project, cx);
    let snapshot = cx
        .update(|cx| {
            api.snapshot_session_for_test(
                session.clone(),
                AgentDebuggerSnapshotLimits::default(),
                cx,
            )
        })
        .await
        .unwrap();

    assert_eq!(snapshot.session.session_id, session_id);
    assert_eq!(
        snapshot.session.status,
        AgentDebuggerSessionStatus::Terminated
    );
    assert!(snapshot.threads.is_empty());
    assert_eq!(snapshot.output.len(), 1);
    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note == "Session has ended; threads were not requested")
    );
    assert_eq!(*thread_requests.lock().unwrap(), 0);
}

#[gpui::test]
async fn test_agent_api_snapshot_returns_partial_data_when_nested_requests_fail(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(path!("/project"), json!({ "main.js": "debugger;\n" }))
        .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());

    client.on_request::<Threads, _>(move |_, _| {
        Ok(dap::ThreadsResponse {
            threads: vec![
                dap::Thread {
                    id: 1,
                    name: "Thread with stack error".into(),
                },
                dap::Thread {
                    id: 2,
                    name: "Thread with partial frames".into(),
                },
            ],
        })
    });

    client.on_request::<StackTrace, _>(move |_, args| match args.thread_id {
        1 => Err(error_response("stack trace failed")),
        2 => Ok(dap::StackTraceResponse {
            stack_frames: vec![
                StackFrame {
                    id: 20,
                    name: "Frame with scopes error".into(),
                    source: None,
                    line: 1,
                    column: 1,
                    end_line: None,
                    end_column: None,
                    can_restart: None,
                    instruction_pointer_reference: None,
                    module_id: None,
                    presentation_hint: None,
                },
                StackFrame {
                    id: 21,
                    name: "Frame with variables error".into(),
                    source: None,
                    line: 1,
                    column: 1,
                    end_line: None,
                    end_column: None,
                    can_restart: None,
                    instruction_pointer_reference: None,
                    module_id: None,
                    presentation_hint: None,
                },
            ],
            total_frames: None,
        }),
        thread_id => panic!("unexpected thread id {thread_id}"),
    });

    client.on_request::<Scopes, _>(move |_, args| match args.frame_id {
        20 => Err(error_response("scopes failed")),
        21 => Ok(dap::ScopesResponse {
            scopes: vec![Scope {
                name: "Locals".into(),
                presentation_hint: None,
                variables_reference: 200,
                named_variables: Some(1),
                indexed_variables: None,
                expensive: false,
                source: None,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
            }],
        }),
        frame_id => panic!("unexpected frame id {frame_id}"),
    });

    client.on_request::<Variables, _>(move |_, args| match args.variables_reference {
        200 => Err(error_response("variables failed")),
        variables_reference => panic!("unexpected variables reference {variables_reference}"),
    });

    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: Some(true),
            hit_breakpoint_ids: None,
        }))
        .await;
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let snapshot = cx
        .update(|cx| {
            api.snapshot(
                session_id,
                AgentDebuggerSnapshotLimits {
                    max_frames: 10,
                    max_variables_per_scope: 10,
                    max_variable_value_length: 1024,
                    max_output_events: 10,
                    max_output_bytes: 1024,
                    max_source_context_lines: 0,
                },
                None,
                cx,
            )
        })
        .await
        .unwrap();

    assert_eq!(snapshot.threads.len(), 2);
    assert_eq!(snapshot.threads[0].thread_id.0, 1);
    assert!(snapshot.threads[0].frames.is_empty());
    assert_eq!(snapshot.threads[1].thread_id.0, 2);
    assert_eq!(snapshot.threads[1].frames.len(), 2);
    assert!(snapshot.threads[1].frames[0].scopes.is_empty());
    let scope = &snapshot.threads[1].frames[1].scopes[0];
    assert_eq!(scope.name, "Locals");
    assert!(scope.variables.is_empty());
    assert!(scope.variables_truncated);

    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note.contains("Stack frames for thread")),
        "expected a stack frame failure note, got {:?}",
        snapshot.notes
    );
    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note.contains("Scopes for frame")),
        "expected a scopes failure note, got {:?}",
        snapshot.notes
    );
    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note.contains("Variables for scope")),
        "expected a variables failure note, got {:?}",
        snapshot.notes
    );
}

#[gpui::test]
async fn test_agent_run_to_line_removes_temporary_breakpoint_when_continue_fails(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\nlet b = 2;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());
    let breakpoint_requests = Arc::new(Mutex::new(Vec::<Vec<u64>>::new()));

    client.on_request::<SetBreakpoints, _>({
        let breakpoint_requests = breakpoint_requests.clone();
        move |_, args| {
            let lines = args
                .breakpoints
                .unwrap_or_default()
                .into_iter()
                .map(|breakpoint| breakpoint.line)
                .collect::<Vec<_>>();
            breakpoint_requests.lock().unwrap().push(lines);
            Ok(dap::SetBreakpointsResponse {
                breakpoints: Vec::default(),
            })
        }
    });

    client.on_request::<Continue, _>(move |_, _| Err(error_response("continue failed")));

    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: None,
            hit_breakpoint_ids: None,
        }))
        .await;
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let result = cx
        .update(|cx| {
            api.run_to_line(
                session_id,
                project::debugger::session::ThreadId(1),
                PathBuf::from(path!("/project/src/main.js")),
                2,
                Duration::from_millis(100),
                cx,
            )
        })
        .await;

    assert!(result.is_err());
    let breakpoint_requests = breakpoint_requests.lock().unwrap();
    assert_eq!(&*breakpoint_requests, &[vec![2], Vec::<u64>::new()]);
}

/// ISSUE-0046: a control request whose response never arrives must not wedge the
/// tool. `pause_thread` must return within the caller's timeout (reported as
/// `TimedOut`) instead of awaiting the adapter's acknowledgement forever.
#[gpui::test]
async fn test_agent_pause_is_bounded_when_adapter_never_acknowledges(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\nlet b = 2;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());
    // The adapter accepts the request but never answers it, which is exactly the
    // wedged-debuggee condition.
    client.on_request_ext::<Pause, _>(|_, _| RequestHandling::Defer);
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let pause =
        cx.update(|cx| api.pause_thread(session_id, ThreadId(1), Duration::from_millis(50), cx));
    futures::pin_mut!(pause);
    let outcome = futures::future::select(pause, executor.timer(Duration::from_secs(2))).await;
    match outcome {
        futures::future::Either::Left((result, _)) => {
            let result = result.expect("pause_thread should report a bounded outcome");
            assert_eq!(result.status, AgentDebuggerWaitStatus::TimedOut);
        }
        futures::future::Either::Right(_) => {
            panic!("pause_thread hung: the request send is not bounded by the timeout")
        }
    }
}

#[gpui::test]
async fn test_start_debug_session_rejects_invalid_explicit_worktree_id(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(path!("/project"), json!({ "main.js": "" }))
        .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let invalid_worktree_id = WorktreeId::from_usize(usize::MAX);

    let error = workspace
        .update(cx, |multi, window, cx| {
            multi.workspace().update(cx, |workspace, cx| {
                workspace.start_debug_session(
                    task::DebugScenario {
                        adapter: "fake-adapter".into(),
                        label: "agent session".into(),
                        build: None,
                        config: json!({ "request": "launch" }),
                        tcp_connection: None,
                    },
                    SharedTaskContext::default(),
                    None,
                    Some(invalid_worktree_id),
                    window,
                    cx,
                )
            })
        })
        .unwrap()
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains(&format!("Could not find worktree {invalid_worktree_id}")),
        "unexpected error: {error:#}"
    );
}

#[gpui::test]
async fn test_start_debug_session_returns_session_info(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(path!("/project"), json!({ "main.js": "" }))
        .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;

    let _subscription = project::debugger::test::intercept_debug_sessions(cx, |_| {});
    let info = workspace
        .update(cx, |multi, window, cx| {
            multi.workspace().update(cx, |workspace, cx| {
                workspace.start_debug_session(
                    task::DebugScenario {
                        adapter: "fake-adapter".into(),
                        label: "agent session".into(),
                        build: None,
                        config: json!({ "request": "launch" }),
                        tcp_connection: None,
                    },
                    SharedTaskContext::default(),
                    None,
                    None,
                    window,
                    cx,
                )
            })
        })
        .unwrap()
        .unwrap();

    assert_eq!(info.label, "agent session");
    assert_eq!(info.adapter, "fake-adapter");

    cx.run_until_parked();

    // The reported session id refers to a real session in the DAP store.
    let api = agent_api(&project, cx);
    let sessions = cx.update(|cx| api.list_sessions(cx));
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id.to_proto(), info.session_id);
    assert_eq!(sessions[0].label.as_deref(), Some("agent session"));
    assert_eq!(sessions[0].adapter, "fake-adapter");
}

#[gpui::test]
async fn test_agent_api_snapshot_frame_budget_prioritizes_stop_thread(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(path!("/project"), json!({ "main.js": "debugger;\n" }))
        .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());

    client.on_request::<Threads, _>(move |_, _| {
        Ok(dap::ThreadsResponse {
            threads: vec![
                dap::Thread {
                    id: 1,
                    name: "Worker A".into(),
                },
                dap::Thread {
                    id: 2,
                    name: "Main".into(),
                },
                dap::Thread {
                    id: 3,
                    name: "Worker B".into(),
                },
            ],
        })
    });

    let frame = |id: u64| StackFrame {
        id,
        name: format!("Frame {id}"),
        source: None,
        line: 1,
        column: 1,
        end_line: None,
        end_column: None,
        can_restart: None,
        instruction_pointer_reference: None,
        module_id: None,
        presentation_hint: None,
    };
    client.on_request::<StackTrace, _>(move |_, args| {
        let frames = match args.thread_id {
            1 => vec![frame(101), frame(102), frame(103)],
            2 => vec![frame(201)],
            3 => vec![frame(301)],
            thread_id => panic!("unexpected thread id {thread_id}"),
        };
        Ok(dap::StackTraceResponse {
            stack_frames: frames,
            total_frames: None,
        })
    });

    client.on_request::<Scopes, _>(move |_, _| Ok(dap::ScopesResponse { scopes: Vec::new() }));

    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(2),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: Some(true),
            hit_breakpoint_ids: None,
        }))
        .await;
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let snapshot = cx
        .update(|cx| {
            api.snapshot(
                session_id,
                AgentDebuggerSnapshotLimits {
                    max_frames: 2,
                    ..Default::default()
                },
                Some(ThreadId(2)),
                cx,
            )
        })
        .await
        .unwrap();

    // The stop-reported thread is listed first and always gets frames; the
    // remaining budget is spread across the other stopped threads instead of
    // being consumed entirely by the first worker thread in adapter order.
    let thread_ids = snapshot
        .threads
        .iter()
        .map(|thread| thread.thread_id.0)
        .collect::<Vec<_>>();
    assert_eq!(thread_ids, vec![2, 1, 3]);
    assert_eq!(snapshot.threads[0].frames.len(), 1);
    assert_eq!(snapshot.threads[0].frames[0].frame_id, 201);
    assert_eq!(snapshot.threads[1].frames.len(), 1);
    assert!(snapshot.threads[2].frames.is_empty());
    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note.contains("max_frames budget exhausted")),
        "expected a budget-exhausted note, got {:?}",
        snapshot.notes
    );
}

#[gpui::test]
async fn test_agent_api_snapshot_keeps_synthetic_placeholder_thread(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(path!("/project"), json!({ "main.js": "" }))
        .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());

    // Delve synthesizes a "Dummy" placeholder thread before the first real
    // goroutine exists; it has no stack. The fake adapter is NOT named
    // "Delve", so this also proves the snapshot detects the placeholder by
    // thread name rather than by the adapter's name.
    client.on_request::<Threads, _>(move |_, _| {
        Ok(dap::ThreadsResponse {
            threads: vec![dap::Thread {
                id: 1,
                name: "Dummy".into(),
            }],
        })
    });

    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: None,
            hit_breakpoint_ids: None,
        }))
        .await;

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let snapshot = cx
        .update(|cx| api.snapshot(session_id, AgentDebuggerSnapshotLimits::default(), None, cx))
        .await
        .unwrap();

    assert_eq!(snapshot.threads.len(), 1);
    let thread = &snapshot.threads[0];
    assert_eq!(thread.name, "Dummy");
    assert_eq!(thread.status, AgentDebuggerThreadStatus::Stopped);
    assert!(thread.frames.is_empty());
    assert!(
        snapshot
            .notes
            .iter()
            .any(|note| note.contains("Synthetic `Dummy` thread has no stack")),
        "expected the synthetic-placeholder note, got {:?}",
        snapshot.notes
    );
}

#[gpui::test]
async fn test_agent_api_evaluate(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());

    client.on_request::<Evaluate, _>(move |_, args| {
        assert_eq!(args.expression, "1 + 1");
        Ok(dap::EvaluateResponse {
            result: "2".into(),
            type_: Some("int".into()),
            presentation_hint: None,
            variables_reference: 0,
            named_variables: None,
            indexed_variables: None,
            memory_reference: None,
            value_location_reference: None,
        })
    });

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let result = cx
        .update(|cx| {
            api.evaluate(
                session_id,
                "1 + 1".into(),
                dap::EvaluateArgumentsContext::Repl,
                None,
                cx,
            )
        })
        .await
        .unwrap();

    assert_eq!(result.result, "2");
    assert_eq!(result.type_name.as_deref(), Some("int"));
    assert_eq!(result.variables_reference, 0);
}

#[gpui::test]
async fn test_agent_api_set_variable(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |client| {
        // Advertise setVariable support before the session sends its
        // `initialize` request, so the capability gate passes.
        client.on_request::<Initialize, _>(move |_, _| {
            Ok(dap::Capabilities {
                supports_set_variable: Some(true),
                ..Default::default()
            })
        });
        client.on_request::<SetVariable, _>(move |_, args| {
            assert_eq!(args.name, "a");
            assert_eq!(args.value, "2");
            assert_eq!(args.variables_reference, 100);
            Ok(dap::SetVariableResponse {
                value: "2".into(),
                type_: Some("int".into()),
                variables_reference: Some(0),
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
                value_location_reference: None,
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let result = cx
        .update(|cx| api.set_variable(session_id, None, 100, "a".into(), "2".into(), cx))
        .await
        .unwrap();

    assert_eq!(result.value, "2");
    assert_eq!(result.type_name.as_deref(), Some("int"));
    assert_eq!(result.variables_reference, Some(0));
}

#[gpui::test]
async fn test_agent_api_set_variable_reconciles_thread_state(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |client| {
        client.on_request::<Initialize, _>(move |_, _| {
            Ok(dap::Capabilities {
                supports_set_variable: Some(true),
                ..Default::default()
            })
        });
        client.on_request::<SetVariable, _>(move |_, args| {
            assert_eq!(args.name, "a");
            assert_eq!(args.value, "2");
            assert_eq!(args.variables_reference, 100);
            Ok(dap::SetVariableResponse {
                value: "2".into(),
                type_: Some("int".into()),
                variables_reference: Some(0),
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
                value_location_reference: None,
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    // Several adapters implement `setVariable` by briefly resuming the debuggee:
    // a `continued` event flips the thread to Running before the adapter
    // re-stops. A successful `setVariable` must reconcile the thread back to
    // Stopped so the next control op is not rejected as "thread not stopped".
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());
    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: Some(true),
            hit_breakpoint_ids: None,
        }))
        .await;
    client
        .fake_event(dap::messages::Events::Continued(dap::ContinuedEvent {
            thread_id: 1,
            all_threads_continued: Some(true),
        }))
        .await;
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    cx.update(|cx| api.set_variable(session_id, None, 100, "a".into(), "2".into(), cx))
        .await
        .unwrap();

    let status = session.read_with(cx, |session, _| session.thread_status(ThreadId(1)));
    assert_eq!(status, ThreadStatus::Stopped);
}

#[gpui::test]
async fn test_agent_api_set_variable_rejects_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    // The fake adapter advertises no capabilities, so `set_variable` must
    // fail fast with a clear unsupported-capability error rather than sending
    // a `setVariable` request.
    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| api.set_variable(session_id, None, 100, "a".into(), "2".into(), cx))
        .await
        .expect_err("set_variable should fail when the adapter lacks support");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_read_memory(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |client| {
        client.on_request::<Initialize, _>(move |_, _| {
            Ok(dap::Capabilities {
                supports_read_memory_request: Some(true),
                ..Default::default()
            })
        });
        client.on_request::<ReadMemory, _>(move |_, args| {
            assert_eq!(args.memory_reference, "0x1000");
            assert_eq!(args.count, 4);
            Ok(dap::ReadMemoryResponse {
                address: "0x1000".into(),
                unreadable_bytes: None,
                data: Some("AQIDBA==".into()),
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let result = cx
        .update(|cx| api.read_memory(session_id, "0x1000".into(), None, 4, cx))
        .await
        .unwrap();

    assert_eq!(result.address, "0x1000");
    assert_eq!(result.byte_count, 4);
    assert_eq!(result.unreadable_bytes, None);
    assert_eq!(result.content, "AQIDBA==");
}

#[gpui::test]
async fn test_agent_api_read_memory_rejects_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    // The fake adapter advertises no capabilities, so `read_memory` must fail
    // fast with a clear unsupported-capability error rather than sending a
    // `readMemory` request.
    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| api.read_memory(session_id, "0x1000".into(), None, 4, cx))
        .await
        .expect_err("read_memory should fail when the adapter lacks support");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_list_modules(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |client| {
        client.on_request::<Initialize, _>(move |_, _| {
            Ok(dap::Capabilities {
                supports_modules_request: Some(true),
                ..Default::default()
            })
        });
        client.on_request::<Modules, _>(move |_, _| {
            Ok(dap::ModulesResponse {
                modules: vec![dap::Module {
                    id: dap::ModuleId::Number(1),
                    name: "main".into(),
                    path: Some("/project/main.js".into()),
                    is_optimized: Some(false),
                    is_user_code: Some(true),
                    version: None,
                    symbol_status: Some("Symbols loaded.".into()),
                    symbol_file_path: None,
                    date_time_stamp: None,
                    address_range: None,
                }],
                total_modules: Some(1),
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let modules = cx
        .update(|cx| api.list_modules(session_id, cx))
        .await
        .unwrap();

    assert_eq!(modules.len(), 1);
    let module = &modules[0];
    assert_eq!(module.id, "1");
    assert_eq!(module.name, "main");
    assert_eq!(module.path.as_deref(), Some("/project/main.js"));
    assert_eq!(module.symbol_status.as_deref(), Some("Symbols loaded."));
    assert_eq!(module.is_optimized, Some(false));
}

#[gpui::test]
async fn test_agent_api_list_loaded_sources(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |client| {
        client.on_request::<Initialize, _>(move |_, _| {
            Ok(dap::Capabilities {
                supports_loaded_sources_request: Some(true),
                ..Default::default()
            })
        });
        client.on_request::<LoadedSources, _>(move |_, _| {
            Ok(dap::LoadedSourcesResponse {
                sources: vec![dap::Source {
                    name: Some("main.js".into()),
                    path: Some("/project/main.js".into()),
                    source_reference: Some(0),
                    presentation_hint: None,
                    origin: None,
                    sources: None,
                    adapter_data: None,
                    checksums: None,
                }],
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let sources = cx
        .update(|cx| api.list_loaded_sources(session_id, cx))
        .await
        .unwrap();

    assert_eq!(sources.len(), 1);
    let source = &sources[0];
    assert_eq!(source.name.as_deref(), Some("main.js"));
    assert_eq!(source.path.as_deref(), Some("/project/main.js"));
    assert_eq!(source.source_reference, Some(0));
}

#[gpui::test]
async fn test_agent_api_listing_rejects_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let modules_error = cx
        .update(|cx| api.list_modules(session_id, cx))
        .await
        .expect_err("list_modules should fail when the adapter lacks support");
    assert!(
        modules_error.to_string().contains("does not support"),
        "unexpected error: {modules_error}"
    );

    let sources_error = cx
        .update(|cx| api.list_loaded_sources(session_id, cx))
        .await
        .expect_err("list_loaded_sources should fail when the adapter lacks support");
    assert!(
        sources_error.to_string().contains("does not support"),
        "unexpected error: {sources_error}"
    );
}

#[gpui::test]
async fn test_agent_api_watch_expressions(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |client| {
        client.on_request::<Evaluate, _>(move |_, args| {
            Ok(dap::EvaluateResponse {
                result: args.expression,
                type_: Some("int".into()),
                presentation_hint: None,
                variables_reference: 0,
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
                value_location_reference: None,
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);

    cx.update(|cx| api.add_watch_expression(session_id, "a + b".into(), None, cx))
        .await
        .unwrap();

    let expressions = cx
        .update(|cx| api.list_watch_expressions(session_id, cx))
        .unwrap();
    assert_eq!(expressions.len(), 1);
    assert_eq!(expressions[0].expression, "a + b");
    assert_eq!(expressions[0].value, "a + b");
    assert_eq!(expressions[0].variables_reference, 0);

    cx.update(|cx| api.remove_watch_expression(session_id, "a + b".into(), cx))
        .await
        .unwrap();
    let expressions = cx
        .update(|cx| api.list_watch_expressions(session_id, cx))
        .unwrap();
    assert!(expressions.is_empty());
}

#[gpui::test]
async fn test_agent_api_select_history_out_of_bounds(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);

    // A fresh session has no historic snapshots.
    let history = cx.update(|cx| api.list_history(session_id, cx)).unwrap();
    assert!(history.is_empty());

    // Selecting any index is therefore out of bounds.
    let error = cx
        .update(|cx| api.select_history(session_id, 0, cx))
        .await
        .expect_err("select_history should fail when history is empty");
    assert!(
        error.to_string().contains("out of bounds"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_step_back(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let step_back_thread_id = Arc::new(Mutex::new(None));
    let step_back_granularity = Arc::new(Mutex::new(None));
    let session = start_debug_session(&workspace, cx, {
        let step_back_thread_id = step_back_thread_id.clone();
        let step_back_granularity = step_back_granularity.clone();
        move |client| {
            client.on_request::<Initialize, _>(move |_, _| {
                Ok(dap::Capabilities {
                    supports_step_back: Some(true),
                    supports_stepping_granularity: Some(true),
                    ..Default::default()
                })
            });
            client.on_request::<StepBack, _>({
                let step_back_thread_id = step_back_thread_id.clone();
                let step_back_granularity = step_back_granularity.clone();
                move |_, args| {
                    *step_back_thread_id.lock().unwrap() = Some(args.thread_id);
                    *step_back_granularity.lock().unwrap() = args.granularity;
                    Ok(())
                }
            });
        }
    })
    .unwrap();

    let session_id = session.read_with(cx, |session, _| session.session_id());
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());
    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: None,
            hit_breakpoint_ids: None,
        }))
        .await;
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    cx.update(|cx| {
        api.step_thread(
            session_id,
            ThreadId(1),
            AgentDebuggerStepKind::Back,
            dap::SteppingGranularity::Instruction,
            Duration::from_millis(100),
            cx,
        )
    })
    .await
    .expect("step_back should succeed when the adapter supports it");

    assert_eq!(*step_back_thread_id.lock().unwrap(), Some(1));
    assert_eq!(
        *step_back_granularity.lock().unwrap(),
        Some(dap::SteppingGranularity::Instruction)
    );
}

#[gpui::test]
async fn test_agent_api_step_back_rejects_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let client = session.update(cx, |session, _| session.adapter_client().unwrap());
    let session_id = session.read_with(cx, |session, _| session.session_id());

    client
        .fake_event(dap::messages::Events::Stopped(dap::StoppedEvent {
            reason: dap::StoppedEventReason::Pause,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: None,
            hit_breakpoint_ids: None,
        }))
        .await;
    cx.run_until_parked();

    // The fake adapter advertises no capabilities, so `step_thread` with the
    // back kind must fail fast with a clear unsupported-capability error rather
    // than sending a `stepBack` request.
    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| {
            api.step_thread(
                session_id,
                ThreadId(1),
                AgentDebuggerStepKind::Back,
                dap::SteppingGranularity::Line,
                Duration::from_millis(100),
                cx,
            )
        })
        .await
        .expect_err("step_back should fail when the adapter lacks support");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_restart(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let restart_received = Arc::new(Mutex::new(false));
    let session = start_debug_session(&workspace, cx, {
        let restart_received = restart_received.clone();
        move |client| {
            client.on_request::<Initialize, _>(move |_, _| {
                Ok(dap::Capabilities {
                    supports_restart_request: Some(true),
                    ..Default::default()
                })
            });
            client.on_request::<Restart, _>({
                let restart_received = restart_received.clone();
                move |_, _| {
                    *restart_received.lock().unwrap() = true;
                    Ok(())
                }
            });
        }
    })
    .unwrap();

    let session_id = session.read_with(cx, |session, _| session.session_id());
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    cx.update(|cx| api.restart_session(session_id, cx))
        .await
        .expect("restart should succeed when the adapter supports it");

    assert!(*restart_received.lock().unwrap());
}

#[gpui::test]
async fn test_agent_api_restart_rejects_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    // The fake adapter advertises no capabilities, so `restart` must fail fast
    // with a clear unsupported-capability error rather than sending a restart
    // request.
    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| api.restart_session(session_id, cx))
        .await
        .expect_err("restart should fail when the adapter lacks support");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_restart_stack_frame(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let restart_frame_id = Arc::new(Mutex::new(None));
    let session = start_debug_session(&workspace, cx, {
        let restart_frame_id = restart_frame_id.clone();
        move |client| {
            client.on_request::<Initialize, _>(move |_, _| {
                Ok(dap::Capabilities {
                    supports_restart_frame: Some(true),
                    ..Default::default()
                })
            });
            client.on_request::<RestartFrame, _>({
                let restart_frame_id = restart_frame_id.clone();
                move |_, args| {
                    *restart_frame_id.lock().unwrap() = Some(args.frame_id);
                    Ok(())
                }
            });
        }
    })
    .unwrap();

    let session_id = session.read_with(cx, |session, _| session.session_id());
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    cx.update(|cx| api.restart_stack_frame(session_id, 7, cx))
        .await
        .expect("restart frame should succeed when the adapter supports it");

    assert_eq!(*restart_frame_id.lock().unwrap(), Some(7));
}

#[gpui::test]
async fn test_agent_api_restart_stack_frame_rejects_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| api.restart_stack_frame(session_id, 7, cx))
        .await
        .expect_err("restart frame should fail when the adapter lacks support");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_detach(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let disconnect_received = Arc::new(Mutex::new(false));
    let session = start_debug_session_with(
        &workspace,
        cx,
        DebugTaskDefinition {
            adapter: "fake-adapter".into(),
            label: "test".into(),
            config: json!({ "request": "attach" }),
            tcp_connection: None,
        },
        {
            let disconnect_received = disconnect_received.clone();
            move |client| {
                client.on_request::<Attach, _>(move |_, _| Ok(()));
                client.on_request::<Disconnect, _>({
                    let disconnect_received = disconnect_received.clone();
                    move |_, args| {
                        assert_eq!(args.terminate_debuggee, Some(false));
                        assert_eq!(args.suspend_debuggee, Some(false));
                        *disconnect_received.lock().unwrap() = true;
                        Ok(())
                    }
                });
            }
        },
    )
    .unwrap();

    let session_id = session.read_with(cx, |session, _| session.session_id());
    cx.run_until_parked();

    let api = agent_api(&project, cx);
    cx.update(|cx| api.detach_session(session_id, cx))
        .await
        .expect("detach should succeed for an attach session");

    assert!(*disconnect_received.lock().unwrap());
}

#[gpui::test]
async fn test_agent_api_detach_rejects_when_not_attached(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| api.detach_session(session_id, cx))
        .await
        .expect_err("detach should fail for a launch session");

    assert!(
        error.to_string().contains("not attached"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_exception_breakpoints(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, move |client| {
        client.on_request::<Initialize, _>(move |_, _| {
            Ok(dap::Capabilities {
                exception_breakpoint_filters: Some(vec![dap::ExceptionBreakpointsFilter {
                    filter: "uncaught".into(),
                    label: "Uncaught Exceptions".into(),
                    description: None,
                    default: Some(false),
                    supports_condition: None,
                    condition_description: None,
                }]),
                ..Default::default()
            })
        });
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);

    // The advertised filter is surfaced, disabled by default.
    let breakpoints = cx
        .update(|cx| api.list_exception_breakpoints(session_id, cx))
        .unwrap();
    assert_eq!(breakpoints.len(), 1);
    assert_eq!(breakpoints[0].id, "uncaught");
    assert_eq!(breakpoints[0].label, "Uncaught Exceptions");
    assert!(!breakpoints[0].enabled);

    // Enabling is an idempotent set, not a toggle.
    let result = cx
        .update(|cx| {
            api.set_exception_breakpoints(
                session_id,
                vec![AgentExceptionBreakpointInput {
                    id: "uncaught".into(),
                    enabled: true,
                }],
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].enabled);

    // Setting an unknown filter fails with a clear error.
    let error = cx
        .update(|cx| {
            api.set_exception_breakpoints(
                session_id,
                vec![AgentExceptionBreakpointInput {
                    id: "missing".into(),
                    enabled: true,
                }],
                cx,
            )
        })
        .await
        .expect_err("setting an unknown filter should fail");
    assert!(
        error
            .to_string()
            .contains("unknown exception breakpoint filter"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_exception_breakpoints_reject_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| {
            api.set_exception_breakpoints(
                session_id,
                vec![AgentExceptionBreakpointInput {
                    id: "uncaught".into(),
                    enabled: true,
                }],
                cx,
            )
        })
        .await
        .expect_err("setting exception breakpoints should fail when unsupported");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_data_breakpoints(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let set_data_received = Arc::new(Mutex::new(Vec::new()));
    let session = start_debug_session(&workspace, cx, {
        let set_data_received = set_data_received.clone();
        move |client| {
            client.on_request::<Initialize, _>(move |_, _| {
                Ok(dap::Capabilities {
                    supports_data_breakpoints: Some(true),
                    ..Default::default()
                })
            });
            client.on_request::<DataBreakpointInfo, _>(move |_, args| {
                assert_eq!(args.variables_reference, Some(100));
                assert_eq!(args.name, "a");
                Ok(dap::DataBreakpointInfoResponse {
                    data_id: Some("data-1".into()),
                    description: "a".into(),
                    access_types: None,
                    can_persist: None,
                })
            });
            client.on_request::<SetDataBreakpoints, _>({
                let set_data_received = set_data_received.clone();
                move |_, args| {
                    *set_data_received.lock().unwrap() = args.breakpoints;
                    Ok(dap::SetDataBreakpointsResponse {
                        breakpoints: vec![],
                    })
                }
            });
        }
    })
    .unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let result = cx
        .update(|cx| {
            api.set_data_breakpoints(
                session_id,
                vec![AgentDataBreakpointInput {
                    variables_reference: 100,
                    name: "a".into(),
                    access_type: Some("write".into()),
                    condition: None,
                    hit_condition: None,
                }],
                cx,
            )
        })
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].data_id, "data-1");
    assert_eq!(result[0].access_type.as_deref(), Some("write"));
    assert!(result[0].enabled);

    cx.run_until_parked();
    let sent = set_data_received.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].data_id, "data-1");
    assert_eq!(
        sent[0].access_type,
        Some(dap::DataBreakpointAccessType::Write)
    );
}

#[gpui::test]
async fn test_agent_api_data_breakpoints_reject_when_unsupported(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    let error = cx
        .update(|cx| {
            api.set_data_breakpoints(
                session_id,
                vec![AgentDataBreakpointInput {
                    variables_reference: 100,
                    name: "a".into(),
                    access_type: None,
                    condition: None,
                    hit_condition: None,
                }],
                cx,
            )
        })
        .await
        .expect_err("setting data breakpoints should fail when unsupported");

    assert!(
        error.to_string().contains("does not support"),
        "unexpected error: {error}"
    );
}

#[gpui::test]
async fn test_agent_api_set_ignore_breakpoints(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.js": "let a = 1;\n" } }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let workspace = init_test_workspace(&project, cx).await;
    let session = start_debug_session(&workspace, cx, |_| {}).unwrap();
    let session_id = session.read_with(cx, |session, _| session.session_id());

    cx.run_until_parked();

    let api = agent_api(&project, cx);
    cx.update(|cx| api.set_ignore_breakpoints(session_id, true, cx))
        .await
        .unwrap();
    assert!(session.read_with(cx, |session, _| session.ignore_breakpoints()));

    cx.update(|cx| api.set_ignore_breakpoints(session_id, false, cx))
        .await
        .unwrap();
    assert!(!session.read_with(cx, |session, _| session.ignore_breakpoints()));
}

#[gpui::test]
async fn test_agent_api_clear_breakpoints(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/project"),
        json!({
            "src": {
                "main.js": "let a = 1;\nlet b = 2;\nlet c = 3;\n",
            }
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let api = agent_api(&project, cx);
    let path = PathBuf::from(path!("/project/src/main.js"));

    let input = AgentSourceBreakpointInput {
        path: path.clone(),
        line: 2,
        enabled: true,
        condition: None,
        hit_condition: None,
        log_message: None,
    };
    cx.update(|cx| api.set_source_breakpoint(input, cx))
        .await
        .unwrap();
    assert_eq!(cx.update(|cx| api.list_breakpoints(cx)).len(), 1);

    cx.update(|cx| api.clear_breakpoints(cx)).await.unwrap();
    assert_eq!(cx.update(|cx| api.list_breakpoints(cx)).len(), 0);
}
