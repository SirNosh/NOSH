# Structured terminal output

Task-bound and runtime-scoped Pi turns end with one unfenced JSON object in final assistant text:

```json
{"$schema":"https://nosh.dev/schemas/terminal-output/v1","schemaVersion":1,"records":["complete typed record objects go here"]}
```

The placeholder above is illustrative, not a valid submission. Each record must be a complete schema-valid object. The host supplies the allowed record schemas and expected Episode type for the turn.

This is host-enforced final-text JSON, not provider-constrained decoding or a guarantee that a model produces valid JSON. Only a successful final provider stop is eligible. Tool-loop intermediates, deltas, cancelled, failed, and truncated turns are not proposals.

The envelope has exactly `$schema`, `schemaVersion`, and `records`. It permits one or two records, at most one terminal outcome plus one Episode draft, and at most 131,072 UTF-8 bytes. Duplicate keys, fenced JSON, prose, unknown envelope fields, disallowed schemas, and wrong Episode types are rejected. A task requires one terminal outcome; a runtime step requires one Episode draft. Zod and host authorization remain authoritative beyond envelope parsing.

The host binds routing to the active agent/task/instruction/thread/version. Parsed output is a proposal, not proof of acceptance or completed effects. Host receipts and durable state determine the result. One correction attempt is available only when the host permits retry. Correction instructions say not to repeat execution tools; this is not a rollback of earlier tool effects.

For scoped turns, final-text records replace `nosh_response_submit`, `nosh_review_submit`, and `nosh_episode_submit` tool calls. Execution tools and immediate acknowledgement, progress, and effect commands remain capability-scoped tools. Unscoped normal research chat retains its tools and prose; it is not forced through this terminal envelope.

Real-provider and crash/recovery acceptance still need separate evidence. Automated parser and host tests are not provider-decoding guarantees.
