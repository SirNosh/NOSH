# NOSH Research

NOSH (Networked Orchestrated Science Harness) is a local-first research operating environment for one Project at a time, one evidence-linked paper per Project, and one daemon-owned source of truth.

It is built around a simple split:

- `noshd` owns durable state, graphs, jobs, and semantic commands.
- the web app is the control surface for chat, Missions, inspection, and the workbench.
- Pi is the only model runtime.
- Git holds reviewable project content.
- SQLite holds live operational state.

If you have downloaded the packaged release from npm or from a release archive, you can use the shipped `release/NOSH-Research-0.1.0` directory as a local test target. It contains the same package layout as the built distribution.

## Local setup

From the repository root:

```powershell
corepack pnpm install --frozen-lockfile
corepack pnpm build
corepack pnpm test
```

To exercise the shipped package locally after downloading it:

```powershell
cd release\NOSH-Research-0.1.0
corepack pnpm install --frozen-lockfile
corepack pnpm build
```

On Windows, the usual runtime path is:

```powershell
nosh setup
nosh doctor
nosh start
nosh open
```

For a fresh Project, point NOSH at a Git repository:

```powershell
nosh project open C:\path\to\repo
```

## Start here if NOSH is new to you

NOSH is a research workspace, not a general chatbot. It keeps the research question, agent work, code, experiments, reviews, evidence, and paper attached to one durable Project. You talk to Nosh in the web app; the local `noshd` service keeps working state and supervised jobs alive even when the browser closes.

The four ideas to understand first are:

1. **Project:** one Git repository, one research contract, and one evidence-linked paper workspace.
2. **Normal mode:** ordinary user-directed research chat. You decide what happens next.
3. **Mission mode:** autonomous, graph-backed work with explicit approval, budgets, workers, Reviews, Pause, and Stop.
4. **Research Direction and Autoresearch:** a Direction resolves one bounded scientific question; Autoresearch runs a controlled family of experiments inside a frozen evaluation contract.

If you only want to explore an idea, start in Normal mode. If the question becomes experimentally testable, create a Direction. Use Autoresearch when repeated comparable experiments are warranted. Use a Mission when several dependent research activities should proceed autonomously toward explicit completion criteria.

## Complete user workflow

### 1. Install and check the workstation

Run:

```powershell
nosh setup
nosh doctor
nosh start
nosh status
nosh open
```

What these commands do:

- `setup` creates the per-user NOSH state directory and local bootstrap credential;
- `doctor` checks Windows, Git, WSL2, Pi, model authentication, storage, and optional GPU support;
- `start` launches `noshd` as a hidden per-user background process;
- `status` confirms the daemon address;
- `open` opens the web app.

If the GUI asks for the loopback bootstrap token, copy the `bootstrapToken` value from `%LOCALAPPDATA%\NOSH\config.json` into Settings once. NOSH exchanges it for a short-lived token scoped to that browser tab. Do not share it.

### 2. Create or open a Project

Choose **New Project** in the left sidebar and provide:

- a working title;
- the local repository path, typed or selected with **Browse**;
- whether NOSH should create the directory and initialize Git;
- an optional `https://github.com/owner/repository` link;
- the default Pi model.

For an existing repository, select its Git root—not a subdirectory. The optional GitHub link becomes the Git `origin`; NOSH rejects a conflicting existing `origin` instead of silently replacing it.

At this point NOSH creates the paper workspace, `.nosh` metadata, schema lock, operational database registration, and an **unapproved draft** Project contract. It deliberately does not guess the north-star question or compute budget.

### 3. Complete Project discovery

The main chat begins a one-question-at-a-time discovery session. Explain the idea in your own words. Nosh will clarify and challenge:

- the actual problem and motivation;
- the north-star research question;
- what decision the answer should support;
- the intended contribution and what would make it novel;
- included scope, exclusions, and non-goals;
- datasets, licenses, privacy, and network constraints;
- baselines, primary metrics, guardrails, and falsification conditions;
- reproducibility expectations;
- publication or venue expectations;
- realistic GPU-hours, disk usage, and allowed hardware.

Nosh periodically reflects its understanding. Correct it when the spirit is wrong even if the wording sounds plausible. When the contract is complete, Nosh presents the proposed contract and asks for explicit approval. Approval creates the next immutable contract version; until then it remains a draft.

