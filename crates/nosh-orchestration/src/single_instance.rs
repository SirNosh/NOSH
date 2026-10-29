//! The daemon's single-instance lock file, as `apps/noshd/src/single-instance.ts`: one daemon per
//! data directory. The file holds `{"pid":…,"token":…}`, so either daemon recognises the other's
//! lock and removes it only when its process is gone.

use crate::{Result, rejected};
use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SingleInstanceLock {
    path: PathBuf,
    token: String,
    acquired: Cell<bool>,
}

impl SingleInstanceLock {
    pub fn new(path: &Path) -> Self {
        SingleInstanceLock {
            path: path.to_path_buf(),
            token: uuid::Uuid::new_v4().to_string(),
            acquired: Cell::new(false),
        }
    }

    pub fn acquire(&self) -> Result<()> {
        match self.create() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !self.remove_stale_lock() {
                    return rejected(format!(
                        "Another noshd instance owns {}",
                        self.path.display()
                    ));
                }
                self.create().map_err(io_error)?;
            }
            Err(error) => return Err(io_error(error)),
        }
        self.acquired.set(true);
        Ok(())
    }

    pub fn release(&self) {
        if !self.acquired.get() || !self.path.exists() {
            return;
        }
        if std::fs::read_to_string(&self.path).is_ok_and(|current| current.contains(&self.token)) {
            let _ = std::fs::remove_file(&self.path);
        }
        self.acquired.set(false);
    }

    /// `writeFileSync(path, …, { flag: "wx" })`.
    fn create(&self) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        let text = serde_json::json!({ "pid": std::process::id(), "token": self.token });
        file.write_all(text.to_string().as_bytes())
    }

    /// Removes a lock whose process is gone; a live (or unqueryable) owner keeps it.
    fn remove_stale_lock(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let Ok(lock) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        if let Some(pid) = lock
            .get("pid")
            .and_then(serde_json::Value::as_f64)
            .filter(|pid| *pid > 0.0)
        {
            // A pid beyond u32 names no process, as `process.kill` reports for it.
            let running = if pid.fract() == 0.0 && pid <= f64::from(u32::MAX) {
                nosh_sys::process::pid_exists(pid as u32)
            } else {
                Some(false)
            };
            if running != Some(false) {
                return false;
            }
        }
        std::fs::remove_file(&self.path).is_ok()
    }
}

impl Drop for SingleInstanceLock {
    fn drop(&mut self) {
        self.release();
    }
}

fn io_error(error: std::io::Error) -> crate::Error {
    crate::Error::Rejected(error.to_string())
}
