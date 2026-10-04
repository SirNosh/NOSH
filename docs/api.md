# Local HTTP API

`noshd` serves a JSON API on loopback only, by default `http://127.0.0.1:4321` (port from the per-user `config.json`). Paths work with or without the `/api` prefix; the CLI and TUI use `/api`. This reference lists routes in `apps/noshd/src/server.ts`. Request and record schemas remain authoritative in `packages/wire`; validation errors name the failing fields.

## Authentication and limits

- A request is trusted automatically when its socket is loopback, its `Host` header is `localhost`, `127.x.x.x`, or `[::1]`, and it either has no `Origin` (native clients such as the CLI) or an `Origin` equal to that loopback host. Cross-origin and DNS-rebinding requests are not trusted.
- Otherwise send `Authorization: Bearer <session>`. `POST /session` with `Authorization: Bearer <bootstrap capability>` returns a session token valid for 15 minutes. The TUI uses this flow.
- `GET /health` needs no authentication. Unauthorized requests return `401 {"error":"unauthorized"}`; unknown routes `404`; invalid requests `400 {"error":"…"}`.
- Request bodies are limited to 3 MB. Mutations take an `idempotencyKey`; version-checked mutations take `expectedVersion` (or the named expected-version field). Do not blindly retry an ambiguous failed mutation; inspect state first.
- Trusted callers have full local authority, including launching Jobs and agents. Never expose the port beyond loopback.

## Events

| Method | Path | Parameters | Purpose |
|---|---|---|---|
| GET | `/events` | `projectId`, `after` (cursor ≥ 0), `limit` (1–1000, default 300), `recent=true` (only with `after=0`) | Page of durable events after a cursor; `recent` returns the latest page. |
| WebSocket | `/events` | `projectId`, `after`; session token as subprotocol `auth.<token>` when not automatically trusted | Durable events from the cursor, then live events, including transient text/tool deltas. Slow consumers are closed (code 1013) and must reconnect from their last durable cursor. Transient events are not replayable. |

## Daemon, Projects, and chat

| Method | Path | Required | Purpose |
|---|---|---|---|
| GET | `/health` | — | Liveness. |
| POST | `/session` | bootstrap bearer | Issue a 15-minute session token. |
| POST | `/shutdown` | — | Stop the daemon (refused while a backup runs). |
| GET | `/projects` | — | Registered Projects. |
| POST | `/projects` | registration record | Register an initialized Project (used by `nosh project open`). |
| POST | `/projects/open` | `path`, `workingTitle`, `createRepository`; optional `githubRepositoryUrl`, `model`, `thinkingLevel` | Open or create a repository and start intake. |
| GET | `/projects/:id/contract` | — | Active Project contract. |
| GET | `/models` | — | Authenticated Pi models and advertised thinking levels. |
| POST | `/chat` | `projectId`, `message`, `idempotencyKey`; optional `model`, `thinkingLevel` | Send a message to the Project's Nosh agent. |
| GET | `/agents` | optional `projectId` | Agent inspection. |
| POST | `/agents` | Pi session options | Start an agent. |
| POST | `/agents/:id/{prompt,steer,compact,abort,stop}` | `message` for prompt/steer; optional `instructions` for compact | Agent control. |

## Missions, Directions, Autoresearch

All `GET` routes require `projectId`; all `POST` routes require `projectId` and `idempotencyKey`.