### 4. Work in Normal mode

Normal mode is the default. Use the composer to ask questions, request a literature search, inspect the repository, synthesize evidence, or delegate a bounded task. The composer also shows:

- the selected Pi model;
- the active Pi session's context fill;
- Normal/Mission mode;
- Send or Stop for the current turn.

Stopping a chat turn does not stop a Mission, child agent, or supervised Job. Those objects have their own controls.

When Nosh delegates work, open the task or Agent in the right inspector to see its role, bounded task, current operation, elapsed time, context use, changed files, attached Jobs, and handoff state. Hidden chain-of-thought is never displayed.

### 5. Create a Research Direction

Create a Direction when you can state one bounded question whose answer will change a research or paper decision. Define its evaluation contract: datasets, metric, objective, baseline, meaningful-effect threshold, seeds, runner, command, result path, timeout, and GPU use.

A Direction progresses through proposal, activation, work, Review, and closure. Its evaluation contract is frozen so later results remain comparable. A Direction-backed Autoresearch execution cannot begin until the immutable baseline has passed independent Review.

Use the Direction view to inspect:

- the question and decision use;
- internal graph and current frontier;
- frozen evaluation-contract hash;
- accepted baseline commit;
- related Autoresearch executions;
- evidence and failed hypotheses;
- closure readiness and Review defects.

### 6. Run Autoresearch

Use Autoresearch for a narrow hypothesis family that benefits from repeated, controlled variation. Set the writable scope and limits for experiments, rounds, wall-clock time, model tokens, GPU time, and disk.

NOSH then:

1. proposes a child experiment against an accepted parent;
2. creates an isolated Git worktree and immutable evaluated commit;
3. launches a supervised Run and Job under the frozen contract;
4. collects metrics, logs, configuration, and environment artifacts;
5. compares the child with its parent using the declared keep rule;
6. requests independent Review;
7. promotes, rejects, or records the result as inconclusive;
8. advances the accepted frontier until a stop rule or budget is reached.

Failed experiments are retained as negative evidence. They are not erased merely because they did not improve the metric.

### 7. Create and operate a Mission

Use Mission mode when the work has several dependent activities and clear completion criteria. Create a draft with a title, objective, deliverables, deterministic success criteria, non-objectives, and starting assumptions or evidence.

The lifecycle is deliberate:

1. **Draft:** the Mission contract exists but cannot run.
2. **Planning:** the Director prepares the graph.
3. **Awaiting approval:** inspect the objective, graph, criteria, and budgets.
4. **Running:** the Mission Director leases ready nodes to bounded workers.
5. **Reviewing or Blocked:** defects, missing evidence, policy issues, or failed gates require attention.
6. **Completed:** deterministic completion gates and fresh independent final Review have passed.

Mission mode provides:

- **Board:** Ready, Running, Review, Blocked, and Done nodes;
- **Graph:** dependencies, selected nodes, and graph versions;
- **Timeline:** typed lifecycle, agent, job, Review, steering, and budget events;
- **Completion:** criteria, evidence, defects, limitations, budget reconciliation, and final Review;
- **Mission Chat:** instructions and steering directed to the Mission context.

Switching back to Normal mode does not pause the Mission. **Pause** prevents new work from starting and coordinates safe agent/job boundaries. **Stop** is terminal for that Mission but retains accepted evidence and artifacts.

### 8. Observe Agents, execution, and problems

Use the right inspector for the selected Mission, node, Direction, Agent, Run, Job, or Review. Use the bottom workbench for:

- **Terminal:** a local PowerShell or WSL2 shell in the active Project;
- **Logs:** daemon, Agent, validator, and Job output;
- **Problems:** failed deterministic checks, unresolved defects, and environment problems;
- **Jobs:** durable process state, command, elapsed time, metrics, checkpoints, and outcome.

Use the terminal for short interactive commands. Use supervised Jobs for training, evaluation, or any process that must survive browser closure and retain an execution manifest.

### 9. Turn results into evidence and claims

An Artifact is a content-addressed output such as a report, configuration, log, metric file, figure, diff, checkpoint, or synthesis. Evidence gives an Artifact or external source a precise research meaning. Claims connect supported, contradicting, or qualifying Evidence to statements intended for the paper.

Do not treat Agent prose as accepted evidence. A result becomes trustworthy through exact source references, deterministic checks, immutable commits where applicable, and independent Review. Unsupported claims must remain hypotheses or carry an explicit limitation.

