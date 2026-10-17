//! Process identity and control, as in `packages/jobs/src/index.ts`.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

/// `CREATE_NO_WINDOW`: helper programs never open a console window (Node's `windowsHide`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Node's `detached: true` on Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`.
#[cfg(windows)]
const DETACHED: u32 = 0x0000_0008 | 0x0000_0200;

/// A command that runs without a visible window.
pub fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
    command
}

/// Makes `command` start detached from this process, so it outlives the daemon.
pub fn detach(command: &mut Command) {
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(command, DETACHED);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
}

/// A process's identity: its start time, so a reused PID is never mistaken for the same process.
/// On Windows this runs the exact PowerShell query the TypeScript daemon runs, so records written
/// by either daemon compare equal.
pub fn fingerprint(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        let query = format!(
            "(Get-Process -Id {pid} -ErrorAction Stop).StartTime.ToUniversalTime().ToString('O')"
        );
        let output = hidden("powershell.exe")
            .args(["-NoProfile", "-Command", &query])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    #[cfg(not(windows))]
    {
        // Field 22 of /proc/<pid>/stat is the start time in clock ticks.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.split(' ').nth(21).map(str::to_owned)
    }
}

/// Whether the process recorded as `pid` with `expected` fingerprint is still running.
pub fn alive(pid: Option<u32>, expected: Option<&str>) -> bool {
    match (pid, expected) {
        (Some(pid), Some(expected)) => fingerprint(pid).as_deref() == Some(expected),
        _ => false,
    }
}

/// Kills a process and its descendants, then waits (at most 5 s) until its identity is gone.
pub fn kill_tree(pid: u32, fingerprint_now: Option<&str>) {
    #[cfg(windows)]
    {
        let _ = hidden("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        for _ in 0..50 {
            if !alive(Some(pid), fingerprint_now) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, fingerprint_now);
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .status();
    }
}

/// Runs `command` to completion with captured output, killing it after `timeout`
/// (`spawnSync` with `timeout`). A timed-out run reports `None` for its status.
pub fn output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<(Option<ExitStatus>, Output)> {
    run_with_input(command, None, timeout)
}

/// [`output_with_timeout`] with `input` written to the command's stdin (`spawnSync`'s `input`).
pub fn run_with_input(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> io::Result<(Option<ExitStatus>, Output)> {
    let stdin = if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let writer = input.zip(child.stdin.take()).map(|(bytes, mut pipe)| {
        std::thread::spawn(move || {
            use std::io::Write;
            // A command that exits without reading its input closes the pipe; that is its answer.
            let _ = pipe.write_all(&bytes);
        })
    });
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = wait_until(&mut child, Instant::now() + timeout)?;
    if status.is_none() {
        // The whole tree: a surviving grandchild would keep the output pipes open.
        kill_tree(child.id(), None);
        let _ = child.kill();
    }
    let final_status = child.wait()?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let output = Output {
        status: final_status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    };
    Ok((status, output))
}

/// Polls `child` until it exits or `deadline` passes.
pub fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Node reports Windows exit codes as unsigned 32-bit numbers (a crash is 3221225477, not -1073741819).
pub fn node_exit_code(code: i32) -> i64 {
    if cfg!(windows) {
        i64::from(code.cast_unsigned())
    } else {
        i64::from(code)
    }
}

/// The message Node gives a failed `spawn(file)`, which TypeScript records as a Job's failure reason.
pub fn spawn_error_message(file: &str, error: &io::Error) -> String {
    let code = match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        _ => "UNKNOWN",
    };
    format!("spawn {file} {code}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_identify_a_live_process_until_it_exits() {
        let mut child = hidden("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        let identity = fingerprint(child.id()).expect("a running process has a fingerprint");
        assert!(
            identity.ends_with('Z') && identity.contains('T'),
            "{identity}"
        );
        assert!(alive(Some(child.id()), Some(&identity)));
        assert!(!alive(
            Some(child.id()),
            Some("2000-01-01T00:00:00.0000000Z")
        ));
        kill_tree(child.id(), Some(&identity));
        assert!(!alive(Some(child.id()), Some(&identity)));
        let _ = child.wait();
        assert!(!alive(None, Some(&identity)) && !alive(Some(1), None));
    }

    #[test]
    fn output_with_timeout_kills_slow_commands() {
        let (status, output) = output_with_timeout(
            hidden("cmd").args(["/c", "echo hi"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(status.unwrap().success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
        let started = Instant::now();
        let (status, _) = output_with_timeout(
            hidden("cmd").args(["/c", "ping -n 30 127.0.0.1 >NUL"]),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(status.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn spawn_errors_read_like_node() {
        let error = Command::new("nosh-definitely-missing-program")
            .spawn()
            .unwrap_err();
        assert_eq!(
            spawn_error_message("nosh-definitely-missing-program", &error),
            "spawn nosh-definitely-missing-program ENOENT"
        );
    }
}
