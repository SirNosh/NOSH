//! The Job supervisor, as `JobSupervisor` in `packages/jobs/src/index.ts`.
//!
//! Jobs are detached processes recorded in `<data>/jobs/<jobId>/job.json` (canonical JSON). Native
//! Jobs run under an exit wrapper that writes `exit.json`; a process's identity is its start-time
//! fingerprint. Both files and the fingerprint are shared with the TypeScript daemon, so either
//! daemon recovers, reports, and cancels Jobs the other started.

use crate::paths::resolve;
use crate::process::{
    self, alive, child_fingerprint, detach, hidden, node_exit_code, spawn_error_message,
};
use crate::{Result, rejected};
use nosh_wire::{canonical_json, now_utc, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Starting,
    Running,
    Checkpointing,
    Finishing,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Failed | JobState::Cancelled | JobState::Lost
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    Native,
    Wsl2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSpec {
    pub job_id: String,
    pub project_id: String,
    pub mission_id: Option<String>,
    pub direction_id: Option<String>,
    pub autoresearch_id: Option<String>,
    pub run_id: String,
    pub experiment_id: Option<String>,
    pub commit_sha: String,
    pub working_directory: String,
    pub runner: Runner,
    pub distribution: Option<String>,
    pub command: Vec<String>,
    pub checkpoint_command: Option<Vec<String>>,
    pub environment_lock_hash: String,
    pub evaluation_contract_hash: String,
    pub timeout_seconds: f64,
    pub uses_gpu: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    #[serde(flatten)]
    pub spec: JobSpec,
    pub state: JobState,
    pub command_digest: String,
    pub windows_pid: Option<u32>,
    pub process_fingerprint: Option<String>,
    pub wsl_pid_file: Option<String>,
    pub stdout_path: String,
    pub stderr_path: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub failure_reason: Option<String>,
    pub recovered_after_restart: bool,
    /// Fields a newer NOSH wrote, kept when the record is rewritten.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// How a Job's command ended (`exit.json`); `error` means it could not be started at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExitOutcome {
    exit_code: Option<i64>,
    signal: Option<String>,
    finished_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub elapsed_seconds: u64,
    pub resident_bytes: Option<f64>,
    pub gpu: Option<Value>,
    pub output_stalled_seconds: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Stdout,
    Stderr,
}

type OnChange = Box<dyn Fn(&JobRecord) + Send + Sync>;

pub struct JobSupervisor {
    inner: Arc<Inner>,
}

struct Inner {
    data_directory: PathBuf,
    /// The exit-wrapper program and any leading arguments, before `<exitFile> -- <command>`.
    wrapper: Vec<OsString>,
    on_change: OnChange,
    /// Serialises every read-modify-write of a Job record.
    records: Mutex<()>,
    closed: AtomicBool,
    telemetry: Mutex<HashMap<String, (Instant, ResourceSnapshot)>>,
    gpu: Mutex<Option<(Instant, Option<Value>)>>,
}

impl JobSupervisor {
    /// `wrapper` is the exit-wrapper command (e.g. the `nosh-job-wrapper` binary).
    pub fn new(
        data_directory: &Path,
        wrapper: Vec<OsString>,
        on_change: impl Fn(&JobRecord) + Send + Sync + 'static,
    ) -> Result<Self> {
        std::fs::create_dir_all(data_directory.join("jobs"))?;
        assert!(!wrapper.is_empty(), "the exit wrapper needs a program");
        let inner = Inner {
            data_directory: data_directory.to_path_buf(),
            wrapper,
            on_change: Box::new(on_change),
            records: Mutex::new(()),
            closed: AtomicBool::new(false),
            telemetry: Mutex::default(),
            gpu: Mutex::default(),
        };
        Ok(JobSupervisor {
            inner: Arc::new(inner),
        })
    }

    /// Starts a Job; starting the same Job ID again with the same spec returns its record.
    pub fn start(&self, spec: &JobSpec) -> Result<JobRecord> {
        validate_spec(spec)?;
        let inner = &self.inner;
        let mut spec = spec.clone();
        spec.working_directory = resolve(Path::new(&spec.working_directory))?
            .to_string_lossy()
            .into_owned();
        let guard = inner.lock();
        if inner.record_path(&spec.job_id).exists() {
            let prior = inner.get(&spec.job_id)?;
            if canonical_json(&serde_json::to_value(&prior.spec)?)
                != canonical_json(&serde_json::to_value(&spec)?)
            {
                return rejected(format!(
                    "Job {} already exists with a different specification",
                    spec.job_id
                ));
            }
            return Ok(prior);
        }
        let directory = inner.data_directory.join("jobs").join(&spec.job_id);
        std::fs::create_dir_all(&directory)?;
        let mut record = JobRecord {
            command_digest: sha256(&json!(spec.command)),
            wsl_pid_file: (spec.runner == Runner::Wsl2)
                .then(|| format!("/tmp/nosh-{}.pid", spec.job_id)),
            stdout_path: directory.join("stdout.log").to_string_lossy().into_owned(),
            stderr_path: directory.join("stderr.log").to_string_lossy().into_owned(),
            spec,
            state: JobState::Starting,
            windows_pid: None,
            process_fingerprint: None,
            started_at: Some(now_utc()),
            finished_at: None,
            exit_code: None,
            failure_reason: None,
            recovered_after_restart: false,
            extra: Map::new(),
        };
        inner.save(&record)?;
        let spawned = (|| -> std::io::Result<Child> {
            let stdout = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&record.stdout_path)?;
            let stderr = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&record.stderr_path)?;
            let (file, mut command) = inner.command_for(&record);
            command
                .current_dir(&record.spec.working_directory)
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr);
            detach(&mut command);
            command.spawn().map_err(|error| {
                std::io::Error::new(error.kind(), spawn_error_message(&file, &error))
            })
        })();
        let child = match spawned {
            Ok(child) => child,
            Err(error) => return inner.launch_failed(record, &error.to_string()),
        };
        record.windows_pid = Some(child.id());
        record.process_fingerprint = child_fingerprint(&child);
        record.state = JobState::Running;
        inner.save(&record)?;
        drop(guard);
        let watched = Arc::clone(inner);
        let (job_id, deadline) = (record.spec.job_id.clone(), deadline_of(&record));
        std::thread::spawn(move || watched.watch_child(&job_id, child, deadline));
        Ok(record)
    }

    pub fn list(&self) -> Result<Vec<JobRecord>> {
        self.inner.list()
    }

    pub fn get(&self, job_id: &str) -> Result<JobRecord> {
        self.inner.get(job_id)
    }

    /// Re-attaches Jobs that were running when the daemon stopped: still-live processes resume
    /// (`recoveredAfterRestart`), others take the exit their runner recorded, or become `lost`.
    pub fn recover(&self) -> Result<Vec<JobRecord>> {
        let mut out = Vec::new();
        for mut record in self.inner.list()? {
            if !matches!(
                record.state,
                JobState::Starting
                    | JobState::Running
                    | JobState::Checkpointing
                    | JobState::Finishing
            ) {
                out.push(record);
                continue;
            }
            if !self.inner.is_alive(&record) {
                let _guard = self.inner.lock();
                out.push(
                    self.inner
                        .reconcile_exited(record, "process_identity_not_recoverable")?,
                );
                continue;
            }
            record.state = JobState::Running;
            record.recovered_after_restart = true;
            self.inner.save(&record)?;
            let watched = Arc::clone(&self.inner);
            let job_id = record.spec.job_id.clone();
            std::thread::spawn(move || watched.watch_recovered(&job_id));
            out.push(record);
        }
        Ok(out)
    }

    pub fn checkpoint(&self, job_id: &str) -> Result<JobRecord> {
        let inner = &self.inner;
        let _guard = inner.lock();
        let mut record = inner.get(job_id)?;
        let Some(command) = record
            .spec
            .checkpoint_command
            .clone()
            .filter(|c| record.state == JobState::Running && !c.is_empty())
        else {
            return rejected("Running job does not declare a checkpoint command");
        };
        record.state = JobState::Checkpointing;
        inner.save(&record)?;
        let mut run = match record.spec.runner {
            Runner::Wsl2 => {
                let mut run = hidden("wsl.exe");
                run.args(distribution_args(&record))
                    .arg("--exec")
                    .args(&command);
                run
            }
            Runner::Native => {
                let mut run = hidden(&command[0]);
                run.args(&command[1..]);
                run
            }
        };
        run.current_dir(&record.spec.working_directory);
        let status = process::output_with_timeout(&mut run, Duration::from_secs(60))
            .ok()
            .and_then(|(status, _)| status);
        if status.is_some_and(|s| s.success()) {
            record.state = JobState::Running;
            inner.save(&record)?;
            return Ok(record);
        }
        inner.terminate(&record);
        record.state = JobState::Failed;
        record.failure_reason = Some("checkpoint_failed".into());
        record.finished_at = Some(now_utc());
        inner.save(&record)?;
        Ok(record)
    }

    pub fn cancel(&self, job_id: &str, reason: &str) -> Result<JobRecord> {
        self.inner.cancel(job_id, reason)
    }

    /// The last `maximum_bytes` of a Job's log, decoded as UTF-8 (lossily, like Node's Buffer).
    pub fn tail(&self, job_id: &str, stream: Stream, maximum_bytes: u64) -> Result<String> {
        let record = self.inner.get(job_id)?;
        let path = if stream == Stream::Stdout {
            &record.stdout_path
        } else {
            &record.stderr_path
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return Ok(String::new());
        };
        if maximum_bytes > 1024 * 1024 {
            return rejected("Tail size must be between 0 and 1048576 bytes");
        }
        let size = file.metadata()?.len();
        let length = size.min(maximum_bytes);
        file.seek(SeekFrom::Start(size - length))?;
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.take(length).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Elapsed time, memory, GPU use, and output staleness, cached for 5 s per Job.
    pub fn resource_snapshot(&self, job_id: &str) -> Result<ResourceSnapshot> {
        self.inner.resource_snapshot(job_id)
    }

    /// [`resource_snapshot`](Self::resource_snapshot) for another thread: it runs telemetry
    /// commands, which would otherwise block the caller's event loop for seconds.
    pub fn resource_snapshot_task(
        &self,
        job_id: &str,
    ) -> impl FnOnce() -> Result<ResourceSnapshot> + Send + 'static {
        let (inner, job_id) = (Arc::clone(&self.inner), job_id.to_owned());
        move || inner.resource_snapshot(&job_id)
    }

    /// Stops watching: after this, exits are reconciled by the next daemon's `recover()`.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
    }
}

