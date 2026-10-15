//! Behavior at the same tool seam the canvas agent uses.
use super::*;
use serde_json::{json, Value};

fn server() -> CanvasServer {
    CanvasServer::new(Arc::new(AppState::new(
        std::path::PathBuf::from("."),
        None,
        true,
    )))
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

async fn script(server: &CanvasServer, beats: Value) -> Value {
    let result = server
        .canvas_script(Parameters(
            serde_json::from_value(json!({"beats": beats})).unwrap(),
        ))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    serde_json::from_str(text(&result)).unwrap()
}

async fn read(server: &CanvasServer, args: Value) -> CallToolResult {
    server
        .canvas_read(Parameters(serde_json::from_value(args).unwrap()))
        .await
        .unwrap()
}

async fn status(server: &CanvasServer) -> Value {
    serde_json::from_str(text(&server.canvas_status().await.unwrap())).unwrap()
}

#[tokio::test]
async fn selected_frame_reads_and_updates_preserve_existing_identities() {
    let server = server();
    let created = script(
        &server,
        json!([
            {"draw":{"as":"worker", "kind":"rectangle", "text":"Worker"}},
            {"draw":{"as":"queue", "kind":"rectangle", "text":"Queue"}},
            {"connect":{"from":"queue", "to":"worker", "label":"deliver"}},
            {"draw":{"as":"workers", "kind":"frame", "name":"Workers", "around":["worker"]}}
        ]),
    )
    .await;
    let worker = created["created"]["worker"].as_str().unwrap();
    let frame = created["created"]["workers"].as_str().unwrap();
    let workspace = server.current().await;
    let worker_id = workspace.resolve_id(worker).await;
    *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
        selection: vec![worker_id.clone()],
        camera: None,
        viewport: None,
    });
    let selected = read(&server, json!({"selected":true})).await;
    assert!(text(&selected).contains("Worker"));
    assert!(!text(&selected).contains("Queue"));
    let context = status(&server).await;
    let revision = context["revision"].as_u64().unwrap();
    let edited = server.canvas_script(Parameters(serde_json::from_value(json!({
        "context":context["context"], "revision":revision,
        "beats":[{"update":{"id":worker,"text":"At-least-once worker"}}, {"update":{"id":frame,"y":240}}]
    })).unwrap())).await.unwrap();
    assert_ne!(edited.is_error, Some(true), "{}", text(&edited));
    assert_eq!(workspace.resolve_id(worker).await, worker_id);
    let scoped = read(&server, json!({"frame":frame, "detail":true})).await;
    assert!(text(&scoped).contains("At-least-once worker"));
    assert!(text(&scoped).contains("frameId"));
    assert!(!text(&scoped).contains("Queue"));
}

