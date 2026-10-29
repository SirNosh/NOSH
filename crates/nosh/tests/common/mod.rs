//! Helpers shared by the CLI test files.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// `command.output()`, given 60 s (cli.md §12) until the command exits and its output pipes close:
/// a process it started that still holds them, as a daemon given inherited handles would, fails
/// the run too. A failure names the command and what it printed so far.
pub fn output(command: &mut Command) -> Result<Output, String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{command:?}: {error}"))?;
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let status = nosh_sys::process::wait_until(&mut child, deadline).unwrap();
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let (stdout, stdout_closed) = collect(&stdout, deadline);
    let (stderr, stderr_closed) = collect(&stderr, deadline);
    match status {
        Some(status) if stdout_closed && stderr_closed => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        _ => Err(format!(
            "{command:?} did not finish within 60 s (exit status {status:?})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )),
    }
}

/// What `pipe` yields, as chunks sent while it is read.
fn drain(mut pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (send, chunks) = channel();
    std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(read @ 1..) = pipe.read(&mut buffer) {
            if send.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    chunks
}

/// The chunks received by `deadline`, and whether the pipe closed by then.
fn collect(chunks: &Receiver<Vec<u8>>, deadline: Instant) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    loop {
        match chunks.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => bytes.extend(chunk),
            Err(RecvTimeoutError::Disconnected) => return (bytes, true),
            Err(RecvTimeoutError::Timeout) => return (bytes, false),
        }
    }
}