impl Drop for JobSupervisor {
    fn drop(&mut self) {
        self.close();
    }
}

impl Inner {
    fn resource_snapshot(&self, job_id: &str) -> Result<ResourceSnapshot> {
        let inner = self;
        if let Some((expires, value)) = inner
            .telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(job_id)
            .cloned()
            && expires > Instant::now()
        {
            return Ok(value);
        }
        let record = inner.get(job_id)?;
        let value = inner.collect_resources(&record);
        let mut cache = inner
            .telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.remove(job_id);
        // Bound the cache independently of the number of historical Jobs.
        if cache.len() >= 128 {
            let evict = cache
                .iter()
                .min_by_key(|(_, (expires, _))| *expires)
                .map(|(k, _)| k.clone());
            if let Some(evict) = evict {
                cache.remove(&evict);
            }
        }
        cache.insert(
            job_id.to_owned(),
            (Instant::now() + Duration::from_secs(5), value.clone()),
        );
        Ok(value)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn record_path(&self, job_id: &str) -> PathBuf {
        self.data_directory
            .join("jobs")
            .join(job_id)
            .join("job.json")
    }

    fn exit_path(&self, job_id: &str) -> PathBuf {
        self.data_directory
            .join("jobs")
            .join(job_id)
            .join("exit.json")
    }

    fn get(&self, job_id: &str) -> Result<JobRecord> {
        Ok(serde_json::from_str(&std::fs::read_to_string(
            self.record_path(job_id),
        )?)?)
    }

    fn list(&self) -> Result<Vec<JobRecord>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.data_directory.join("jobs"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && self.record_path(&name).exists() {
                out.push(self.get(&name)?);
            }
        }
        Ok(out)
    }

