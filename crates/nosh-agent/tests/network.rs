//! `read_allowed_url` against TypeScript's `readAllowedUrl` on live HTTPS hosts. It needs the
//! internet, so it is ignored by default: `cargo test -p nosh-agent --test network -- --ignored`.

use nosh_agent::network::read_allowed_url;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run_typescript(ops: &[Value]) -> Vec<Value> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pi-adapter-ts.mjs");
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(ops).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "the TypeScript side ran");
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Live bodies can change between two requests; compare everything else and the body's presence.
fn summary(outcome: &Value) -> Value {
    match outcome.get("ok") {
        Some(ok) => {
            json!({ "url": ok["url"], "status": ok["status"], "contentType": ok["contentType"], "body": !ok["body"].as_str().unwrap_or_default().is_empty() })
        }
        None => json!({ "error": outcome["error"] }),
    }
}

#[tokio::test]
#[ignore = "needs the internet"]
async fn reads_live_https_like_typescript() {
    let allowlist = vec![
        "example.com".to_owned(),
        "api.crossref.org".to_owned(),
        "github.com".to_owned(),
    ];
    let urls = [
        "https://example.com/",
        "https://EXAMPLE.com:443/?q=1#x",
        "https://api.crossref.org/works?rows=0",
        "https://github.com/SirNosh/NOSH/nonexistent-page",
        "https://github.com/sirnosh/nosh",
        "https://github.com/SirNosh/NOSH/archive/HEAD.zip",
        "https://api.crossref.org/works?rows=400",
    ];
    let ops: Vec<Value> = urls
        .iter()
        .map(|url| json!({ "fn": "readAllowedUrl", "args": [url, allowlist] }))
        .collect();
    let expected = run_typescript(&ops);
    for (url, expected) in urls.iter().zip(expected) {
        let actual = match read_allowed_url(url, &allowlist).await {
            Ok(value) => json!({ "ok": value }),
            Err(error) => json!({ "error": error }),
        };
        assert_eq!(summary(&actual), summary(&expected), "{url}");
        println!("{url}: {}", summary(&actual));
    }
}