### 10. Write and export the paper

Open **Paper** to edit the canonical Markdown source. NOSH keeps the paper in the Project repository and can export reproducible LaTeX and, when the local toolchain supports it, PDF.

Before final export, confirm that:

- every material claim resolves to Evidence or an explicit limitation;
- figures and tables resolve to stored Artifacts and provenance;
- evaluated results identify their exact commit and environment;
- contradicting and negative Evidence is not hidden;
- required Reviews and completion gates have passed.

### 11. Pause, close, recover, and work remotely

Closing the browser leaves `noshd`, Missions, and supervised Jobs running. Reopen the app to restore the Project view from durable state. After daemon restart, NOSH reconciles Jobs, pending external operations, program/thread state, and interrupted commands without guessing unknown outcomes.

Remote devices receive encrypted Project state and approved semantic controls such as inspect, steer, pause, checkpoint, cancel, or stop. They do not receive an unrestricted shell by default.

### 12. Back up and shut down

Create a backup before moving machines, major Git surgery, or destructive cleanup:

```powershell
nosh backup <project-id-or-path>
```

The backup includes operational state, contracts, paper sources, required Git references, and integrity metadata. Use `nosh stop` only when you intend to stop the daemon; first inspect active Missions, Agents, and Jobs.

## What NOSH is for

NOSH is designed for long-lived research work where the question, the evidence, the execution history, and the final paper all need to stay tied together.

Typical things NOSH handles:

- normal research chat about a Project;
- autonomous Missions with a graph of work;
- live subagents and their tool use;
- experiments and runs;
- supervised jobs and terminal work;
- evidence, claims, and paper output;
- remote observation and semantic control.

## Architecture

The architecture is intentionally narrow:

1. `noshd` is the authority.
1. The GUI renders typed records from `@nosh/wire`.
1. The CLI sends admin commands to `noshd`.
1. Pi handles agent turns and directed work.
1. Jobs run outside the UI so they survive browser closure.
1. Git stores the paper, code, and reviewable projections.
1. SQLite stores mutable operational truth.
1. The relay carries encrypted remote traffic only.

That split keeps the UI honest. Closing the browser does not stop the daemon, and the daemon does not trust UI state as truth.

## Hierarchy

The product hierarchy is:

- Project
- Mission
- Research Direction
- Autoresearch Execution
- Experiment
- Run
- Job
- Agent
- Review
- Evidence
- Claim
- Artifact
- Paper section

The top-level object is the Project. Everything else hangs off a Project and is selected from it.

The ownership model is:

- Projects contain Missions, Directions, Autoresearch, runs, jobs, evidence, and paper content.
- Missions organize graph-backed work.
- Directions organize research questions and closure.
- Autoresearch organizes experiment families.
- Experiments resolve into runs and jobs.
- Jobs produce logs, metrics, checkpoints, and artifacts.
- Agents perform bounded work and report progress.
- Reviews validate outcomes.
- Evidence and claims support the paper.

## How it works

### Project

A Project is the unit of durable work. It is rooted in a Git repository and paired with a local operational database.

The web app asks only for a working title, repository path, optional GitHub repository link, and Pi model. A native Windows folder picker is available beside the path field. NOSH then creates an unapproved draft contract and begins a guided discovery chat. Nosh asks one focused question at a time to establish the north star, decision use, contribution, scope, datasets, evaluation, reproducibility, policies, GPU hours, and disk limit. It writes a new versioned contract only after the user explicitly approves the reflected understanding. A supplied GitHub link becomes the repository's `origin`; an existing conflicting `origin` is rejected.

Use it when you want a persistent research workspace with repeatable history and a paper trail.

Example:

You have a repository called `cancer-literature-review`. Open it as a Project so NOSH can track chat, evidence, jobs, and paper output against that repo.

### Local browser authentication

`nosh setup` creates a random loopback bootstrap token in the current Windows user's ACL-protected `%LOCALAPPDATA%\NOSH\config.json`. The token is not a GitHub or model-provider credential. It is a local capability used once to exchange for a 15-minute, tab-scoped session token; the browser then removes the bootstrap token from its storage. Never share it or expose the daemon beyond loopback.

### Normal mode

Normal mode is the default chat-first mode.

Use it when you want to:

