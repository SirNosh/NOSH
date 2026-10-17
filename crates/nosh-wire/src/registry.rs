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
    Draft, Keyword, PatternOptions, ValidationError, Validator,
    error::{TypeKind, ValidationErrorKind},
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

struct Compiled {
    name: String,
    validator: Validator,
    /// The adapted schema document, for resolving the schema of a reported location.
    document: Value,
}

/// Schema URI -> compiled schema.
static VALIDATORS: LazyLock<BTreeMap<String, Compiled>> = LazyLock::new(|| {
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
        validators.insert(
            uri,
            Compiled {
                name: (*name).to_owned(),
                validator,
                document,
            },
        );
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
    let Some(Compiled {
        name,
        validator,
        document,
    }) = VALIDATORS.get(uri)
    else {
        return Err(vec![Issue {
            pointer: "/$schema".into(),
            code: "unknown_schema".into(),
            message: format!("Unsupported schema: {uri}"),
        }]);
    };
    let mut issues: Vec<Issue> = validator
        .iter_errors(value)
        .flat_map(|error| issues_from(&error, document))
        .collect();
    // zod applies defaults while parsing, so refinements see them.
    let mut value = value.clone();
    crate::json::normalize_numbers(&mut value);
    EXTRAS.apply_defaults(name, &mut value);
    // A refinement still runs when the value only has "dirty" issues (failed string, size, or
    // key checks); an aborting issue at or below it (a wrong type, literal, enum, or union, or a
    // missing key) stops it, as in zod.
    let aborted: Vec<&str> = issues
        .iter()
        .filter(|issue| {
            !matches!(
                issue.code.as_str(),
                "too_small" | "too_big" | "invalid_string" | "unrecognized_keys" | "custom"
            )
        })
        .map(|issue| issue.pointer.as_str())
        .collect();
    let refinement_issues = EXTRAS.check(name, &value, &aborted);
    issues.extend(refinement_issues);
    if issues.is_empty() {
        Ok(value)
    } else {
        Err(issues)
    }
}

/// zod's issue for a JSON Schema error: the same codes and messages TypeScript's `validateRecord`
/// returns, which agents read to correct a submission.
fn issues_from(error: &ValidationError<'_>, document: &Value) -> Vec<Issue> {
    let base = error.instance_path().as_str();
    let instance: &Value = error.instance();
    let issue = |pointer: String, code: &str, message: String| Issue {
        pointer,
        code: code.into(),
        message,
    };
    let here = |code: &str, message: String| vec![issue(base.to_owned(), code, message)];
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
            let pointer = format!("{base}/{}", escape_pointer(key));
            let schema = error
                .schema_path()
                .as_str()
                .strip_suffix("/required")
                .and_then(|parent| {
                    document.pointer(&format!("{parent}/properties/{}", escape_pointer(key)))
                });
            match schema.map_or(Expected::Union, |schema| expected(document, schema)) {
                Expected::Literal(value) => vec![issue(
                    pointer,
                    "invalid_literal",
                    format!(
                        "Invalid literal value, expected {}",
                        crate::canonical_json(&value)
                    ),
                )],
                Expected::Enum(options) => vec![issue(
                    pointer,
                    "invalid_type",
                    format!("Required: missing; expected {}", join_values(&options)),
                )],
                Expected::Type(name) => vec![issue(
                    pointer,
                    "invalid_type",
                    format!("Required: missing; expected {name}"),
                )],
                Expected::Union => vec![issue(pointer, "invalid_union", "Invalid input".into())],
            }
        }
        ValidationErrorKind::AnyOf { context } => union_issues(error, base, context, document),
        ValidationErrorKind::Type { kind } => {
            // z.literal() and z.enum() check the value in one step: their own issue reports it.
            let sibling = error
                .schema_path()
                .as_str()
                .strip_suffix("/type")
                .and_then(|location| document.pointer(location));
            if sibling
                .is_some_and(|schema| schema.get("const").is_some() || schema.get("enum").is_some())
            {
                return Vec::new();
            }
            let expected = match kind {
                TypeKind::Single(single) => single.to_string(),
                TypeKind::Multiple(_) => return here("invalid_type", error.to_string()),
            };
            // z.number().int(): a string fails as "number", a fraction as "integer"/"float".
            let expected = if expected == "integer" && !instance.is_number() {
                "number".to_owned()
            } else {
                expected
            };
            here(
                "invalid_type",
                format!(
                    "Expected {expected}, received {}",
                    received_type(instance, &expected)
                ),
            )
        }
        ValidationErrorKind::Constant { expected_value } => here(
            "invalid_literal",
            format!(
                "Invalid literal value, expected {}",
                crate::canonical_json(expected_value)
            ),
        ),
        ValidationErrorKind::Enum { options } => {
            let options = options.as_array().cloned().unwrap_or_default();
            match instance.as_str() {
                Some(received) => here(
                    "invalid_enum_value",
                    format!(
                        "Invalid enum value. Expected {}, received '{received}'",
                        join_values(&options)
                    ),
                ),
                None => here(
                    "invalid_type",
                    format!(
                        "Expected {}, received {}",
                        join_values(&options),
                        received_type(instance, "")
                    ),
                ),
            }
        }
        ValidationErrorKind::Pattern { .. } => here("invalid_string", "Invalid".into()),
        ValidationErrorKind::Format { format } => here(
            "invalid_string",
            match format.as_str() {
                "date-time" => "Invalid datetime",
                "date" => "Invalid date",
                "uri" => "Invalid url",
                _ => "Invalid",
            }
            .into(),
        ),
        ValidationErrorKind::MinItems { limit } => here(
            "too_small",
            format!("Array must contain at least {limit} element(s)"),
        ),
        ValidationErrorKind::MaxItems { limit } => here(
            "too_big",
            format!("Array must contain at most {limit} element(s)"),
        ),
        ValidationErrorKind::Minimum { limit } => here(
            "too_small",
            format!(
                "Number must be greater than or equal to {}",
                crate::js::string(Some(limit))
            ),
        ),
        ValidationErrorKind::ExclusiveMinimum { limit } => here(
            "too_small",
            format!(
                "Number must be greater than {}",
                crate::js::string(Some(limit))
            ),
        ),
        ValidationErrorKind::Maximum { limit } => here(
            "too_big",
            format!(
                "Number must be less than or equal to {}",
                crate::js::string(Some(limit))
            ),
        ),
        ValidationErrorKind::ExclusiveMaximum { limit } => here(
            "too_big",
            format!(
                "Number must be less than {}",
                crate::js::string(Some(limit))
            ),
        ),
        ValidationErrorKind::Custom { .. } if error.schema_path().as_str().ends_with(MIN_UTF16) => {
            here("too_small", error.to_string())
        }
        ValidationErrorKind::Custom { .. } if error.schema_path().as_str().ends_with(MAX_UTF16) => {
            here("too_big", error.to_string())
        }
        // z.record's key schema: the offending key is the issue's last path segment.
        ValidationErrorKind::PropertyNames { error: inner } => {
            let key = inner.instance().as_str().unwrap_or_default().to_owned();
            vec![issue(
                format!("{base}/{}", escape_pointer(&key)),
                "invalid_string",
                "Invalid".into(),
            )]
        }
        _ => here("invalid", error.to_string()),
    }
}

