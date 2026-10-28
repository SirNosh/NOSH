//! Ports `packages/pi-adapter/src/code-mode.test.ts`, plus the result formats TypeScript's
//! `runProgram` produces for thrown values, logs, and serialization failures.

use nosh_agent::async_calls::AbortSignal;
use nosh_agent::code_mode::{CODE_LIMITS, CodeLimits, ProgramHost, ProgramResult, run_program};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Each call takes 200 ms and reports when it finished.
fn echo() -> ProgramHost {
    Arc::new(|name, args, _index| {
        let name = name.to_owned();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(json!({ "name": name, "args": args, "at": now_ms() }))
        })
    })
}

async fn run(
    code: &str,
    functions: &[&str],
    host: ProgramHost,
    limits: CodeLimits,
) -> ProgramResult {
    run_program(code, functions, host, &AbortSignal::new(), limits).await
}

fn ok(result: &ProgramResult, value: Value) {
    assert_eq!(
        (result.ok, result.value.as_ref(), result.error.as_deref()),
        (true, Some(&value), None),
        "{result:?}"
    );
}

fn error(result: &ProgramResult) -> &str {
    assert!(!result.ok, "{result:?}");
    result.error.as_deref().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn reaches_no_host_globals_beyond_the_tool_functions() {
    let result = run(
        r#"return [typeof require, typeof process, typeof fetch, typeof setTimeout, typeof globalThis.__call, typeof globalThis.__log, Object.keys(tools),
      (() => { try { return typeof (function () {}).constructor("return process")(); } catch { return "threw"; } })(),
      (() => { try { return typeof (async () => {}).constructor("return import('node:fs')")(); } catch { return "threw"; } })()];"#,
        &["read"],
        echo(),
        CODE_LIMITS,
    )
    .await;
    // Reaching for process through the Function constructor throws: the name does not exist in the guest.
    ok(
        &result,
        json!([
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            ["read"],
            "threw",
            "object"
        ]),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_dynamic_import_of_any_module() {
    let result = run(r#"try { await import("node:fs"); return "imported"; } catch (error) { return "refused"; }"#, &[], echo(), CODE_LIMITS).await;
    ok(&result, json!("refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_independent_tool_calls_concurrently_and_chains_dependent_ones() {
    // Each host call takes 200 ms; the two parallel calls finish within 100 ms of each other only if they ran together.
    let result = run(
        r#"const [a, b] = await Promise.all([tools.read("a.txt"), tools.read("b.txt")]); const c = await tools.read(a.args[0] + "+" + b.args[0]); return [c.args[0], Math.abs(a.at - b.at) < 100];"#,
        &["read"],
        echo(),
        CODE_LIMITS,
    )
    .await;
    ok(&result, json!(["a.txt+b.txt", true]));
    assert_eq!(result.calls, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_a_failed_tool_call_to_the_program_as_an_error_it_can_catch() {
    let host: ProgramHost = Arc::new(|_, _, _| {
        Box::pin(async { Err("Workspace path escapes the assigned root".to_owned()) })
    });
    let result = run(r#"try { await tools.read("../x"); } catch (error) { return error instanceof Error ? error.message : "not an Error"; }"#, &["read"], host, CODE_LIMITS).await;
    ok(&result, json!("Workspace path escapes the assigned root"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stops_a_busy_loop_at_the_cpu_limit() {
    let result = run(
        "while (true) {}",
        &[],
        echo(),
        CodeLimits {
            cpu_ms: 400,
            ..CODE_LIMITS
        },
    )
    .await;
    assert!(
        error(&result).to_lowercase().contains("interrupted"),
        "{result:?}"
    );
    // A loop reached after an await is interrupted inside a pending job.
    let result = run(
        "await null; while (true) {}",
        &[],
        echo(),
        CodeLimits {
            cpu_ms: 400,
            ..CODE_LIMITS
        },
    )
    .await;
    assert_eq!(error(&result), "InternalError: interrupted");
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_count_time_spent_waiting_on_tools_against_the_cpu_limit() {
    let result = run(
        r#"for (let i = 0; i < 4; i++) await tools.read(String(i)); return "done";"#,
        &["read"],
        echo(),
        CodeLimits {
            cpu_ms: 300,
            ..CODE_LIMITS
        },
    )
    .await;
    ok(&result, json!("done"));
    assert_eq!(result.calls, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn enforces_the_memory_limit() {
    let result = run(
        "const chunks = []; while (true) chunks.push(new ArrayBuffer(4 << 20));",
        &[],
        echo(),
        CodeLimits {
            memory_bytes: 16 * 1024 * 1024,
            ..CODE_LIMITS
        },
    )
    .await;
    assert!(
        error(&result).to_lowercase().contains("memory"),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_tool_calls_beyond_the_per_program_cap() {
    let host: ProgramHost = Arc::new(|_, _, _| Box::pin(async { Ok(json!("x")) }));
    let result = run(
        r#"const outcomes = []; for (let i = 0; i < 4; i++) { try { await tools.read(String(i)); outcomes.push("ok"); } catch (error) { outcomes.push(error.message); } } return outcomes;"#,
        &["read"],
        host,
        CodeLimits { max_calls: 3, ..CODE_LIMITS },
    )
    .await;
    ok(
        &result,
        json!(["ok", "ok", "ok", "Program exceeded 3 tool calls"]),
    );
    assert_eq!(result.calls, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_thrown_errors_syntax_errors_and_programs_that_can_never_finish() {
    assert_eq!(
        error(
            &run(
                r#"throw new TypeError("bad input");"#,
                &[],
                echo(),
                CODE_LIMITS
            )
            .await
        ),
        "TypeError: bad input"
    );
    assert!(error(&run("return (;", &[], echo(), CODE_LIMITS).await).starts_with("SyntaxError: "));
    assert_eq!(
        error(&run("await new Promise(() => {});", &[], echo(), CODE_LIMITS).await),
        "The program is waiting on something that can never finish (no tool call is outstanding)."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn describes_thrown_values_as_typescript_does() {
    let cases = [
        (r#"throw "plain""#, "plain"),
        ("throw 5", "5"),
        ("throw null", "null"),
        ("throw undefined", "undefined"),
        (r#"throw { message: "m" }"#, "Error: m"),
        (r#"throw { name: "Custom", message: "m" }"#, "Custom: m"),
        ("throw [1, 2]", "1,2"),
        ("throw { a: 1 }", "[object Object]"),
    ];
    for (code, expected) in cases {
        assert_eq!(
            error(&run(code, &[], echo(), CODE_LIMITS).await),
            expected,
            "{code}"
        );
    }
    // The engine writes these messages; quickjs-ng's wording differs from Bellard QuickJS's
    // (TypeScript: "Do not know how to serialize a BigInt", "circular reference", "not a function").
    let engine = [
        ("return 1n", "TypeError: "),
        ("const a = {}; a.a = a; return a", "TypeError: "),
        ("return tools.nope()", "TypeError: "),
        ("return foo.bar", "ReferenceError: "),
    ];
    for (code, name) in engine {
        let result = run(code, &[], echo(), CODE_LIMITS).await;
        assert!(error(&result).starts_with(name), "{code}: {result:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serializes_the_return_value_as_json() {
    ok(
        &run("return undefined", &[], echo(), CODE_LIMITS).await,
        Value::Null,
    );
    ok(
        &run("return { toJSON: () => 7 }", &[], echo(), CODE_LIMITS).await,
        json!(7),
    );
    ok(
        &run("return [1.5, 2.0, -0, 1e21, NaN]", &[], echo(), CODE_LIMITS).await,
        json!([1.5, 2, 0, 1e21, null]),
    );
    // TypeScript's daemon crashed here (JSON.parse("undefined")); the port returns null.
    ok(
        &run("return () => 1", &[], echo(), CODE_LIMITS).await,
        Value::Null,
    );
    ok(
        &run("return Symbol()", &[], echo(), CODE_LIMITS).await,
        Value::Null,
    );
    // A lone surrogate, which Rust strings cannot hold, becomes U+FFFD.
    ok(
        &run(
            r#"return ["\ud83d", "😀".slice(0, 1) + "x", "😀"]"#,
            &[],
            echo(),
            CODE_LIMITS,
        )
        .await,
        json!(["\u{FFFD}", "\u{FFFD}x", "😀"]),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_tool_arguments_and_results_as_json() {
    let host: ProgramHost = Arc::new(|name, args, index| {
        let name = name.to_owned();
        Box::pin(async move {
            if name == "write" {
                Err("no such file".to_owned())
            } else {
                Ok(json!({ "args": args, "index": index }))
            }
        })
    });
    let result = run(
        r#"const read = await tools.read("a", { n: 1.0 }, [null]); let failed; try { await tools.write("x"); } catch (error) { failed = String(error); } return { read, failed };"#,
        &["read", "write"],
        host,
        CODE_LIMITS,
    )
    .await;
    ok(
        &result,
        json!({ "read": { "args": ["a", { "n": 1 }, [null]], "index": 0 }, "failed": "Error: no such file" }),
    );
    assert_eq!(result.calls, 2);
    let failed = run(
        r#"await tools.write("x")"#,
        &["write"],
        run_host_failure(),
        CODE_LIMITS,
    )
    .await;
    assert_eq!((error(&failed), failed.calls), ("Error: no such file", 1));
}

fn run_host_failure() -> ProgramHost {
    Arc::new(|_, _, _| Box::pin(async { Err("no such file".to_owned()) }))
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_console_log_output_within_the_log_budget_and_returns_null_for_no_value() {
    let result = run(
        r#"console.log("files:", [1, 2]); console.log("x".repeat(50)); console.log("dropped");"#,
        &[],
        echo(),
        CodeLimits {
            max_log_chars: 30,
            ..CODE_LIMITS
        },
    )
    .await;
    ok(&result, Value::Null);
    assert_eq!(result.logs.concat().chars().count(), 30);
    assert_eq!(result.logs[0], "files: [1,2]");
    // Output past the budget is dropped, not logged as empty lines.
    assert_eq!(result.logs.len(), 2);
    let joined = run(
        r#"console.log(1, "two", { a: [3] }, undefined, null)"#,
        &[],
        echo(),
        CODE_LIMITS,
    )
    .await;
    assert_eq!(joined.logs, vec![r#"1 two {"a":[3]}  null"#]);
}

#[tokio::test(flavor = "multi_thread")]
async fn stops_at_once_when_cancelled_even_while_spinning_or_waiting() {
    for code in ["while (true) {}", r#"await tools.read("slow")"#] {
        let signal = AbortSignal::new();
        let aborter = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            aborter.abort();
        });
        let slow: ProgramHost = Arc::new(|_, _, _| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(Value::Null)
            })
        });
        let started = Instant::now();
        let result = run_program(
            code,
            &["read"],
            slow,
            &signal,
            CodeLimits {
                cpu_ms: 60_000,
                ..CODE_LIMITS
            },
        )
        .await;
        assert_eq!(error(&result), "Program cancelled", "{code}");
        assert!(started.elapsed() < Duration::from_secs(1), "{code}");
        assert_eq!(
            error(&run_program("return 1;", &[], echo(), &signal, CODE_LIMITS).await),
            "Program cancelled"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_programs_over_the_size_limit_before_starting() {
    assert_eq!(
        error(&run(&" ".repeat(20_001), &[], echo(), CODE_LIMITS).await),
        "Program exceeds 20000 characters"
    );
    // The limit counts UTF-16 code units, as JavaScript's length does.
    let astral = format!("return 1;{}", "😀".repeat(9_996));
    assert_eq!(
        error(&run(&astral, &[], echo(), CODE_LIMITS).await),
        "Program exceeds 20000 characters"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_each_call_its_position_in_program_order() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let host: ProgramHost = {
        let seen = Arc::clone(&seen);
        Arc::new(move |_, args, index| {
            seen.lock().unwrap().push((index, args[0].clone()));
            Box::pin(async { Ok(Value::Null) })
        })
    };
    let result = run(r#"await Promise.all(["a", "b", "c"].map((path) => tools.read(path))); await tools.read("d");"#, &["read"], host, CODE_LIMITS).await;
    ok(&result, Value::Null);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (0, json!("a")),
            (1, json!("b")),
            (2, json!("c")),
            (3, json!("d"))
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accumulates_computation_across_the_slices_between_tool_calls() {
    let host: ProgramHost = Arc::new(|_, _, _| Box::pin(async { Ok(Value::Null) }));
    // Each slice spins 150 ms, under the 400 ms limit; together they exceed it.
    let result = run(
        r#"for (let i = 0; i < 10; i++) { const until = Date.now() + 150; while (Date.now() < until) {} await tools.read(String(i)); } return "finished";"#,
        &["read"],
        host,
        CodeLimits { cpu_ms: 400, ..CODE_LIMITS },
    )
    .await;
    assert_eq!(error(&result), "InternalError: interrupted");
    assert!((2..=3).contains(&result.calls), "{result:?}");
}
