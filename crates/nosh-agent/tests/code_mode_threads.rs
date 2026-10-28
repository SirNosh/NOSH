//! A cancelled nosh_code program's interpreter thread stops, rather than spinning to its CPU limit
//! after the program was answered. Alone in its own test binary, because the thread count is
//! process-wide.

use nosh_agent::async_calls::AbortSignal;
use nosh_agent::code_mode::{
    CODE_LIMITS, CodeLimits, ProgramHost, live_guest_threads, run_program,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_programs_stop_their_interpreter_thread() {
    let slow: ProgramHost = Arc::new(|_, _, _| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(Value::Null)
        })
    });
    for code in [
        "while (true) {}",
        "await null; while (true) {}",
        r#"await tools.read("slow")"#,
    ] {
        let signal = AbortSignal::new();
        let aborter = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            aborter.abort();
        });
        let result = run_program(
            code,
            &["read"],
            Arc::clone(&slow),
            &signal,
            CodeLimits {
                cpu_ms: 60_000,
                ..CODE_LIMITS
            },
        )
        .await;
        assert_eq!(result.error.as_deref(), Some("Program cancelled"), "{code}");
        let answered = Instant::now();
        while live_guest_threads() > 0 {
            assert!(
                answered.elapsed() < Duration::from_secs(1),
                "{code}: the interpreter thread kept running"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