/// What zod expects at a schema location, for reporting a missing key.
enum Expected {
    Literal(Value),
    Enum(Vec<Value>),
    Type(String),
    Union,
}

fn expected(document: &Value, schema: &Value) -> Expected {
    let schema = resolve_ref(document, schema);
    if let Some(value) = schema.get("const") {
        return Expected::Literal(value.clone());
    }
    if let Some(Value::Array(options)) = schema.get("enum") {
        return Expected::Enum(options.clone());
    }
    let is_type = |branch: &Value, name: &str| {
        resolve_ref(document, branch)
            .get("type")
            .and_then(Value::as_str)
            == Some(name)
    };
    if let Some(Value::Array(branches)) = schema.get("anyOf") {
        // `.nullable()` is not a union in zod: the inner schema reports.
        let inner: Vec<&Value> = branches
            .iter()
            .filter(|branch| !is_type(branch, "null"))
            .collect();
        if let [only] = inner.as_slice() {
            return expected(document, only);
        }
        // A discriminated union first requires an object.
        if !inner.is_empty() && inner.iter().all(|branch| is_type(branch, "object")) {
            return Expected::Type("object".into());
        }
        return Expected::Union;
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("integer") => Expected::Type("number".into()),
        Some(kind) => Expected::Type(kind.to_owned()),
        None => Expected::Union,
    }
}

