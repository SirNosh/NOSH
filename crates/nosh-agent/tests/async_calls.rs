//! Ports the `AsyncToolCalls` cases of `packages/pi-adapter/src/async-calls.test.ts` (the real Pi
//! loop case waits for the sidecar, P5d).

use nosh_agent::async_calls::{AsyncOutcome, AsyncToolCalls};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

fn calls(grace_ms: u64) -> Arc<AsyncToolCalls> {
    Arc::new(AsyncToolCalls::new(Duration::from_millis(grace_ms)))
}

fn stringify(value: &Value) -> String {
    nosh_wire::canonical_json(value)
}

/// `String(value)` for the string results these tests deliver.
fn string(value: &Value) -> String {
    nosh_wire::js::string(Some(value))
}

/// A call that settles when the sender is used.
fn pending() -> (
    oneshot::Sender<Result<Value, String>>,
    impl Future<Output = Result<Value, String>> + Send + 'static,
) {
    let (finish, finished) = oneshot::channel();
    (finish, async move {
        finished.await.unwrap_or_else(|_| Err("dropped".into()))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_a_call_that_settles_within_the_grace_period_as_an_ordinary_result() {
    let calls = calls(200);
    let outcome = calls
        .start(
            "call_1",
            "nosh_run command_test",
            async { Ok(json!({ "exitCode": 0 })) },
            string,
        )
        .await;
    assert_eq!(outcome, AsyncOutcome::Settled(json!({ "exitCode": 0 })));
    assert!(calls.idle());
    assert_eq!(calls.next(Duration::from_secs(600)).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn answers_a_slow_call_with_a_placeholder_then_wakes_once_with_every_result_that_landed_together()
 {
    let calls = calls(10);
    let (finish_run, run) = pending();
    let (finish_read, read) = pending();
    let run = calls
        .start(
            "call_run",
            "nosh_run command_test",
            calls.track_run(run),
            stringify,
        )
        .await;
    let read = calls
        .start(
            "call_read",
            "nosh_network_read https://api.crossref.org/x",
            read,
            stringify,
        )
        .await;
    let AsyncOutcome::Running { placeholder } = run else {
        panic!("{run:?}")
    };
    assert!(placeholder.contains("nosh_run command_test is still running"));
    assert!(matches!(read, AsyncOutcome::Running { .. }));
    assert!(calls.worktree_busy());
    let running = calls.running();
    assert_eq!(running.len(), 2);
    assert!(
        running[0].starts_with("nosh_run command_test (call call_run"),
        "{running:?}"
    );
    assert!(running[1].contains("nosh_network_read"), "{running:?}");
    let woken = {
        let calls = Arc::clone(&calls);
        tokio::spawn(async move { calls.next(Duration::from_secs(60)).await })
    };
    tokio::task::yield_now().await;
    finish_run.send(Ok(json!({ "exitCode": 0 }))).unwrap();
    finish_read.send(Ok(json!({ "status": 200 }))).unwrap();
    let message = woken.await.unwrap().unwrap();
    assert!(
        message
            .contains("Async result of nosh_run command_test (call call_run):\n{\"exitCode\":0}"),
        "{message}"
    );
    assert!(
        message.contains("Async result of nosh_network_read"),
        "{message}"
    );
    assert!(!calls.worktree_busy());
    assert!(calls.idle());
}

#[tokio::test(flavor = "multi_thread")]
async fn counts_a_run_as_testing_the_worktree_from_its_first_moment_and_releases_it_even_when_the_run_fails()
 {
    let calls = calls(60_000);
    let (fail_run, failing) = pending();
    let (finish_run, passing) = pending();
    let failing = tokio::spawn(calls.track_run(failing));
    let passing = tokio::spawn(calls.track_run(passing));
    // Before any grace period: the old label check only saw runs that had already returned a placeholder.
    assert!(calls.worktree_busy());
    fail_run.send(Err("job failed".into())).unwrap();
    assert_eq!(failing.await.unwrap(), Err("job failed".into()));
    assert!(calls.worktree_busy());
    finish_run.send(Ok(json!({ "exitCode": 0 }))).unwrap();
    passing.await.unwrap().unwrap();
    assert!(!calls.worktree_busy());
    // A run dropped before it settles (its session stopped) releases the worktree too.
    let (_never, never) = pending();
    drop(calls.track_run(never));
    assert!(!calls.worktree_busy());
}

#[test]
fn aborts_running_programs_on_cancel_and_gives_later_programs_a_fresh_signal() {
    let calls = AsyncToolCalls::default();
    let first = calls.signal();
    calls.cancel();
    assert!(first.aborted());
    assert!(!calls.signal().aborted());
}

#[tokio::test(flavor = "multi_thread")]
async fn steers_a_result_into_a_live_run_requeues_one_that_missed_the_run_and_reports_a_heartbeat()
{
    let calls = calls(5);
    let steered = Arc::new(Mutex::new(Vec::<String>::new()));
    let live = Arc::new(Mutex::new(true));
    {
        let (steered, live) = (Arc::clone(&steered), Arc::clone(&live));
        calls.set_deliver(move |text| {
            let delivered = *live.lock().unwrap();
            if delivered {
                steered.lock().unwrap().push(text);
            }
            Box::pin(async move { delivered })
        });
    }
    let (finish, work) = pending();
    calls
        .start("call_a", "nosh_run command_test", work, string)
        .await;
    let heartbeat = calls.next(Duration::from_millis(20)).await.unwrap();
    assert!(
        heartbeat.contains(
            "Heartbeat: waited 0 seconds. Still running: nosh_run command_test (call call_a"
        ),
        "{heartbeat}"
    );
    finish.send(Ok(json!("done"))).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let delivered = steered.lock().unwrap().clone();
    assert_eq!(
        delivered,
        vec!["Async result of nosh_run command_test (call call_a):\ndone".to_owned()]
    );
    *live.lock().unwrap() = false;
    calls.requeue(&delivered);
    assert_eq!(
        calls.next(Duration::from_secs(600)).await,
        Some(delivered[0].clone())
    );
    // A failed call is a result too.
    let failed = calls
        .start(
            "call_b",
            "nosh_run command_test",
            async { Err("boom".to_owned()) },
            stringify,
        )
        .await;
    assert_eq!(
        failed,
        AsyncOutcome::Settled(json!({ "accepted": false, "error": "boom" }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wakes_a_sleeping_loop_on_interrupt_and_keeps_requeued_results_in_order() {
    let calls = calls(5);
    let (_finish, work) = pending();
    calls
        .start("call_a", "nosh_run command_test", work, string)
        .await;
    let sleeping = {
        let calls = Arc::clone(&calls);
        tokio::spawn(async move { calls.next(Duration::from_secs(600)).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    calls.interrupt();
    // Woken with nothing queued: an empty message, which the wake loop skips.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), sleeping)
            .await
            .unwrap()
            .unwrap(),
        Some(String::new())
    );
    calls.requeue(&["second".to_owned()]);
    calls.requeue(&["first a".to_owned(), "first b".to_owned()]);
    assert_eq!(
        calls.next(Duration::from_secs(600)).await.as_deref(),
        Some("first a\n\nfirst b\n\nsecond")
    );
}

/// Rust-only: an abort that lands while the wake loop is between sleeps (on its Pi round-trip)
/// must still end the loop's next sleep, not leave it waiting for a result.
#[tokio::test(flavor = "multi_thread")]
async fn an_abort_before_the_loop_sleeps_still_ends_its_next_sleep() {
    let calls = calls(5);
    let (_finish, work) = pending();
    calls
        .start("call_a", "nosh_run command_test", work, string)
        .await;
    calls.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), calls.next(Duration::from_secs(600)))
            .await
            .unwrap(),
        Some(String::new())
    );
    // The interrupt is consumed: the following sleep waits again.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            calls.next(Duration::from_secs(600))
        )
        .await
        .is_err()
    );
}
