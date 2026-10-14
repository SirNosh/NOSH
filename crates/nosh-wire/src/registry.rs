//! Record validation against the vendored TypeScript JSON Schemas, adapted to zod's semantics.
//!
//! The JSON Schemas come from `zod-to-json-schema`, which loses some of zod's behaviour. Loading
//! restores it:
//! - `minLength`/`maxLength` count UTF-16 code units (JavaScript `.length`), not code points.
//! - `pattern` regexes are ECMAScript; `jsonschema` translates them (`\d` is ASCII-only, etc.).
//! - `date-time` and `date` use zod's exact regexes (recorded in refinements.json); `uri` is a
//!   WHATWG URL parse, like zod's `new URL(value)`.
//! - Refinements and defaults are applied by [`crate::refinements`].

use crate::refinements::Extras;
use jsonschema::{
    Draft, Keyword, PatternOptions, ValidationError, Validator, error::ValidationErrorKind,
    paths::Location,
};
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::LazyLock;

include!(concat!(env!("OUT_DIR"), "/schemas.rs"));

const MIN_UTF16: &str = "x-nosh-minUtf16Length";
const MAX_UTF16: &str = "x-nosh-maxUtf16Length";

/// One validation problem, shaped like the TypeScript `validateRecord` errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// JSON Pointer into the record ("" for the root).
    pub pointer: String,
    /// A zod-style issue code, e.g. `unrecognized_keys`, `invalid_type`, `custom`.
    pub code: String,
    pub message: String,
}

struct Formats {
    date_time: Regex,
    date: Regex,
}

static EXTRAS: LazyLock<Extras> = LazyLock::new(|| {
    Extras::parse(include_str!(
        "../../../conformance/schemas/refinements.json"
    ))
});

static FORMATS: LazyLock<Formats> = LazyLock::new(|| {
    let compile = |name: &str| {
        let source = EXTRAS
            .formats
            .get(name)
            .unwrap_or_else(|| panic!("refinements.json has no {name} format"));
        let rust = jsonschema_regex::to_rust_regex(source)
            .unwrap_or_else(|()| panic!("unsupported {name} regex"));
        Regex::new(&rust).expect("zod format regex")
    };
    Formats {
        date_time: compile("date-time"),
        date: compile("date"),
    }
});

/// Schema URI -> (schema name, validator).
static VALIDATORS: LazyLock<BTreeMap<String, (String, Validator)>> = LazyLock::new(|| {
    let mut validators = BTreeMap::new();
    for (name, text) in SCHEMAS {
        let mut document: Value =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        adapt(&mut document);
        let uri = document["$id"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: no $id"))
            .to_owned();
        let validator = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true)
            .with_format("date-time", |s: &str| FORMATS.date_time.is_match(s))
            .with_format("date", |s: &str| FORMATS.date.is_match(s))
            .with_format("uri", |s: &str| url::Url::parse(s).is_ok())
            .with_keyword(MIN_UTF16, min_utf16_length)
            .with_keyword(MAX_UTF16, max_utf16_length)
            .with_pattern_options(PatternOptions::regex())
            .build(&document)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        validators.insert(uri, ((*name).to_owned(), validator));
    }
    validators
});

/// Every schema URI, sorted.
pub fn schema_uris() -> Vec<String> {
    VALIDATORS.keys().cloned().collect()
}

/// Validates `value` as the record type `uri`. On success returns the value with zod defaults
/// applied, as TypeScript's `validateRecord` returns `result.data`.
pub fn validate_record(uri: &str, value: &Value) -> Result<Value, Vec<Issue>> {
    let Some((name, validator)) = VALIDATORS.get(uri) else {
        return Err(vec![Issue {
            pointer: "/$schema".into(),
            code: "unknown_schema".into(),
            message: format!("Unsupported schema: {uri}"),
        }]);
    };
    let issues: Vec<Issue> = validator
        .iter_errors(value)
        .flat_map(|error| issues_from(&error))
        .collect();
    if !issues.is_empty() {
        return Err(issues);
    }
    // zod applies defaults while parsing, so refinements see them.
    let mut value = value.clone();
    crate::json::normalize_numbers(&mut value);
    EXTRAS.apply_defaults(name, &mut value);
    let issues = EXTRAS.check(name, &value);
    if issues.is_empty() {
        Ok(value)
    } else {
        Err(issues)
    }
}