/// Follows a local `$ref` (`#/...`).
fn resolve_ref<'a>(document: &'a Value, schema: &'a Value) -> &'a Value {
    match schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
    {
        Some(pointer) => document
            .pointer(pointer)
            .map_or(schema, |target| resolve_ref(document, target)),
        None => schema,
    }
}

/// zod's `getParsedType` name for a JSON value; a fraction where an integer is expected is "float".
fn received_type(value: &Value, expected: &str) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Number(n)
            if expected == "integer" && n.as_f64().is_some_and(|f| f.fract() != 0.0) =>
        {
            "float"
        }
        Value::Number(_) => "number",
    }
}

/// zod's `util.joinValues`: strings quoted with `'`, joined by ` | `.
fn join_values(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| {
            if let Value::String(text) = value {
                format!("'{text}'")
            } else {
                crate::js::string(Some(value))
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
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
    document: &Value,
) -> Vec<Issue> {
    // `.nullable()`: the schema's non-null branch reports, whatever its errors are.
    let schemas = document
        .pointer(error.schema_path().as_str())
        .and_then(Value::as_array);
    let non_null: Vec<&Vec<ValidationError<'static>>> = branches
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            schemas.and_then(|s| s.get(*index)).is_none_or(|branch| {
                resolve_ref(document, branch)
                    .get("type")
                    .and_then(Value::as_str)
                    != Some("null")
            })
        })
        .map(|(_, errors)| errors)
        .collect();
    if non_null.len() == 1 && branches.len() == 2 {
        return non_null[0]
            .iter()
            .flat_map(|e| issues_from(e, document))
            .collect();
    }
    // A type mismatch at the union itself rules a branch out.
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
        return branch
            .iter()
            .flat_map(|e| issues_from(e, document))
            .collect();
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
        // zod lists every branch's discriminator value.
        let options: Vec<Value> = document
            .pointer(error.schema_path().as_str())
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|branch| {
                resolve_ref(document, branch)
                    .get("properties")?
                    .get(key.as_str())?
                    .get("const")
                    .cloned()
            })
            .collect();
        return vec![Issue {
            pointer: format!("{base}/{key}"),
            code: "invalid_union_discriminator".into(),
            message: format!(
                "Invalid discriminator value. Expected {}",
                join_values(&options)
            ),
        }];
    }
    let selected: Vec<_> = candidates
        .iter()
        .zip(&rejected)
        .filter(|(_, keys)| keys.is_empty())
        .map(|(branch, _)| branch)
        .collect();
    if let [branch] = selected.as_slice() {
        return branch
            .iter()
            .flat_map(|e| issues_from(e, document))
            .collect();
    }
    vec![Issue {
        pointer: base.to_owned(),
        code: "invalid_union".into(),
        message: "Invalid input".into(),
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
    /// disagreement at once. Rejections must report the same issues: pointer, zod code, and
    /// message (as a set; zod and JSON Schema visit keys in different orders).
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
            let mut theirs: Vec<(String, String, String)> = case["issues"]
                .as_array()
                .map(|issues| {
                    issues
                        .iter()
                        .map(|i| {
                            (
                                i["pointer"].as_str().unwrap().to_owned(),
                                i["code"].as_str().unwrap().to_owned(),
                                i["message"].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            theirs.sort();
            match (case["outcome"].as_str().unwrap(), &result) {
                ("accept", Ok(value)) if value != &case["value"] => {
                    failures.push(format!("{name}: parsed value differs: {value}"))
                }
                ("accept", Ok(_)) => {}
                ("custom" | "reject", Err(issues)) => {
                    let mut ours: Vec<(String, String, String)> = issues
                        .iter()
                        .map(|i| (i.pointer.clone(), i.code.clone(), i.message.clone()))
                        .collect();
                    ours.sort();
                    if ours != theirs {
                        failures.push(format!(
                            "{name}:
      TypeScript {theirs:?}
      Rust       {ours:?}"
                        ));
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
