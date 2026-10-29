//! The launch configuration of the terminal UI: the `TuiConfig` interface of `apps/tui/src/client.ts`.
//!
//! Specified by tui-transport.md §1.1, tui-workspace.md §1.3 and CA-4, cli.md CL-1, INDEX.md §2
//! item 1 and §3. The CLI builds the value directly and calls [`crate::run`]; the serde form (camelCase
//! JSON) exists so tests can reuse the TypeScript fixtures. `main_from_env` and `from_env` are not
//! ported (CA-4).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `TuiConfig`: where the daemon is, how to authenticate, which project to open and where to record
/// the project the TUI ends on.
///
/// JSON keys are `baseUrl`, `bootstrapToken`, `sessionToken`, `currentProjectId` and `handoffPath`.
/// `null` is the same as an absent key (the CLI really sends `currentProjectId: null`), unknown keys
/// are ignored, and a non-string token is a deserialization error rather than a request with
/// `Bearer 123` (transport §1.1, a benign divergence). `baseUrl` is required; its value is only
/// checked when the client is built.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiConfig {
    /// `baseUrl`: the daemon origin, e.g. `http://127.0.0.1:4321`.
    pub base_url: String,
    /// `bootstrapToken`: exchanged for a session token at `POST /api/session` when no session token
    /// is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_token: Option<String>,
    /// `sessionToken`: used as is; renewed through the bootstrap token after a 401.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// `currentProjectId`: the project to open first; `None` means the controller picks the first
    /// registered project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_project_id: Option<String>,
    /// `handoffPath`: where `{"currentProjectId":"<id>"}` is written at exit (`tui-session.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_path: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_cli_json_shape() {
        let config: TuiConfig = serde_json::from_str(
            r#"{"baseUrl":"http://127.0.0.1:4321","bootstrapToken":"boot","currentProjectId":null,"handoffPath":"C:\\state\\NOSH\\tui-session.json","extra":1}"#,
        )
        .expect("valid config");
        assert_eq!(
            config,
            TuiConfig {
                base_url: "http://127.0.0.1:4321".into(),
                bootstrap_token: Some("boot".into()),
                session_token: None,
                current_project_id: None,
                handoff_path: Some(PathBuf::from(r"C:\state\NOSH\tui-session.json")),
            }
        );
    }

    #[test]
    fn requires_a_base_url_and_string_tokens() {
        let missing = serde_json::from_str::<TuiConfig>(r#"{"sessionToken":"s"}"#).unwrap_err();
        assert!(missing.to_string().contains("baseUrl"), "{missing}");
        assert!(
            serde_json::from_str::<TuiConfig>(r#"{"baseUrl":"x","sessionToken":123}"#).is_err()
        );
    }

    #[test]
    fn round_trips_without_absent_keys() {
        let config = TuiConfig {
            base_url: "http://127.0.0.1".into(),
            session_token: Some("test".into()),
            ..TuiConfig::default()
        };
        let text = serde_json::to_string(&config).expect("serializes");
        assert_eq!(
            text,
            r#"{"baseUrl":"http://127.0.0.1","sessionToken":"test"}"#
        );
        assert_eq!(
            serde_json::from_str::<TuiConfig>(&text).expect("parses"),
            config
        );
    }
}