    fn save(&self, record: &JobRecord) -> Result<()> {
        crate::git::write_atomic(
            &self.record_path(&record.spec.job_id),
            &format!("{}\n", canonical_json(&serde_json::to_value(record)?)),
        )?;
        (self.on_change)(record);
        Ok(())
    }

    fn command_for(&self, record: &JobRecord) -> (String, Command) {
        match record.spec.runner {
            Runner::Native => {
                let mut command = hidden(&self.wrapper[0]);
                command
                    .args(&self.wrapper[1..])
                    .arg(self.exit_path(&record.spec.job_id))
                    .arg("--")
                    .args(&record.spec.command);
                (self.wrapper[0].to_string_lossy().into_owned(), command)
            }
            Runner::Wsl2 => {
                // Under `wsl.exe --exec` the shell already leads its session, so a plain `setsid` forks and returns at once; `setsid -w`
                // waits and keeps the exit status. The new session's leader writes its PID (the Job's process group), runs the command,
                // then records its exit status.
                let script = r#"umask 077; rm -f "$1" "$2"; exec setsid -w sh -c 'echo "$$" > "$0"; e=$1; shift; "$@"; c=$?; printf %s "$c" > "$e.tmp" && mv "$e.tmp" "$e"; exit $c' "$@""#;
                let mut command = hidden("wsl.exe");
                command
                    .args(distribution_args(record))
                    .args(["--exec", "sh", "-c", script, "nosh"])
                    .arg(record.wsl_pid_file.as_deref().unwrap_or_default())
                    .arg(wsl_exit_file(&record.spec.job_id))
                    .args(&record.spec.command);
                ("wsl.exe".into(), command)
            }
        }
    }

