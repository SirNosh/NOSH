//! Port of `apps/noshd/src/terminal-host.test.ts`: the durable terminal-turn journal over stub
//! hosts (the TypeScript tests' `Object.create(NoshDaemon.prototype)` daemons), and once over a
//! real daemon with only session liveness faked.

mod common;

use common::{FakeAgents, assert_match, daemon, inspection, local, schema_uri};
use futures_util::FutureExt;
use futures_util::future::LocalBoxFuture;
use nosh_agent::policy::Role;
use nosh_agent::terminal::TerminalContext;
use nosh_orchestration::daemon::terminal::{
    JournalHost, TurnLocks, admit_terminal, reject_terminal, submit_terminal,
};
use nosh_orchestration::{Error, Result};
use nosh_store::EventStore;
use nosh_wire::{IdPrefix, create_id};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

type Submit = Box<dyn FnMut(&str) -> Result<Value>>;
type Validate = Box<dyn FnMut() -> Result<()>>;

/// A daemon with only the journal's needs: liveness, validation, and record acceptance stubbed.
struct Stub {
    locks: TurnLocks,
    store: Rc<EventStore>,
    active: Rc<Cell<bool>>,
    validate: RefCell<Validate>,
    submit: RefCell<Submit>,
    calls: Cell<usize>,
    artifacts: Vec<String>,
}

fn host(store: &Rc<EventStore>) -> Stub {
    Stub {
        locks: TurnLocks::default(),
        store: Rc::clone(store),
        active: Rc::new(Cell::new(true)),
        validate: RefCell::new(Box::new(|| Ok(()))),
        submit: RefCell::new(Box::new(|_| Ok(json!({ "accepted": true })))),
        calls: Cell::new(0),
        artifacts: Vec::new(),
    }
}

fn refusing(store: &Rc<EventStore>, message: &'static str) -> Stub {
    let stub = host(store);
    *stub.validate.borrow_mut() = Box::new(move || Err(Error::Rejected(message.into())));
    stub
}

impl JournalHost for Stub {
    fn turn_locks(&self) -> &TurnLocks {
        &self.locks
    }
    fn journal_store(&self, _project_id: &str) -> Result<Rc<EventStore>> {
        Ok(Rc::clone(&self.store))
    }
    fn turn_active(&self, _context: &TerminalContext) -> bool {
        self.active.get()
    }
    fn validate_terminal(&self, _context: &TerminalContext, _records: &[Value]) -> Result<()> {
        (self.validate.borrow_mut())()
    }
    fn with_daemon_facts(
        &self,
        _context: &TerminalContext,
        records: Vec<Value>,
    ) -> Result<Vec<Value>> {
        Ok(records)
    }
    fn submit_tool<'a>(
        &'a self,
        _tool: &'a str,
        _project_id: &'a str,
        attempt_key: &'a str,
        _record: &'a Value,
        _agent_id: &'a str,
    ) -> LocalBoxFuture<'a, Result<Value>> {
        self.calls.set(self.calls.get() + 1);
        let result = (self.submit.borrow_mut())(attempt_key);
        async move { result }.boxed_local()
    }
    fn recorded_ids(&self, _project_id: &str, kind: &str) -> Result<Vec<String>> {
        Ok(if kind == "art" {
            self.artifacts.clone()
        } else {
            Vec::new()
        })
    }
}

fn context() -> TerminalContext {
    TerminalContext {
        project_id: create_id(IdPrefix::Prj),
        agent_id: create_id(IdPrefix::Agt),
        task_id: Some(create_id(IdPrefix::Tsk)),
        instruction_id: Some("ins_test".into()),
        thread_id: Some("thr_test".into()),
        expected_version: Some(2.0),
        turn_id: "ins_test".into(),
        allowed_tools: vec!["nosh_response_submit".into(), "nosh_episode_submit".into()],
        expected_episode_type: Some("task_execution".into()),
    }
}

fn records() -> Vec<Value> {
    vec![
        json!({ "$schema": schema_uri("general-worker-completion"), "schemaVersion": 1 }),
        json!({ "$schema": schema_uri("episode-draft"), "schemaVersion": 1 }),
    ]
}

