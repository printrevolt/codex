use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_pr_runtime::probe;
use codex_pr_runtime::probe::LifecycleEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_touchpoints_fire_in_expected_order() -> anyhow::Result<()> {
    probe::reset();

    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .build(&server)
        .await?
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let events = probe::take();
    assert!(
        events.len() >= 4,
        "expected at least 4 lifecycle events, got {events:?}"
    );

    assert!(matches!(events[0], LifecycleEvent::SessionStart { .. }));
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, LifecycleEvent::BeforeTask { .. }))
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, LifecycleEvent::BeforeFinalize { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(LifecycleEvent::SessionEnd { .. })
    ));

    Ok(())
}