    /// Waits for a started Job's process, enforcing its deadline.
    fn watch_child(&self, job_id: &str, mut child: Child, deadline: Option<Instant>) {
        let mut timed_out = false;
        loop {
            if self.closed() {
                return;
            }
            match child.try_wait() {
                Ok(Some(status)) => return self.finish(job_id, status.code().map(node_exit_code)),
                Ok(None) => {}
                Err(_) => return,
            }
            if !timed_out && deadline.is_some_and(|d| Instant::now() >= d) {
                timed_out = true;
                let _ = self.cancel(job_id, "timeout");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Polls a Job re-attached after a restart until it ends, enforcing its original deadline.
    fn watch_recovered(&self, job_id: &str) {
        loop {
            let Ok(record) = self.get(job_id) else { return };
            let deadline = deadline_of(&record);
            let wait = deadline.map_or(Duration::from_secs(2), |d| {
                d.saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(2))
            });
            std::thread::sleep(wait);
            // A record removed underneath the watch (deleted data) ends it.
            if self.closed() || !self.record_path(job_id).exists() {
                return;
            }
            let Ok(record) = self.get(job_id) else { return };
            if record.state != JobState::Running {
                return;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                let _ = self.cancel(job_id, "timeout");
                return;
            }
            if !self.is_alive(&record) {
                let _guard = self.lock();
                let _ = self.reconcile_exited(record, "exit_unobserved_after_restart");
                return;
            }
        }
    }

    fn finish(&self, job_id: &str, exit_code: Option<i64>) {
        // After close(), restart recovery reconciles the outcome.
        if self.closed() || !self.record_path(job_id).exists() {
            return;
        }
        let outcome = read_exit_file(&self.exit_path(job_id)).unwrap_or_else(|| ExitOutcome {
            exit_code,
            signal: None,
            finished_at: now_utc(),
            error: None,
        });
        let _guard = self.lock();
        let _ = self.finish_record(job_id, &outcome);
    }

    fn finish_record(&self, job_id: &str, outcome: &ExitOutcome) -> Result<()> {
        let mut record = self.get(job_id)?;
        if record.state.is_terminal() {
            return Ok(());
        }
        record.state = JobState::Finishing;
        self.save(&record)?;
        record.exit_code = outcome.exit_code;
        record.finished_at = Some(outcome.finished_at.clone());
        let succeeded = outcome.exit_code == Some(0);
        record.state = if succeeded {
            JobState::Completed
        } else {
            JobState::Failed
        };
        record.failure_reason = (!succeeded).then(|| {
            outcome
                .error
                .clone()
                .unwrap_or_else(|| match &outcome.signal {
                    Some(signal) => format!("signal_{signal}"),
                    None => format!(
                        "exit_{}",
                        outcome
                            .exit_code
                            .map_or_else(|| "null".to_owned(), |c| c.to_string())
                    ),
                })
        });
        self.save(&record)
    }

    /// The process is gone: record the exit its runner wrote down, or `lost` when there is none.
    fn reconcile_exited(
        &self,
        mut record: JobRecord,
        unrecorded_reason: &str,
    ) -> Result<JobRecord> {
        let outcome = match record.spec.runner {
            Runner::Wsl2 => wsl_exit(&record),
            Runner::Native => read_exit_file(&self.exit_path(&record.spec.job_id)),
        };
        if let Some(outcome) = outcome {
            self.finish_record(&record.spec.job_id, &outcome)?;
            return self.get(&record.spec.job_id);
        }
        record.state = JobState::Lost;
        record.failure_reason = Some(unrecorded_reason.into());
        record.finished_at = Some(now_utc());
        self.save(&record)?;
        Ok(record)
    }

    fn cancel(&self, job_id: &str, reason: &str) -> Result<JobRecord> {
        let _guard = self.lock();
        let mut record = self.get(job_id)?;
        if record.state.is_terminal() {
            return Ok(record);
        }
        self.terminate(&record);
        record.state = JobState::Cancelled;
        record.failure_reason = Some(reason.into());
        record.finished_at = Some(now_utc());
        self.save(&record)?;
        Ok(record)
    }

    fn launch_failed(&self, mut record: JobRecord, message: &str) -> Result<JobRecord> {
        record.state = JobState::Failed;
        record.failure_reason = Some(if message.is_empty() {
            "launch_failed".into()
        } else {
            message.into()
        });
        record.finished_at = Some(now_utc());
        self.save(&record)?;
        Ok(record)
    }

    fn terminate(&self, record: &JobRecord) {
        if record.spec.runner == Runner::Wsl2 {
            if let Some(pid_file) = &record.wsl_pid_file {
                // No `--` before the process group: dash (Ubuntu's /bin/sh) rejects it as an illegal number.
                let script = r#"pid=$(cat "$1" 2>/dev/null) || exit 0; kill -TERM -"$pid" 2>/dev/null || true"#;
                let mut command = hidden("wsl.exe");
                command
                    .args(distribution_args(record))
                    .args(["--exec", "sh", "-c", script, "nosh", pid_file]);
                let _ = process::output_with_timeout(&mut command, Duration::from_secs(10));
            }
            return;
        }
        if let Some(pid) = record.windows_pid {
            process::kill_tree(pid, record.process_fingerprint.as_deref());
        }
    }

    fn is_alive(&self, record: &JobRecord) -> bool {
        match record.spec.runner {
            Runner::Native => alive(record.windows_pid, record.process_fingerprint.as_deref()),
            Runner::Wsl2 => record.wsl_pid_file.as_ref().is_some_and(|pid_file| {
                let script =
                    r#"pid=$(cat "$1" 2>/dev/null) || exit 1; kill -0 -"$pid" 2>/dev/null"#;
                let mut command = hidden("wsl.exe");
                command
                    .args(distribution_args(record))
                    .args(["--exec", "sh", "-c", script, "nosh", pid_file]);
                process::output_with_timeout(&mut command, Duration::from_secs(10))
                    .ok()
                    .and_then(|(status, _)| status)
                    .is_some_and(|s| s.success())
            }),
        }
    }

    fn collect_resources(&self, record: &JobRecord) -> ResourceSnapshot {
        let now = SystemTime::now();
        let millis = |at: &Option<String>| at.as_deref().and_then(parse_iso_millis);
        let now_ms = system_millis(now);
        let elapsed_seconds = millis(&record.started_at).map_or(0, |start| {
            let end = millis(&record.finished_at).unwrap_or(now_ms);
            u64::try_from(((end - start) / 1000).max(0)).unwrap_or(0)
        });
        let newest = [&record.stdout_path, &record.stderr_path]
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
            .map(system_millis)
            .max()
            .unwrap_or(0);
        let resident_bytes = match (record.windows_pid, &record.finished_at) {
            (Some(pid), None) if cfg!(windows) => {
                // A native Job's command is its exit wrapper's only child; measure the command, not the wrapper.
                let query = match record.spec.runner {
                    Runner::Native => format!(
                        "(Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' -ErrorAction Stop | Measure-Object -Property WorkingSetSize -Sum).Sum"
                    ),
                    Runner::Wsl2 => {
                        format!("(Get-Process -Id {pid} -ErrorAction Stop).WorkingSet64")
                    }
                };
                telemetry("powershell.exe", &["-NoProfile", "-Command", &query])
                    .and_then(|out| out.trim().parse::<f64>().ok())
                    .filter(|bytes| bytes.is_finite() && *bytes >= 0.0)
            }
            _ => None,
        };
        let gpu = record.spec.uses_gpu.then(|| self.gpu_snapshot()).flatten();
        let output_stalled_seconds = if newest > 0 {
            u64::try_from(((now_ms - newest) / 1000).max(0)).unwrap_or(0)
        } else {
            elapsed_seconds
        };
        ResourceSnapshot {
            elapsed_seconds,
            resident_bytes,
            gpu,
            output_stalled_seconds,
        }
    }

    fn gpu_snapshot(&self) -> Option<Value> {
        let mut cache = self
            .gpu
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((expires, value)) = cache.as_ref()
            && *expires > Instant::now()
        {
            return value.clone();
        }
        let value = telemetry("nvidia-smi", &["--query-gpu=uuid,utilization.gpu,memory.used,temperature.gpu", "--format=csv,noheader,nounits"]).map(|output| {
            let finite = |value: Option<&str>| value.and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite()).map_or(Value::Null, |n| json!(n));
            Value::Array(
                output
                    .trim()
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| {
                        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
                        json!({ "uuid": parts.first().copied().unwrap_or(""), "utilizationPercent": finite(parts.get(1).copied()), "memoryMiB": finite(parts.get(2).copied()), "temperatureC": finite(parts.get(3).copied()) })
                    })
                    .collect(),
            )
        });
        *cache = Some((Instant::now() + Duration::from_secs(5), value.clone()));
        value
    }
}

