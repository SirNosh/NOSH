//! `config.json` (`loadConfig`, `saveConfig` and `writeAtomic` in `main.ts`). The parsed object is
//! kept whole, so keys NOSH does not know survive every rewrite; there is no validation, and a
//! field is only checked where a command uses it.

use super::Cli;
use nosh_wire::js;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `Config`: `{ dataDirectory, port, bootstrapToken, currentProjectId, defaultModel? }` plus any
/// other keys the file holds.
#[derive(Clone)]
pub(super) struct Config(Map<String, Value>);

impl Config {
    pub(super) fn new(fields: Map<String, Value>) -> Self {
        Self(fields)
    }

    pub(super) fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// `{ ...config, [key]: value }`.
    pub(super) fn with(&self, key: &str, value: Value) -> Self {
        let mut fields = self.0.clone();
        fields.insert(key.to_owned(), value);
        Self(fields)
    }

    /// `config.dataDirectory`, which Node's `path.join` requires to be a string.
    pub(super) fn data_directory(&self) -> Result<PathBuf, String> {
        match self.get("dataDirectory") {
            Some(Value::String(directory)) => Ok(PathBuf::from(directory)),
            _ => Err("config.json has no dataDirectory string".into()),
        }
    }

    /// `url(config)`: `http://127.0.0.1:{port}`, the port as JavaScript interpolates it.
    pub(super) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", js::string(self.get("port")))
    }

    /// `config.currentProjectId` when it is truthy, as JavaScript interpolates it.
    pub(super) fn current_project_id(&self) -> Option<String> {
        let value = self.get("currentProjectId");
        js::truthy(value).then(|| js::string(value))
    }

    /// `config.defaultModel` when it is truthy.
    pub(super) fn default_model(&self) -> Option<String> {
        let value = self.get("defaultModel");
        js::truthy(value).then(|| js::string(value))
    }
}

impl Cli {
    /// `loadConfig()`: one leading BOM is dropped (Windows editors may add one); a syntax error
    /// carries V8's `JSON.parse` message.
    pub(super) fn load_config(&self) -> Result<Config, String> {
        if !self.config_path.exists() {
            return Err("NOSH is not initialized; run `nosh setup`".into());
        }
        let bytes = std::fs::read(&self.config_path).map_err(|error| error.to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
        match nosh_agent::js_json::parse(text).map_err(|error| error.message)? {
            Value::Object(fields) => Ok(Config(fields)),
            _ => Err("config.json is not a JSON object".into()),
        }
    }

    /// `saveConfig(config)`.
    pub(super) fn save_config(&self, config: &Config) -> Result<(), String> {
        write_atomic(&self.config_path, &Value::Object(config.0.clone()))
    }
}

/// `writeAtomic(path, value, 0o600)`: canonical JSON and a newline, written beside `path` and
/// renamed over it.
fn write_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let failed = |error: std::io::Error| error.to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary).map_err(failed)?;
    file.write_all(format!("{}\n", nosh_wire::canonical_json(value)).as_bytes())
        .map_err(failed)?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(failed)
}