fn memory() -> Rc<EventStore> {
    Rc::new(EventStore::open(Path::new(":memory:")).unwrap())
}

#[test]
fn fails_closed_after_an_admitted_generation_is_interrupted_before_rejection() {
    local(async {
        let (store, context) = (memory(), context());
        assert_eq!(admit_terminal(&host(&store), &context).await.unwrap(), None);
        assert_match(
            &admit_terminal(&host(&store), &context)
                .await
                .unwrap()
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": false }),
        );
        assert_match(
            &submit_terminal(&host(&store), &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false }),
        );
    });
}

#[test]
fn reports_an_explicit_permanently_closed_partial_acceptance_when_the_second_record_is_refused() {
    local(async {
        let (store, context) = (memory(), context());
        let daemon = host(&store);
        let mut answers = vec![
            json!({ "accepted": false, "retryAllowed": true }),
            json!({ "accepted": true }),
        ];
        *daemon.submit.borrow_mut() = Box::new(move |_| Ok(answers.pop().unwrap()));
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": false, "status": "partial", "results": [{ "accepted": true }, { "accepted": false }] }),
        );
        assert_match(
            &admit_terminal(&daemon, &context).await.unwrap().unwrap(),
            &json!({ "accepted": false, "retryAllowed": false, "status": "partial" }),
        );
        assert_eq!(daemon.calls.get(), 2);
    });
}

#[test]
fn checks_cancellation_at_the_serialized_commit_boundary() {
    local(async {
        let (store, context) = (memory(), context());
        let daemon = host(&store);
        let pending = submit_terminal(&daemon, &context, records(), false);
        daemon.active.set(false);
        assert_match(
            &pending.await.unwrap(),
            &json!({ "accepted": false, "retryAllowed": false, "status": "rejected" }),
        );
        assert_eq!(daemon.calls.get(), 0);
    });
}

#[test]
fn uses_real_daemon_validation_and_durable_task_authority_then_recovers_the_event_journal_crash_window_without_a_live_agent()
 {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let database = directory.path().join("project.sqlite");
        let agents = Rc::new(FakeAgents::default());
        let daemon = daemon(&directory.path().join("data"), Rc::clone(&agents));
        let base = context();
        let task_context = TerminalContext {
            instruction_id: None,
            thread_id: None,
            expected_version: None,
            turn_id: format!("task:{}", base.task_id.clone().unwrap()),
            expected_episode_type: None,
            ..base.clone()
        };
        let record = json!({ "$schema": schema_uri("general-worker-completion"), "schemaVersion": 1, "taskOutcome": "completed", "workPerformed": [], "codeChanges": { "startingCommit": "a".repeat(40), "endingCommit": "a".repeat(40), "changedPaths": [], "diffArtifactId": null, "branch": "main" }, "commands": [], "criteria": [], "scientificImpact": { "claimIds": [], "evidenceIds": [], "interpretation": "No scientific claims." }, "deviations": [], "newRisks": [], "unresolvedItems": [], "suggestedNextActions": [], "readyForDeterministicPostflight": true, "readyForReview": false });
        daemon
            .register_project(&base.project_id, &repository, &database)
            .unwrap();
        // Only provider/session liveness is a fixture. Validation, research policy, submission
        // gate, event acceptance and SQLite task authority are real.
        agents.sessions.borrow_mut().push(inspection(
            &base.project_id,
            &base.agent_id,
            base.task_id.as_deref(),
            Role::GeneralWorker,
        ));
        agents.turn_active.set(true);
        let submitted = |daemon: &Rc<nosh_orchestration::daemon::NoshDaemon>| {
            daemon
                .replay(&base.project_id, 0)
                .unwrap()
                .iter()
                .filter(|event| event["type"] == "record.submitted")
                .count()
        };
        assert_eq!(daemon.admit_terminal(&task_context).await.unwrap(), None);
        let mut invalid = record.clone();
        invalid["schemaVersion"] = json!(2);
        assert_match(
            &daemon
                .submit_terminal(&task_context, vec![invalid], false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": true }),
        );
        assert_eq!(submitted(&daemon), 0);
        assert_eq!(daemon.admit_terminal(&task_context).await.unwrap(), None);
        assert_match(
            &daemon
                .submit_terminal(&task_context, vec![record.clone()], false)
                .await
                .unwrap(),
            &json!({ "accepted": true, "status": "completed" }),
        );
        assert_eq!(
            daemon
                .research
                .terminal_record(&base.project_id, base.task_id.as_deref().unwrap(), None)
                .unwrap(),
            Some(record.clone())
        );
        {
            let store = EventStore::open(&database).unwrap();
            let mut journal = store
                .terminal_turn(
                    &base.project_id,
                    &task_context.turn_id,
                    &json!(task_context),
                )
                .unwrap();
            // Simulate death after event append but before saving its journal result.
            journal["results"] = json!([]);
            journal["closed"] = json!(false);
            journal["status"] = json!("pending");
            store
                .save_terminal_turn(&base.project_id, &task_context.turn_id, &journal)
                .unwrap();
        }
        agents.sessions.borrow_mut().clear();
        agents.turn_active.set(false);
        assert_match(
            &daemon.admit_terminal(&task_context).await.unwrap().unwrap(),
            &json!({ "accepted": true, "status": "completed" }),
        );
        assert_eq!(submitted(&daemon), 1);
        daemon.stop();
    });
}