fn validate_spec(spec: &JobSpec) -> Result<()> {
    let id_ok = spec.job_id.strip_prefix("job_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    });
    if !id_ok {
        return rejected("Invalid Job ID");
    }
    if spec.command.is_empty() || spec.command.iter().any(|part| part.contains('\0')) {
        return rejected("Job command must be a non-empty argument array");
    }
    if !Path::new(&spec.working_directory).is_dir() {
        return rejected("Job working directory does not exist");
    }
    if spec.runner == Runner::Wsl2 && spec.distribution.as_deref().is_none_or(str::is_empty) {
        return rejected("WSL2 jobs require a distribution");
    }
    Ok(())
}

fn distribution_args(record: &JobRecord) -> Vec<String> {
    record
        .spec
        .distribution
        .as_ref()
        .map_or_else(Vec::new, |d| vec!["-d".into(), d.clone()])
}

fn wsl_exit_file(job_id: &str) -> String {
    format!("/tmp/nosh-{job_id}.exit")
}

fn wsl_exit(record: &JobRecord) -> Option<ExitOutcome> {
    let mut command = hidden("wsl.exe");
    command.args(distribution_args(record)).args([
        "--exec",
        "cat",
        &wsl_exit_file(&record.spec.job_id),
    ]);
    let (status, output) =
        process::output_with_timeout(&mut command, Duration::from_secs(10)).ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    (status?.success() && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| {
        ExitOutcome {
            exit_code: text.parse().ok(),
            signal: None,
            finished_at: now_utc(),
            error: None,
        }
    })
}

fn read_exit_file(path: &Path) -> Option<ExitOutcome> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// `startedAt + timeoutSeconds`, or no deadline.
fn deadline_of(record: &JobRecord) -> Option<Instant> {
    if record.spec.timeout_seconds <= 0.0 {
        return None;
    }
    let started = parse_iso_millis(record.started_at.as_deref()?)?;
    let deadline_ms = started as f64 + record.spec.timeout_seconds * 1000.0;
    let remaining_ms = deadline_ms - system_millis(SystemTime::now()) as f64;
    Some(Instant::now() + Duration::from_secs_f64((remaining_ms / 1000.0).max(0.0)))
}

fn parse_iso_millis(text: &str) -> Option<i64> {
    let at =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

fn system_millis(at: SystemTime) -> i64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn telemetry(program: &str, args: &[&str]) -> Option<String> {
    let mut command = hidden(program);
    command.args(args);
    let (status, output) =
        process::output_with_timeout(&mut command, Duration::from_secs(3)).ok()?;
    status?
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses a metrics file, refusing NaN or infinity as the TypeScript loader does.
pub fn load_native_evaluation(path: &Path) -> Result<Value> {
    // serde_json already rejects NaN and Infinity, which JSON cannot express.
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// `{ runId, metrics }` from a Weights & Biases run directory.
pub fn load_wandb_summary(directory: &Path) -> Result<Value> {
    let metadata = directory.join("wandb-metadata.json");
    let run_id = if metadata.exists() {
        serde_json::from_str::<Value>(&std::fs::read_to_string(metadata)?)?
            .get("id")
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    Ok(
        json!({ "runId": run_id, "metrics": load_native_evaluation(&directory.join("wandb-summary.json"))? }),
    )
}

/// `{ logDirectory, eventFiles }` for a TensorBoard log directory.
pub fn tensorboard_reference(log_directory: &Path) -> Result<Value> {
    let root = resolve(log_directory)?;
    let mut files: Vec<String> = std::fs::read_dir(&root)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("events.out.tfevents"))
        .collect();
    files.sort();
    Ok(json!({ "logDirectory": root.to_string_lossy(), "eventFiles": files }))
}
