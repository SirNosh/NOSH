//! `update` and `help` in `main.ts`.

use super::out;
use nosh_server::noshd::Installation;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// `update()`: how to update this copy. Installs made through the npm launcher never get here;
/// the launcher handles `nosh update` itself.
pub(super) fn update() {
    // The installation root, else the executable's directory, with a trailing separator.
    let root: PathBuf = Installation::find()
        .ok()
        .and_then(|installation| installation.package_path.parent().map(Path::to_path_buf))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|executable| executable.parent().map(Path::to_path_buf))
        })
        .unwrap_or_default();
    let mut root = root.display().to_string();
    if !root.ends_with(MAIN_SEPARATOR) {
        root.push(MAIN_SEPARATOR);
    }
    out(&format!(
        "This NOSH runs from {root}.\nSource checkout: git pull && cargo build --release, then nosh stop and nosh start.\nnpm install: npm install -g nosh-harness@latest, then nosh update.\n"
    ));
}

/// `help()`.
pub(super) fn help() {
    out(
        "nosh setup [--model=provider/id:level] [--yes]\nnosh login [--list]   connect a ChatGPT/Claude subscription or an API key\nnosh logout <provider>\nnosh start|stop [--if-idle]|status|open|tui|doctor\nnosh update\nnosh model [show|list|set <provider/id[:level]> [--restart]]\nnosh project list|open <path>\nnosh mission list|status <id>\nnosh job list\nnosh backup <project>\nnosh backup restore <project> <backup-id>\nnosh logs\n",
    );
}
