//! Runs a native Job command and records how it ended, so a supervisor restarted while the Job ran
//! can still report its result. Replaces `packages/jobs/src/exit-wrapper.ts` and writes the same
//! `exit.json`, so either daemon can recover the other's Jobs.
//!
//! Usage: `nosh job-wrapper <exitFile> -- <command> [args...]`. The command inherits this
//! process's stdio (the Job's log files), so the wrapper never writes to stdout or stderr itself.
//! It stays in the Job's process tree: cancelling the Job kills both. The exit record is complete
//! before the wrapper exits, so "wrapper gone" implies "record present" unless it was killed.

/// Runs the wrapper on its arguments (after the program name); returns its exit code.
pub fn run(args: &[String]) -> i32 {
    let (Some(exit_file), Some("--"), Some(file)) =
        (args.first(), args.get(1).map(String::as_str), args.get(2))
    else {
        return 2;
    };
    let spawned = crate::process::hidden(file).args(&args[3..]).spawn();
    match spawned.and_then(|mut child| child.wait()) {
        Ok(status) => {
            let code = status.code();
            record(exit_file, code.map(crate::process::node_exit_code), None);
            code.unwrap_or(1)
        }
        Err(error) => {
            record(
                exit_file,
                None,
                Some(crate::process::spawn_error_message(file, &error)),
            );
            1
        }
    }
}

/// `{"exitCode":…,"signal":null,"finishedAt":…[,"error":…]}` in `JSON.stringify` order.
fn record(exit_file: &str, exit_code: Option<i64>, error: Option<String>) {
    let mut text = format!(
        "{{\"exitCode\":{},\"signal\":null,\"finishedAt\":{}",
        exit_code.map_or_else(|| "null".to_owned(), |c| c.to_string()),
        serde_json::json!(nosh_wire::now_utc())
    );
    if let Some(error) = error {
        text.push_str(&format!(",\"error\":{}", serde_json::json!(error)));
    }
    text.push_str("}\n");
    let temporary = format!("{exit_file}.tmp");
    // Nothing useful can be reported if the record cannot be written; the supervisor then marks the Job lost.
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, exit_file);
    }
}