- ask questions about the Project;
- start a Research Direction;
- start an Autoresearch Execution;
- inspect live work without switching into Mission control.

Example:

You ask for a literature scan on a new hypothesis. NOSH creates a bounded research task, shows the live subagent in the inspector, and returns a structured result when it finishes.

### Mission mode

Mission mode is the autonomous graph-control surface.

Use it when work needs:

- a Mission graph;
- explicit Pause and Stop controls;
- a Board, Graph, Timeline, and Completion view;
- clear live state for workers and reviewers.

Example:

You create a Mission to benchmark three model variants. NOSH schedules nodes, shows running and blocked work, and lets you inspect a worker or review a node without guessing what state it is in.

### Research Direction

A Direction is a focused research question with closure criteria.

Use it when the main job is to answer something, not to orchestrate a broad Mission.

Example:

You want to know whether a retrieval strategy improves answer quality. NOSH tracks the direction, evidence, failures, and final review until the question is resolved.

### Autoresearch

Autoresearch is the experiment engine.

Use it when you want repeated controlled runs around a narrow hypothesis family.

Example:

You compare prompt variants across seeds. NOSH creates an experiment tree, records the frontier, and keeps the accepted variant separate from the failed ones.

### Experiment, Run, and Job

These are the execution layers.

- An Experiment is the hypothesis container.
- A Run is one resolved execution of that experiment.
- A Job is the durable operating process behind the run.

Use them when you need reproducibility and recovery.

Example:

A training job crashes halfway through. NOSH preserves the job state, logs, and artifacts so you can see exactly what happened and whether it should be resumed or replaced.

### Agent

An Agent is a bounded worker or reviewer with a narrow task packet.

Use it when work should be delegated, observable, and finite.

Example:

A librarian agent gathers sources, a worker agent implements a change, and a reviewer agent checks the result before closure.

### Review

A Review is the deterministic validation layer.

Use it when a result needs a pass/fail judgment with defects or remediation notes.

Example:

After a run finishes, a reviewer records whether the output passes the acceptance criteria and what still needs repair.

### Evidence and Claims

Evidence is the source material. Claims are the statements supported by that material.

Use them when the paper needs traceability instead of plain prose.

Example:

You cite benchmark logs and a plotted result as evidence, then link those to a claim that the new method outperforms the baseline on the measured metric.

### Artifact and Paper

Artifacts are the stored outputs. The paper is the canonical Markdown workspace that turns the Project into a publishable narrative.

Use them when work needs to survive beyond a single session.

Example:

A generated figure, a diff, and a final synthesis note are all stored as artifacts, then pulled into the paper section that describes the experiment.

### Terminal and Jobs

The terminal is for local shell work. Jobs are for supervised execution that should outlive the browser.

Use the terminal for quick commands and the job system for anything that needs persistence, metrics, or restart behavior.

Example:

You run a one-off Git command in the terminal, then launch a supervised training job so the daemon can track it even if the browser closes.

### Remote control

Remote access is for observation and approved semantic commands, not arbitrary shell access.

Use it when you want to check status or steer work from another device.

Example:

You open NOSH from a laptop or phone, inspect a Mission, and issue a pause or stop command without exposing the local shell.

## Feature catalog

### Workspace and interaction

| Feature | What it does |
|---|---|
| Responsive web app and PWA | Uses one interface on the Windows host, laptop, tablet, or phone. Compact layouts preserve semantic controls without pretending a phone is a desktop terminal. |
| Project sidebar | Creates, filters, selects, and opens paper-scale Projects without mixing them with chats, agents, or files. |
| Normal and Mission modes | Keeps user-directed chat separate from autonomous graph execution. Switching views does not silently change lifecycle state. |
| Persistent layout | Remembers open panels, sizes, selected inspector/workbench tabs, Mission view, and split ratios per Project and mode. |
| Command palette and shortcuts | Provides keyboard access to navigation, panels, terminal focus, and common actions. |
| Flexoki dark interface | Uses a restrained, border-led dark visual system with purple for selection and semantic colors for state. Status never relies on color alone. |
| Context meter | Reports the active Pi session's context use and warns as compaction pressure rises. It is not a Mission budget or billing meter. |
| Pi model selector | Reads Pi's available model registry and chooses the model for the next compatible Normal-mode turn or new session. |
| Object references | Lets chat refer to Project objects such as Missions, Directions, experiments, Agents, Jobs, Evidence, Artifacts, and paper sections by stable identity. |
| Live event updates | Replays durable events by sequence and updates selected objects, Agents, Jobs, and graphs without treating chat prose as authority. |

