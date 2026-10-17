//! NOSH wire formats: IDs, timestamps, canonical JSON hashing, and record validation.
//!
//! Compatible with the TypeScript `@nosh/core` and `@nosh/wire` packages. Records are validated
//! against the JSON Schemas generated from the TypeScript zod schemas (vendored under
//! `conformance/schemas`), plus the zod refinements, defaults, and string rules those JSON Schemas
//! cannot express.

mod events;
mod ids;
mod json;
mod refinements;
mod registry;
mod submissions;
mod time;

pub use events::{EVENT_SCHEMA, validate_event, validate_event_draft};
pub use ids::{IdPrefix, create_id, is_nosh_id};
pub use json::{canonical_json, sha256};
pub use registry::{Issue, schema_uris, validate_record};
pub use submissions::is_task_terminal_record;
pub use time::now_utc;