#[test]
fn persists_one_correction_across_reopening_and_never_revives_an_exhausted_turn() {
    local(async {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite");
        let context = context();
        {
            let store = Rc::new(EventStore::open(&path).unwrap());
            assert_match(
                &reject_terminal(&host(&store), &context, "bad JSON")
                    .await
                    .unwrap(),
                &json!({ "retryAllowed": true }),
            );
        }
        let store = Rc::new(EventStore::open(&path).unwrap());
        let daemon = host(&store);
        assert_match(
            &reject_terminal(&daemon, &context, "bad JSON again")
                .await
                .unwrap(),
            &json!({ "retryAllowed": false }),
        );
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": false }),
        );
        assert_eq!(daemon.calls.get(), 0);
    });
}

#[test]
fn freezes_intent_before_effects_resumes_accepted_prefix_and_rejects_rewritten_replay() {
    local(async {
        let (store, context) = (memory(), context());
        let daemon = host(&store);
        let interrupted = Cell::new(true);
        let (journal_store, journal_context) = (Rc::clone(&store), context.clone());
        let instruction = format!("instruction:{}", context.instruction_id.clone().unwrap());
        *daemon.submit.borrow_mut() = Box::new(move |attempt| {
            let journal = journal_store
                .terminal_turn(
                    &journal_context.project_id,
                    &journal_context.turn_id,
                    &json!(journal_context),
                )
                .unwrap();
            assert_eq!(journal["records"], json!(records()));
            if attempt == instruction && interrupted.get() {
                interrupted.set(false);
                return Err(Error::Rejected("interrupted".into()));
            }
            Ok(
                json!({ "accepted": true, "effect": { "state": "failed", "error": "effect failed after acceptance" } }),
            )
        });
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "status": "partial", "retryAllowed": false }),
        );
        let replacement: Vec<Value> = records()
            .into_iter()
            .map(|mut record| {
                record["schemaVersion"] = json!(2);
                record
            })
            .collect();
        assert_match(
            &submit_terminal(&daemon, &context, replacement, false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "error": "Terminal envelope replay conflict" }),
        );
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": true, "status": "completed", "effect": { "state": "failed" } }),
        );
        assert_eq!(daemon.calls.get(), 3);
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": true }),
        );
        assert_eq!(daemon.calls.get(), 3);
        let other = TerminalContext {
            agent_id: "agt_other".into(),
            ..context.clone()
        };
        let error = submit_terminal(&daemon, &other, records(), false)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("context replay conflict"),
            "{error}"
        );
    });
}