fn issues_from(error: &ValidationError<'_>) -> Vec<Issue> {
    let base = error.instance_path().as_str();
    let issue = |pointer: String, code: &str, message: String| Issue {
        pointer,
        code: code.into(),
        message,
    };
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => unexpected
            .iter()
            .map(|key| {
                issue(
                    format!("{base}/{}", escape_pointer(key)),
                    "unrecognized_keys",
                    format!("Unrecognized key: {key}"),
                )
            })
            .collect(),
        ValidationErrorKind::Required { property } => {
            let key = property.as_str().unwrap_or_default();
            vec![issue(
                format!("{base}/{}", escape_pointer(key)),
                "invalid_type",
                "Required: missing".into(),
            )]
        }
        ValidationErrorKind::AnyOf { context } => union_issues(error, base, context),
        kind => {
            let code = match kind {
                ValidationErrorKind::Type { .. } => "invalid_type",
                ValidationErrorKind::Enum { .. } => "invalid_enum_value",
                ValidationErrorKind::Constant { .. } => "invalid_literal",
                ValidationErrorKind::Pattern { .. } | ValidationErrorKind::Format { .. } => {
                    "invalid_string"
                }
                ValidationErrorKind::MinItems { .. }
                | ValidationErrorKind::Minimum { .. }
                | ValidationErrorKind::ExclusiveMinimum { .. } => "too_small",
                ValidationErrorKind::MaxItems { .. }
                | ValidationErrorKind::Maximum { .. }
                | ValidationErrorKind::ExclusiveMaximum { .. } => "too_big",
                ValidationErrorKind::OneOfNotValid { .. }
                | ValidationErrorKind::OneOfMultipleValid { .. } => "invalid_union",
                ValidationErrorKind::Custom { .. }
                    if error.schema_path().as_str().ends_with(MIN_UTF16) =>
                {
                    "too_small"
                }
                ValidationErrorKind::Custom { .. }
                    if error.schema_path().as_str().ends_with(MAX_UTF16) =>
                {
                    "too_big"
                }
                _ => "invalid",
            };
            vec![issue(base.to_owned(), code, error.to_string())]
        }
    }
}

/// zod reports a failed union the way it parsed it, and agents correct records from these issues:
/// - `.nullable()`/`.optional()` are not unions in zod: errors come from the inner schema.
/// - A discriminated union validates only the branch its discriminator selects, and reports a
///   missing or unknown discriminator as `invalid_union_discriminator` at that key.
///
/// JSON Schema expresses both as `anyOf`, so this recovers zod's view from each branch's errors.
fn union_issues(
    error: &ValidationError<'_>,
    base: &str,
    branches: &[Vec<ValidationError<'static>>],
) -> Vec<Issue> {
    // A type mismatch at the union itself (e.g. the `null` branch of a nullable) rules a branch out.
    let candidates: Vec<&Vec<ValidationError<'static>>> = branches
        .iter()
        .filter(|errors| {
            !errors.iter().any(|e| {
                matches!(e.kind(), ValidationErrorKind::Type { .. })
                    && e.instance_path().as_str() == base
            })
        })
        .collect();
    if let [branch] = candidates.as_slice() {
        return branch.iter().flat_map(issues_from).collect();
    }
    // Per branch, the keys directly under the union whose literal value it rejects or that it requires.
    let rejected: Vec<Vec<String>> = candidates
        .iter()
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| match e.kind() {
                    ValidationErrorKind::Constant { .. } => e
                        .instance_path()
                        .as_str()
                        .strip_prefix(base)?
                        .strip_prefix('/')
                        .filter(|key| !key.contains('/'))
                        .map(str::to_owned),
                    ValidationErrorKind::Required { property }
                        if e.instance_path().as_str() == base =>
                    {
                        property.as_str().map(escape_pointer)
                    }
                    _ => None,
                })
                .collect()
        })
        .collect();
    if let Some(key) = rejected
        .first()
        .into_iter()
        .flatten()
        .find(|key| rejected.iter().all(|keys| keys.contains(key)))
    {
        return vec![Issue {
            pointer: format!("{base}/{key}"),
            code: "invalid_union_discriminator".into(),
            message: "Invalid discriminator value".into(),
        }];
    }
    let selected: Vec<_> = candidates
        .iter()
        .zip(&rejected)
        .filter(|(_, keys)| keys.is_empty())
        .map(|(branch, _)| branch)
        .collect();
    if let [branch] = selected.as_slice() {
        return branch.iter().flat_map(issues_from).collect();
    }
    vec![Issue {
        pointer: base.to_owned(),
        code: "invalid_union".into(),
        message: error.to_string(),
    }]
}