### Project and research definition

| Feature | What it does |
|---|---|
| Project discovery | Conducts a focused interview to reach shared understanding of the letter and spirit of the research before approval. |
| Versioned Project contract | Stores the north star, decision use, contribution, scope, datasets, compute envelope, reproducibility, paper requirements, and policies. Drafts cannot masquerade as approved contracts. |
| Git-root enforcement | Requires existing Projects to be opened at the repository root and protects `.git` and `.nosh` from ordinary research tasks. |
| GitHub origin field | Optionally records a GitHub repository as `origin` while refusing to overwrite a conflicting remote. |
| Paper workspace initialization | Creates canonical Markdown, bibliography, figures, contract storage, schema lock, and Project metadata. |
| Research Direction | Tracks one bounded question, decision use, frozen evaluation contract, baseline, graph, frontier, failures, Evidence, and closure Review. |
| Frozen evaluation contract | Fixes datasets, metrics, baselines, seeds, execution command, timeout, and keep rule so later comparisons remain meaningful. |
| Baseline acceptance | Requires an immutable Git commit and exact independent PASS Review before Direction-backed Autoresearch. |

### Autonomous orchestration

| Feature | What it does |
|---|---|
| Mission contract | Defines objective, non-objectives, deliverables, success criteria, starting evidence, budgets, approval boundaries, and final Review requirements. |
| Mission Director | Reconciles stored graph state, criteria, budgets, blockers, and events before issuing ID-addressed work. |
| Mission DAG | Makes dependencies and graph versions explicit. Executable changes go through validated graph operations rather than UI-local dragging. |
| Kanban Board | Projects graph nodes into Ready, Running, Review, Blocked, and Done views. Dragging may reorder the display but cannot change execution. |
| Mission Timeline | Shows typed graph, Agent, Review, Job, lifecycle, budget, and steering events. |
| Completion gates | Requires node disposition, deterministic validators, evidence-linked claims, budget reconciliation, no unresolved blocking defects, and fresh independent final Review. |
| Pause | Stops new reasoning cycles and leases, coordinates safe Agent handoff, and offers finish/checkpoint/cancel policy for active Jobs. |
| Stop | Terminates the Mission's future work while retaining accepted Evidence, Artifacts, and history. Continuation requires a successor Mission. |
| Steering dispositions | Records whether an instruction was applied, partially applied, deferred, rejected, or needs clarification. A chat acknowledgement alone is not treated as applied state. |
| Stale-version protection | Rejects authoritative actions based on outdated graph or object versions and requires reconciliation. |

### Agents, threads, and Reviews

| Feature | What it does |
|---|---|
| Nosh | Runs the user-facing Project chat and may orchestrate bounded research in Normal mode. |
| Mission Director | Owns autonomous Mission reconciliation and graph-backed coordination. |
| Research Director | Owns one Direction and keeps work aligned with its frozen question and evaluation contract. |
| Librarian/Researcher | Finds, evaluates, and reports sources with precise locators and limitations. |
| General-Purpose Worker | Performs bounded implementation, analysis, or artifact-producing tasks inside the issued scope. |
| Reviewer | Independently evaluates a target against an exact Review Request and cannot edit the producer's result. |
| Live Agent inspector | Shows role, task, state, model, elapsed time, current tool, context use, files, Jobs, milestones, and controls before completion. |
| Safe steering and cancellation | Sends bounded instructions, requests an update, stops at a safe boundary, or explicitly aborts the selected Agent. |
| Execution threads | Represents logical long-lived work independently from disposable physical Pi sessions. |
| Foreground forks | Temporarily takes over the center workspace when a focused child thread requires direct user input, then restores the parent view. |
| Session rotation and handoff | Compacts completed steps into Episodes, rotates Pi sessions, and uses explicit handoff/teach-back before ownership transfer. |
| Structured responses | Validates acknowledgements, progress, blockers, completions, Reviews, Evidence, experiments, and runtime instructions against shared schemas. |
| One correction limit | Gives a malformed terminal submission one schema-only correction; a second invalid submission fails the attempt. |
| Independent Review | Binds producer, Reviewer, target version, criteria, required Artifacts/Evidence, findings, defects, and verdict into durable records. |
| Repair loop | Converts Review defects into bounded remediation with explicit acceptance tests instead of silently accepting partial work. |

