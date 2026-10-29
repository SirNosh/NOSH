//! The daemon client (`api` and `healthy` in `main.ts`). Requests carry no credentials: the daemon
//! trusts loopback requests without an `Origin`. Failures read as Node 22's `fetch` reports them.

use super::Cli;
use super::config::Config;
use reqwest::Method;
use serde_json::Value;
use std::time::Duration;

impl Cli {
    /// `healthy(config)`: `GET /health` answers 2xx within 3 s.
    pub(super) fn healthy(&self, config: &Config) -> bool {
        let request = self
            .client
            .get(format!("{}/health", config.url()))
            .timeout(Duration::from_secs(3));
        self.block_on(async {
            request
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
        })
    }

    /// `api(config, path, { method, body })` with a 300 s timeout. `body` is JSON text, sent as
    /// `application/json`. A refused request fails with the daemon's `error`.
    pub(super) fn api(
        &self,
        config: &Config,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Value, String> {
        let url = format!("{}/api{path}", config.url());
        let mut request = self
            .client
            .request(method, &url)
            .timeout(Duration::from_secs(300));
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body);
        }
        self.block_on(async {
            let failed = |error: reqwest::Error| fetch_error(&url, &error);
            let response = request.send().await.map_err(failed)?;
            let status = response.status();
            let text = response.text().await.map_err(failed)?;
            // `response.json()` decodes the body as UTF-8, which drops one leading BOM.
            let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
            if !status.is_success() {
                // `(await response.json().catch(() => ({}))).error ?? …`
                let body = nosh_agent::js_json::parse(text).unwrap_or_default();
                return Err(match body.get("error") {
                    None | Some(Value::Null) => {
                        format!("daemon request failed ({})", status.as_u16())
                    }
                    Some(error) => nosh_wire::js::string(Some(error)),
                });
            }
            nosh_agent::js_json::parse(text).map_err(|error| error.message)
        })
    }
}

/// The message of Node's `fetch` rejection.
fn fetch_error(url: &str, error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "The operation was aborted due to timeout".into()
    } else if error.is_builder() {
        format!("Failed to parse URL from {url}")
    } else {
        "fetch failed".into()
    }
}

/// `encodeURIComponent(text)`.
pub(super) fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_uri_components_as_javascript_does() {
        // `encodeURIComponent` in Node 22.
        assert_eq!(
            super::encode_uri_component("a/b c?d&e=f#é~*'()-_.!:@+,;$%😀"),
            "a%2Fb%20c%3Fd%26e%3Df%23%C3%A9~*'()-_.!%3A%40%2B%2C%3B%24%25%F0%9F%98%80"
        );
    }
}