pub(crate) fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// Rewrites a vendored JSON Schema so the validator follows zod's semantics (see the module docs).
fn adapt(node: &mut Value) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(adapt),
        Value::Object(map) => {
            for (from, to) in [("minLength", MIN_UTF16), ("maxLength", MAX_UTF16)] {
                if map.get(from).is_some_and(Value::is_number) {
                    let limit = map.remove(from).expect("checked above");
                    map.insert(to.to_owned(), limit);
                }
            }
            for (key, child) in map.iter_mut() {
                // Literal values, not subschemas.
                if !matches!(key.as_str(), "const" | "enum" | "default" | "examples") {
                    adapt(child);
                }
            }
        }
        _ => {}
    }
}

struct Utf16Length {
    limit: usize,
    minimum: bool,
}

impl<'i> Keyword<'i> for Utf16Length {
    fn validate(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
        if self.is_valid(instance) {
            return Ok(());
        }
        let bound = if self.minimum { "at least" } else { "at most" };
        Err(ValidationError::custom(format!(
            "String must contain {bound} {} character(s)",
            self.limit
        )))
    }

    fn is_valid(&self, instance: &'i Value) -> bool {
        let Some(text) = instance.as_str() else {
            return true;
        };
        let units = text.encode_utf16().count();
        if self.minimum {
            units >= self.limit
        } else {
            units <= self.limit
        }
    }
}

fn utf16_length<'a>(
    value: &'a Value,
    minimum: bool,
) -> Result<Box<dyn for<'i> Keyword<'i>>, ValidationError<'a>> {
    let limit = value
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| ValidationError::custom("length limits must be non-negative integers"))?;
    Ok(Box::new(Utf16Length { limit, minimum }))
}

fn min_utf16_length<'a>(
    _: &'a Map<String, Value>,
    value: &'a Value,
    _: Location,
) -> Result<Box<dyn for<'i> Keyword<'i>>, ValidationError<'a>> {
    utf16_length(value, true)
}

fn max_utf16_length<'a>(
    _: &'a Map<String, Value>,
    value: &'a Value,
    _: Location,
) -> Result<Box<dyn for<'i> Keyword<'i>>, ValidationError<'a>> {
    utf16_length(value, false)
}

#[cfg(test)]
pub(crate) fn is_wire_timestamp(value: &str) -> bool {
    FORMATS.date_time.is_match(value)
}

/// The TypeScript checkout used by tests that compare against its sources: `NOSH_TS_ROOT`, else
/// the parent of this worktree.
#[cfg(test)]
pub(crate) fn ts_root() -> std::path::PathBuf {
    std::env::var_os("NOSH_TS_ROOT").map_or_else(
        || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
        Into::into,
    )
}