#[test]
fn prevalidates_the_whole_batch_before_any_acceptance_and_serializes_identical_concurrent_submissions()
 {
    local(async {
        let (store, context) = (memory(), context());
        let daemon = host(&store);
        let mut first = true;
        *daemon.validate.borrow_mut() = Box::new(move || {
            if std::mem::take(&mut first) {
                return Err(Error::Rejected(
                    "Invalid terminal record: invalid second record".into(),
                ));
            }
            Ok(())
        });
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": true }),
        );
        assert_eq!(daemon.calls.get(), 0);
        let (a, b) = tokio::join!(
            submit_terminal(&daemon, &context, records(), false),
            submit_terminal(&daemon, &context, records(), false)
        );
        assert!(a.unwrap()["accepted"] == true && b.unwrap()["accepted"] == true);
        assert_eq!(daemon.calls.get(), 2);
    });
}

#[test]
fn offers_the_one_correction_for_a_mis_copied_own_scope_id_but_not_for_a_foreign_task() {
    local(async {
        let (store, context) = (memory(), context());
        let slip = refusing(
            &store,
            "Scope field mismatch: experimentId must be exactly exp_a",
        );
        assert_match(
            &submit_terminal(&slip, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": true }),
        );
        let foreign = refusing(&store, "Terminal task scope mismatch");
        let other = TerminalContext {
            turn_id: "ins_foreign".into(),
            ..context.clone()
        };
        assert_match(
            &submit_terminal(&foreign, &other, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": false }),
        );
    });
}

#[test]
fn names_the_recorded_id_a_mis_copied_reference_was_probably_meant_to_be_so_the_one_correction_can_fix_it()
 {
    local(async {
        let (store, context) = (memory(), context());
        // Live: a reviewer cited art_…bddd3d7f5… for art_…bddd3f7f5… and, told only the bad id,
        // resent the same envelope.
        let mut slip = refusing(
            &store,
            "Episode references unknown record art_7c220b895ba72bddd3d7f5f0b3752b29",
        );
        slip.artifacts = vec![
            "art_7c220b895ba72bddd3f7f5f0b3752b29".into(),
            "art_10bf0ee659b623b9154ab3399391ea96".into(),
        ];
        assert_match(
            &submit_terminal(&slip, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": true, "error": "Episode references unknown record art_7c220b895ba72bddd3d7f5f0b3752b29 (did you mean art_7c220b895ba72bddd3f7f5f0b3752b29?)" }),
        );
        // Nothing close enough: the message is unchanged.
        let mut unrelated = refusing(
            &store,
            "Episode references unknown record art_ffffffffffffffffffffffffffffffff",
        );
        unrelated.artifacts = vec!["art_7c220b895ba72bddd3f7f5f0b3752b29".into()];
        let other = TerminalContext {
            turn_id: "ins_unrelated".into(),
            ..context.clone()
        };
        assert_match(
            &submit_terminal(&unrelated, &other, records(), false)
                .await
                .unwrap(),
            &json!({ "error": "Episode references unknown record art_ffffffffffffffffffffffffffffffff" }),
        );
    });
}

#[test]
fn admits_at_most_two_provider_generations_across_host_replacement() {
    local(async {
        let (store, context) = (memory(), context());
        assert_eq!(admit_terminal(&host(&store), &context).await.unwrap(), None);
        assert_match(
            &reject_terminal(&host(&store), &context, "bad JSON")
                .await
                .unwrap(),
            &json!({ "retryAllowed": true }),
        );
        assert_eq!(admit_terminal(&host(&store), &context).await.unwrap(), None);
        assert_match(
            &admit_terminal(&host(&store), &context)
                .await
                .unwrap()
                .unwrap(),
            &json!({ "accepted": false, "retryAllowed": false, "status": "rejected" }),
        );
        assert_match(
            &submit_terminal(&host(&store), &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false }),
        );
    });
}

#[test]
fn provider_failure_permanently_closes_a_turn_before_accepting_any_apparent_json() {
    local(async {
        let (store, context) = (memory(), context());
        let daemon = host(&store);
        assert_match(
            &reject_terminal(&daemon, &context, "Non-successful provider stop: aborted")
                .await
                .unwrap(),
            &json!({ "retryAllowed": false }),
        );
        assert_match(
            &submit_terminal(&daemon, &context, records(), false)
                .await
                .unwrap(),
            &json!({ "accepted": false }),
        );
        assert_eq!(daemon.calls.get(), 0);
    });
}