#[tokio::test]
async fn failed_revisions_and_partial_verbs_commit_nothing() {
    let server = server();
    let created = script(
        &server,
        json!([{"draw":{"as":"worker","kind":"rectangle","text":"Worker"}}]),
    )
    .await;
    let workspace = server.current().await;
    let before = workspace.document_json().await;
    let revision = workspace.revision.load(Ordering::SeqCst);
    let result = server
        .canvas_script(Parameters(
            serde_json::from_value(json!({"beats":[
                {"update":{"id":created["created"]["worker"],"text":"changed"}},
                {"update":{"id":"missing","x":50}}
            ]}))
            .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("beat 2"));
    assert_eq!(workspace.document_json().await, before);
    assert_eq!(workspace.revision.load(Ordering::SeqCst), revision);
    let result = server
        .canvas_emphasis(Parameters(EmphasisArgs {
            ids: vec![
                created["created"]["worker"].as_str().unwrap().into(),
                "missing".into(),
            ],
            mode: "dim".into(),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(workspace.document_json().await, before);
}

#[tokio::test]
async fn workspace_switch_rejects_explicit_and_session_context() {
    let server = server();
    let observed = status(&server).await;
    let beta = server.state.workspace("beta").await;
    beta.pages.store(1, Ordering::SeqCst);
    beta.note_page_activity();
    for context in [
        Some(observed["context"].as_str().unwrap().to_string()),
        None,
    ] {
        let result = server
            .canvas_script(Parameters(ScriptArgs {
                beats: serde_json::from_value(
                    json!([{"draw":{"kind":"sticky","text":"wrong workspace"}}]),
                )
                .unwrap(),
                context,
                revision: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(text(&result).contains("stale_context"));
    }
    assert_eq!(beta.doc.lock().await.live_count(), 0);
    status(&server).await;
    script(
        &server,
        json!([{"draw":{"kind":"sticky","text":"on beta"}}]),
    )
    .await;
    assert_eq!(beta.doc.lock().await.live_count(), 2);
}

#[tokio::test]
async fn revert_preserves_unrelated_work_and_conflicts_on_later_edits() {
    let server = server();
    let initial = script(
        &server,
        json!([{"draw":{"as":"worker","kind":"rectangle","text":"Worker"}}]),
    )
    .await;
    let worker = initial["created"]["worker"].as_str().unwrap();
    let experiment = script(
        &server,
        json!([{"update":{"id":worker,"text":"Experiment","x":320}}]),
    )
    .await;
    script(
        &server,
        json!([{"draw":{"kind":"sticky","text":"Unrelated"}}]),
    )
    .await;
    let result = server
        .canvas_revert(Parameters(RevertArgs {
            transaction: experiment["edit"]["transaction"].as_str().unwrap().into(),
            context: experiment["edit"]["context"].as_str().unwrap().into(),
        }))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let restored = read(&server, json!({})).await;
    assert!(text(&restored).contains("Worker"));
    assert!(text(&restored).contains("Unrelated"));
    assert!(!text(&restored).contains("Experiment"));
    let changed = script(&server, json!([{"update":{"id":worker,"text":"Trial"}}])).await;
    let workspace = server.current().await;
    let id = workspace.resolve_id(worker).await;
    let mut doc = workspace.doc.lock().await;
    let before = doc.clone();
    let label = canvas_core::edit::owned_label(&doc, &id).unwrap();
    let element = doc.element_mut(&label).unwrap();
    if let canvas_core::ElementKind::Text { text, .. } = &mut element.kind {
        *text = "Human preference".into();
    }
    element.touch(now_ms());
    workspace.record_change(&before, &doc, "page").await;
    drop(doc);
    let before = workspace.document_json().await;
    let result = server
        .canvas_revert(Parameters(RevertArgs {
            transaction: changed["edit"]["transaction"].as_str().unwrap().into(),
            context: workspace.context(),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("revert_conflict"));
    assert_eq!(workspace.document_json().await, before);
}

#[tokio::test]
async fn creation_revert_tombstones_and_rejects_new_dependencies() {
    let server = server();
    let initial = script(
        &server,
        json!([{"draw":{"as":"a","kind":"rectangle","text":"A"}}]),
    )
    .await;
    let b = script(
        &server,
        json!([{"draw":{"as":"b","kind":"rectangle","text":"B"}}]),
    )
    .await;
    let connection = script(
        &server,
        json!([{"connect":{"from":initial["created"]["a"],"to":b["created"]["b"]}}]),
    )
    .await;
    let failed = server
        .canvas_revert(Parameters(RevertArgs {
            transaction: initial["edit"]["transaction"].as_str().unwrap().into(),
            context: initial["edit"]["context"].as_str().unwrap().into(),
        }))
        .await
        .unwrap();
    assert_eq!(failed.is_error, Some(true));
    let reverted = server
        .canvas_revert(Parameters(RevertArgs {
            transaction: connection["edit"]["transaction"].as_str().unwrap().into(),
            context: connection["edit"]["context"].as_str().unwrap().into(),
        }))
        .await
        .unwrap();
    assert_ne!(reverted.is_error, Some(true));
    let created = script(
        &server,
        json!([{"draw":{"as":"temporary","kind":"sticky","text":"Temporary"}}]),
    )
    .await;
    let reverted = server
        .canvas_revert(Parameters(RevertArgs {
            transaction: created["edit"]["transaction"].as_str().unwrap().into(),
            context: created["edit"]["context"].as_str().unwrap().into(),
        }))
        .await
        .unwrap();
    assert_ne!(reverted.is_error, Some(true));
    assert!(!text(&read(&server, json!({})).await).contains("Temporary"));
}

#[tokio::test]
async fn reads_detect_gaps_and_paginate_only_an_unchanged_document() {
    let server = server();
    let created = script(
        &server,
        json!([{"draw":{"as":"a","kind":"sticky","text":"A"}}]),
    )
    .await;
    let first = status(&server).await;
    let page = read(&server, json!({"limit":1})).await;
    assert!(text(&page).contains("next offset=1"));
    let continued = read(
        &server,
        json!({"offset":1,"limit":1,"context":first["context"],"revision":first["revision"]}),
    )
    .await;
    assert_ne!(continued.is_error, Some(true));
    let invalid = read(&server, json!({"offset":1})).await;
    assert_eq!(invalid.is_error, Some(true));
    for index in 0..205 {
        script(
            &server,
            json!([{"update":{"id":created["created"]["a"],"text":format!("A {index}")}}]),
        )
        .await;
    }
    let gap = read(&server, json!({"since":1})).await;
    assert_eq!(gap.is_error, Some(true));
    assert!(text(&gap).contains("resync_required"));
    let changed = read(
        &server,
        json!({"offset":1,"context":first["context"],"revision":first["revision"]}),
    )
    .await;
    assert_eq!(changed.is_error, Some(true));
    let query = read(&server, json!({"query":"a 204"})).await;
    assert!(text(&query).contains("A 204"));
}

#[tokio::test]
async fn discussion_persists_per_workspace_and_rejects_stale_replacement() {
    let dir = std::env::temp_dir().join(format!(
        "rs-canvas-discussion-{}",
        canvas_core::id::new_element_id()
    ));
    let path = dir.join("current.json");
    let server = CanvasServer::new(Arc::new(AppState::new(
        std::path::PathBuf::from("."),
        Some(path.clone()),
        true,
    )));
    let first = status(&server).await;
    let args = || RememberArgs {
        context: first["context"].as_str().unwrap().into(),
        revision: 0,
        notes: crate::state::Discussion {
            constraints: vec!["At-least-once delivery".into()],
            decisions: vec!["Ack after processing to avoid lost jobs".into()],
            questions: vec!["How do we deduplicate?".into()],
        },
    };
    let saved = server.canvas_remember(Parameters(args())).await.unwrap();
    assert_ne!(saved.is_error, Some(true), "{}", text(&saved));
    assert_eq!(
        server
            .canvas_remember(Parameters(args()))
            .await
            .unwrap()
            .is_error,
        Some(true)
    );
    let restarted = CanvasServer::new(Arc::new(AppState::new(
        std::path::PathBuf::from("."),
        Some(path),
        true,
    )));
    let restored = status(&restarted).await;
    assert_eq!(
        restored["discussion"]["notes"]["constraints"][0],
        "At-least-once delivery"
    );
    assert_ne!(restored["context"], first["context"]);
    assert_eq!(
        restarted
            .state
            .workspace("other")
            .await
            .discussion
            .lock()
            .await
            .revision,
        0
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
}