#[cfg(test)]
pub(crate) fn vectors() -> &'static Value {
    static VECTORS: LazyLock<Value> = LazyLock::new(|| {
        serde_json::from_str(include_str!("../../../conformance/schemas/vectors.json"))
            .expect("vectors.json")
    });
    &VECTORS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixtures() -> &'static Path {
        static DIR: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/schemas/fixtures")
        });
        &DIR
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Mirrors packages/wire/src/fixtures.test.ts for every vendored schema.
    #[test]
    fn golden_fixtures_validate_and_hash_like_typescript() {
        let mut names: Vec<_> = SCHEMAS.iter().map(|(name, _)| *name).collect();
        let mut dirs: Vec<String> = std::fs::read_dir(fixtures())
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| std::fs::read_dir(entry.path()).unwrap().next().is_some())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_unstable();
        dirs.sort();
        assert_eq!(
            names, dirs,
            "every schema has golden fixtures and vice versa"
        );
        assert_eq!(schema_uris().len(), names.len());

        for name in names {
            let uri = format!("https://nosh.dev/schemas/{name}/v1");
            let dir = fixtures().join(name);
            let load =
                |file: &str| -> Value { serde_json::from_str(&read(&dir.join(file))).unwrap() };
            for file in ["minimal.valid.json", "full.valid.json"] {
                if let Err(issues) = validate_record(&uri, &load(file)) {
                    panic!("{name}/{file} rejected: {issues:#?}");
                }
            }
            for file in ["missing.invalid.json", "unknown.invalid.json"] {
                assert!(
                    validate_record(&uri, &load(file)).is_err(),
                    "{name}/{file} accepted"
                );
            }
            assert_eq!(
                crate::sha256(&load("full.valid.json")),
                read(&dir.join("full.sha256.txt")).trim(),
                "{name} hash"
            );
        }
    }

    /// Replays every conformance case recorded from the TypeScript validator, reporting every
    /// disagreement at once.
    #[test]
    fn agrees_with_typescript_verdicts() {
        let records = vectors()["records"].as_array().expect("record vectors");
        assert!(!records.is_empty());
        let mut failures = Vec::new();
        for case in records {
            let name = case["name"].as_str().unwrap();
            let input: Value = serde_json::from_str(case["input"].as_str().unwrap()).unwrap();
            let uri = format!(
                "https://nosh.dev/schemas/{}/v1",
                case["schema"].as_str().unwrap()
            );
            let result = validate_record(&uri, &input);
            let theirs: Vec<(&str, &str)> = case["issues"]
                .as_array()
                .map(|issues| {
                    issues
                        .iter()
                        .map(|i| (i["pointer"].as_str().unwrap(), i["code"].as_str().unwrap()))
                        .collect()
                })
                .unwrap_or_default();
            match (case["outcome"].as_str().unwrap(), &result) {
                ("accept", Ok(value)) if value != &case["value"] => {
                    failures.push(format!("{name}: parsed value differs: {value}"))
                }
                ("accept", Ok(_)) => {}
                ("custom", Err(issues)) => {
                    let mut ours: Vec<(&str, &str)> = issues
                        .iter()
                        .map(|i| (i.pointer.as_str(), i.code.as_str()))
                        .collect();
                    let mut theirs = theirs.clone();
                    ours.sort_unstable();
                    theirs.sort_unstable();
                    if ours != theirs {
                        failures.push(format!("{name}: TypeScript {theirs:?}, Rust {ours:?}"));
                    }
                }
                ("reject", Err(issues)) => {
                    if issues.iter().all(|i| i.code == "custom") {
                        failures.push(format!("{name}: rejected only by refinements: {issues:?}"));
                    }
                    for (pointer, code) in theirs {
                        let matched = issues.iter().any(|i| {
                            i.pointer == pointer && (code != "unrecognized_keys" || i.code == code)
                        });
                        if !matched {
                            failures.push(format!("{name}: TypeScript reports {pointer} ({code}); Rust reports {issues:?}"));
                        }
                    }
                }
                (expected, result) => failures.push(format!(
                    "{name}: TypeScript says {expected}, Rust returned {result:?}"
                )),
            }
        }
        assert!(
            failures.is_empty(),
            "{} disagreement(s) with TypeScript:
{}",
            failures.len(),
            failures.join(
                "
"
            )
        );
    }

    #[test]
    fn unknown_schema_is_reported_at_dollar_schema() {
        let issues = validate_record("https://nosh.dev/schemas/nope/v1", &Value::Null).unwrap_err();
        assert_eq!(
            issues,
            vec![Issue {
                pointer: "/$schema".into(),
                code: "unknown_schema".into(),
                message: "Unsupported schema: https://nosh.dev/schemas/nope/v1".into()
            }]
        );
    }

    #[test]
    fn only_known_formats_appear() {
        fn collect(node: &Value, out: &mut Vec<String>) {
            match node {
                Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
                Value::Object(map) => {
                    if let Some(Value::String(format)) = map.get("format") {
                        out.push(format.clone());
                    }
                    map.values().for_each(|child| collect(child, out));
                }
                _ => {}
            }
        }
        let mut formats = Vec::new();
        for (_, text) in SCHEMAS {
            collect(&serde_json::from_str(text).unwrap(), &mut formats);
        }
        formats.sort();
        formats.dedup();
        assert_eq!(
            formats,
            ["date", "date-time", "uri"],
            "a new format needs a zod-compatible check here"
        );
    }
}