### Autoresearch and execution

| Feature | What it does |
|---|---|
| Autoresearch Execution | Explores one distinct hypothesis family under fixed scope, evaluation, and resource limits. |
| Experiment lineage | Records parent-child hypotheses, branches, immutable commits, accepted frontier, rejected variants, and failed-attempt fingerprints. |
| Focus and repetition control | Detects repeated or non-progressing attempts and blocks uncontrolled search loops. |
| Git worktrees | Isolates concurrent experimental changes and freezes the exact commit that was evaluated. |
| Run manifest | Records the resolved command, working directory, seed, contract hash, commit, environment, inputs, outputs, and Job identity. |
| Job Supervisor | Owns long-running native or WSL2 processes independently of Agent turns and browser state. |
| Process recovery | Persists PID/fingerprint information, logs, state, and launch intent so daemon restart can reconcile execution safely. |
| Checkpoint and cancellation | Uses explicit Job controls when supported and records the resulting state. |
| Budget enforcement | Tracks experiments, rounds, wall time, model tokens, GPU time, disk, and concurrency at the relevant scope. |
| Parent comparison and keep rule | Compares a candidate only with its declared parent under the frozen metric and minimum-effect rule. |
| Negative-result retention | Keeps failed, invalid, contradicted, and non-improving outcomes as provenance and negative Evidence. |

### Evidence, artifacts, and paper

| Feature | What it does |
|---|---|
| Content-addressed Artifact store | Stores immutable outputs by hash and verifies that a resolved Artifact still matches its recorded content. |
| Evidence records | Attach a precise statement, polarity, source locator, evaluation contract, limitations, quality state, and Review to research support. |
| Claim graph | Connects claims to supporting, contradicting, and qualifying Evidence and audits unsupported or paper-unlinked claims. |
| Failure ledger | Prevents discarded hypotheses and repeated failed approaches from disappearing from Project memory. |
| Canonical paper editor | Edits repository-backed Markdown while keeping paper work attached to Project state. |
| Bibliography and figure provenance | Keeps citations and generated figures tied to exact sources and Artifacts. |
| LaTeX/PDF export | Produces reproducible LaTeX and optional PDF output with warnings when the local toolchain is incomplete. |
| Uniform outcome cards | Renders Outcome, Produced, Validation, Issues, and Next from validated records rather than parsing prose. |

### Local operations, recovery, and remote use

| Feature | What it does |
|---|---|
| Local terminal | Opens PowerShell or configured WSL2 in the active Project or worktree. Terminal closure does not silently kill supervised Jobs. |
| Logs, Problems, and Jobs workbench | Separates raw output, actionable deterministic failures, and durable process state from the chat timeline. |
| Daemon-owned persistence | Keeps operational truth in SQLite and Project records instead of trusting browser memory. |
| Idempotent commands | Prevents retried commands from applying the same state change twice with different meaning. |
| Operation intents | Records external actions such as Job launch or paper replacement so recovery can finish or report them after interruption. |
| Event replay | Restores state by persistent sequence, discards duplicates, and distinguishes stale or unknown state from failure. |
| Backup | Captures contracts, operational database, paper sources, integrity hashes, Git references, and required commits. |
| Encrypted remote control | Uses device keys and end-to-end encrypted relay frames for remote observation and capability-scoped semantic commands. |
| Device approval and revocation | Requires local approval to enroll a device and rotates the account key when a device is revoked. |
| Remote shell restriction | Exposes logs and typed controls remotely while keeping unrestricted shell access disabled by default. |
| Offline/stale presentation | Shows the last synchronized sequence without claiming that disconnected work stopped or succeeded. |
| Loopback authentication | Exchanges an ACL-protected bootstrap capability for a short-lived tab session so an arbitrary webpage cannot control localhost NOSH. |

## Main surfaces

### Web app

The web app is the primary interaction layer.

It provides:

- left Project navigation;
- central Normal or Mission workspace;
- right research inspector;
- bottom workbench with Terminal, Logs, Problems, and Jobs;
- a composer with model selection, mode switching, context meter, and Send/Stop.

Example:

You are reading a paper draft in Normal mode, then switch to Mission mode to inspect a blocked node and open the related agent in the inspector.

### CLI

The `nosh` CLI is for administration and Project setup.

Useful commands:

- `nosh setup`
- `nosh doctor`
- `nosh start`
- `nosh stop`
- `nosh status`
- `nosh open`
- `nosh project open <path>`
- `nosh project list`
- `nosh mission list`
- `nosh mission status <id>`
- `nosh job list`
- `nosh backup <project-id-or-path>`

Example:

You have a Git repo on disk. `nosh project open` registers it, creates the paper workspace, and makes it the current Project.

### Pi package

The Pi package contributes the NOSH roles, prompts, and tools for embedded agent work.

Use it when Pi needs the NOSH-specific control vocabulary.

Example:

The worker prompt tells Pi how to report progress, ask for delegation, and finish through the typed response path instead of free-form prose.

For deterministic fan-out, put bounded tasks in one JSON plan and run `pnpm delegate <plan.json>`. The script derives replay-stable IDs, opens workers together, starts their steps together, joins them through the existing typed runtime, and stops each worker at the boundary. Use `--dry-run` to inspect the instructions without contacting the daemon. `pnpm context:audit` reports conservative token estimates for the static Pi prompt resources.

## Example use cases

### 1. Starting a new Project

1. Start `noshd` and open the GUI.
1. Select **New Project**.
1. Choose an existing Git root or let NOSH initialize a new repository.
1. Complete the discovery conversation.
1. Correct Nosh's reflected understanding where necessary.
1. Explicitly approve the Project contract.

Result:

You get a persistent research workspace whose objective, scope, evaluation expectations, resource envelope, paper files, and local history were agreed rather than guessed.

### 2. Doing normal research chat

1. Stay in Normal mode.
1. Pick a Pi model.
1. Ask a question or request a literature scan.

Result:

NOSH can answer directly or delegate bounded work to a child agent and then render the outcome as structured state.

### 3. Running a Mission

1. Switch to Mission mode.
1. Create or resume a Mission.
1. Watch the board, graph, and timeline.
1. Inspect workers or reviewers from the right sidebar.

Result:

You can see exactly what is running, what is blocked, what is ready, and what still needs review.

### 4. Launching supervised execution

1. Open the bottom workbench.
1. Use the Terminal tab for the command.
1. Let NOSH track it as a durable Job.

Result:

The process survives browser closure and remains observable through logs, metrics, and job state.

### 5. Writing the paper

1. Gather evidence.
1. Promote claims from validated results.
1. Add artifacts and citations.
1. Export the paper when ready.

Result:

The final paper stays linked to the actual execution history that produced it.

### 6. Working remotely

1. Open NOSH from another device.
1. Inspect the current Project or Mission.
1. Issue semantic controls only.

Result:

You can stay informed and steer work without turning the remote surface into a general shell.

## Verification and release gates

The repo includes explicit checks in `docs/testing/release-gates.md`.

The main local smoke path is:

```powershell
corepack pnpm release:check
corepack pnpm schemas
corepack pnpm package:windows
corepack pnpm soak -- --seconds=172800
```

The 48-hour soak is a long-running stability gate. It is meant to prove that the GUI, daemon, jobs, graphs, and persistence stay healthy under real time, not just during a short test run.

## Documentation map

- [Architecture](ARCHITECTURE.md)
- [Windows installation](docs/user-guide/installation.md)
- [CLI reference](docs/user-guide/cli.md)
- [Remote setup](docs/user-guide/remote.md)
- [Release gates](docs/testing/release-gates.md)
- [Specification audit](docs/SPEC_AUDIT.md)
- [Web design audit](docs/WEB_DESIGN_AUDIT.md)
- [Orchestration runtime](docs/ORCHESTRATION_RUNTIME.md)

## Security

NOSH is local-first and security-sensitive.

- The loopback API is not exposed on LAN interfaces.
- Remote access uses a user-owned relay and encrypted traffic.
- The remote surface does not expose arbitrary shell execution.
- Windows install and uninstall are per-user and data-aware.

Before exposing anything beyond localhost, read:

- [SECURITY.md](SECURITY.md)
- [THREAT_MODEL.md](THREAT_MODEL.md)
- [docs/security/review.md](docs/security/review.md)

## License

NOSH is Apache-2.0. Production dependency notices are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
