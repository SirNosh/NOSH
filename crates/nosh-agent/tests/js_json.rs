//! `js_json::parse` against Node's `JSON.parse` on generated near-JSON text.

use nosh_agent::js_json;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// xorshift64*: deterministic cases without a dependency.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }
}

const BASES: &[&str] = &[
    r#"{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1,"records":[{"$schema":"https://nosh.dev/schemas/general-worker-completion/v1","taskId":"tsk_0123","summary":"Ran the ablation — 3 seeds, \"mean\" 0.81 ± 0.02 😀","criteria":[{"criterionId":"c1","status":"met","validatorRunIds":["job_1","job_2"]}],"metrics":{"loss":-1.5e-3,"steps":1200,"ok":true,"none":null}}]}"#,
    concat!(
        "{
  ",
        r#""a": [1, 2.5, -0, 3e10, {"b": "tab\tline\nquote\" slash\/ u\u00e9"}],"#,
        "
  \"é\": \"日本語\",
  \"c\": {}
}"
    ),
    r#"{"records":[{"guardrailRisks":["x"]},"novelty",{"z":[[],[[]],{}]}],"n":[0,0.0,10,1E5,1e-5,-12.75]}"#,
    "  {\"k\" : true , \"l\" : false , \"m\" : null }  ",
];

const ALPHABET: &[&str] = &[
    "{",
    "}",
    "[",
    "]",
    ":",
    ",",
    "\"",
    "\\",
    "u",
    "0",
    "1",
    "9",
    "-",
    "+",
    ".",
    "e",
    "E",
    "t",
    "f",
    "n",
    "a",
    "x",
    " ",
    "\n",
    "\r",
    "\t",
    "\u{1}",
    "\u{0}",
    "é",
    "€",
    "😀",
    "\u{a0}",
    "\u{2028}",
    "<|im_end|>",
    "true",
    "null",
    "\\u",
    "\\x",
    "\\é",
    "\\€",
];

fn cases() -> Vec<String> {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let mut cases: Vec<String> = BASES.iter().map(|base| (*base).to_owned()).collect();
    for _ in 0..12_000 {
        let base: Vec<char> = BASES[random.below(BASES.len())].chars().collect();
        let mut text = base.clone();
        for _ in 0..1 + random.below(3) {
            let at = random.below(text.len() + 1);
            match random.below(4) {
                0 if at < text.len() => {
                    text.remove(at);
                }
                1 => text
                    .splice(at..at, ALPHABET[random.below(ALPHABET.len())].chars())
                    .for_each(drop),
                2 if at < text.len() => {
                    text.splice(at..=at, ALPHABET[random.below(ALPHABET.len())].chars())
                        .for_each(drop);
                }
                _ => text.truncate(at),
            }
        }
        cases.push(text.into_iter().collect());
    }
    for _ in 0..6_000 {
        let length = random.below(8);
        cases.push(
            (0..length)
                .map(|_| ALPHABET[random.below(ALPHABET.len())])
                .collect(),
        );
    }
    cases.extend(["NaN", "Infinity", "undefined", "[object Object]", "", " "].map(str::to_owned));
    cases
}

#[test]
fn matches_node_json_parse_errors() {
    let cases = cases();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/v8-json.mjs");
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
        .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let expected: Vec<Option<String>> = serde_json::from_slice(&output.stdout).unwrap();
    let mut mismatches = Vec::new();
    let mut errors = 0;
    for (text, expected) in cases.iter().zip(&expected) {
        let actual = js_json::parse(text).err().map(|error| error.message);
        errors += usize::from(expected.is_some());
        if &actual != expected {
            mismatches.push(format!(
                "{text:?}\n  node: {expected:?}\n  rust: {actual:?}"
            ));
        }
    }
    assert!(errors > 10_000, "the cases exercise errors ({errors})");
    assert!(
        mismatches.is_empty(),
        "{} of {} differ:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
