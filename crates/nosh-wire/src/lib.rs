//! NOSH wire formats: IDs, timestamps, canonical JSON hashing, and record validation.
//!
//! Compatible with the TypeScript `@nosh/core` and `@nosh/wire` packages. Records are validated
//! against the JSON Schemas generated from the TypeScript zod schemas (vendored under
//! `conformance/schemas`), plus the zod refinements, defaults, and string rules those JSON Schemas
//! cannot express.

mod budget;
mod events;
mod ids;
pub mod js;
mod json;
mod refinements;
mod registry;
mod submissions;
mod time;

pub use budget::{budget_tokens, event_budget_tokens};
pub use events::{EVENT_SCHEMA, validate_event, validate_event_draft};
pub use ids::{IdPrefix, create_id, is_nosh_id, random_hex};
pub use json::{canonical_json, normalize_numbers, sha256};
pub use registry::{Issue, schema_uris, validate_record};
pub use submissions::{
    episode_type_for_role, is_task_terminal_record, schema_document_path, schema_uri,
    submission_tool_schemas,
};
pub use time::{iso_from_ms, now_utc};