| Method | Path | Additional required fields | Purpose |
|---|---|---|---|
| POST | `/projects/:id/contract/amend` | `contract`, `idempotencyKey` | User-only: write the next approved Project contract version (e.g. `execution.commands` for `nosh_run`). |
| GET/POST | `/missions` | `title`, `objective`, `deliverables[]`, `successCriteria[]`; optional `nonObjectives[]`, `startingEvidence[]` (IDs of existing Evidence records, not prose) | List or create Missions. |
| GET | `/missions/:id` | — | Mission projection. |
| GET | `/missions/:id/completion` | — | Completion basis and latest completion packet. |
| POST | `/missions/:id/steer` | `expectedVersion`, `message` | Steer a running Mission. |
| POST | `/missions/:id/control` | `expectedVersion`, `action` (`pause`/`resume`/`stop`), `mode` (`safe`/`checkpoint`/`immediate`) | Mission control. |
| POST | `/missions/:id/transition` | `expectedVersion`, `next` | State transition. |
| POST | `/missions/:id/graph` | `expectedVersion`, `baseGraphVersion`, `operations`, `rationale` | Propose a graph change. |
| POST | `/missions/:id/nodes/:node/transition` | `expectedVersion`, `next` | Graph node transition. |
| GET | `/graph-proposals` | — | Graph proposals. |
| POST | `/graph-proposals/:id/approve` | `expectedProposalVersion` | Approve an inspected proposal. |
| GET/POST | `/directions` | `question`, `decisionUse` | List or create Directions. |
| POST | `/directions/:id/transition` | `expectedVersion`, `next` | State transition. |
| POST | `/directions/:id/baseline` | `expectedVersion`, `commit`, `reviewId`, `evaluationContractHash` | Accept a reviewed baseline. |
| POST | `/directions/:id/nodes/:node/transition` | `expectedVersion`, `next` | Node transition. |
| GET/POST | `/autoresearch` | `decisionQuestion` | List or create Autoresearch executions. |
| POST | `/autoresearch/:id/transition` | `expectedVersion`, `next` | State transition. |

## Records, evidence, paper, and notifications

| Method | Path | Required | Purpose |
|---|---|---|---|
| GET | `/records` | `projectId`; optional `schema` | Submitted records. |
| GET | `/reviews` | `projectId` | Reviews. |
| GET/POST | `/evidence` | `projectId`; POST: `evidence` object, `idempotencyKey` | Evidence records. |
| GET/POST | `/claims` | `projectId`; POST: `claim` object, `idempotencyKey` | Claims. |
| PUT | `/claims/:id` | `projectId`, `expectedClaimVersion`, `changes`, `idempotencyKey` | Update a claim. |
| GET/PUT | `/paper` | `projectId`; PUT: `markdown`, `bibliography`, `expected` hashes, `idempotencyKey` | Read or replace paper sources. |
| POST | `/paper/export` | `projectId` | Deterministic export. |
| GET | `/notification-acks` | `projectId` | Acknowledged event IDs. |
| POST | `/notifications/acknowledge` | `projectId`, `eventIds`, `idempotencyKey` | Acknowledge notifications. |

## Jobs and backups

| Method | Path | Required | Purpose |
|---|---|---|---|
| GET | `/jobs` | optional `projectId` | Jobs (all Projects when omitted). |
| POST | `/jobs` | Job spec | Launch a supervised Job. |
| GET | `/jobs/:id` | `projectId` | Job record and resource snapshot. |
| GET | `/jobs/:id/tail` | `projectId`; optional `stream` (`stdout`/`stderr`) | Bounded log tail. |
| POST | `/jobs/:id/{checkpoint,cancel}` | `projectId`, `idempotencyKey` | Job control. |
| GET | `/backups` | `projectId` | Verified backups for the Project. |
| POST | `/backups` | `projectId` | Create a backup (clean Git tree and no non-terminal work required). |
| POST | `/backups/:id/restore` | `projectId` | Schedule restore and shut down; applied on next start. |

## Orchestration runtime

| Method | Path | Required | Purpose |
|---|---|---|---|
| POST | `/runtime/instructions` | runtime instruction record | Execute a typed instruction. See [orchestration runtime](ORCHESTRATION_RUNTIME.md). |
| GET | `/threads`, `/threads/:id` | `projectId` | Execution threads. |
| POST | `/threads/:id/rotate` | `projectId`, `idempotencyKey` | Rotate the Pi session. |
| POST | `/threads/:id/messages` | `projectId`, `message`, `idempotencyKey` | Message a foreground fork. |
| GET | `/episodes`, `/episodes/:id/trace` | `projectId` | Episodes and their event traces. |
| GET/POST | `/skills` | `projectId`; POST: `manifest`, `idempotencyKey` | Skill manifests. |
| GET/POST | `/programs` | `projectId`; POST: `program`, `idempotencyKey` | Orchestration programs. |
| GET | `/programs/states` | `projectId` | Program states. |
| POST | `/programs/:id/run` | `projectId` | Run a program. |
