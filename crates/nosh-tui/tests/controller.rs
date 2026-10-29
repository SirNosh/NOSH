//! Controller tests T1-T53, ported one for one from `apps/tui/src/controller.test.ts` ("thin
//! controller", "workspace presentation", "project-scoped concurrency"), including its tests of
//! `view-model.ts` functions.
//!
//! Specified by tui-controller.md §15.0 (goldens), §15.1 (fixtures and the scripted `DaemonApi`
//! mock: calls recorded when `request` is invoked, FIFO one-shot responses, persistent rules,
//! controllable pending responses, a `settle()` helper), §15.2-§15.4 (the cases) and
//! tui-viewmodel.md §5.2-§5.3 (view-model cases and the `climb` fixture). Every test runs inside
//! `LocalSet::run_until`. Owned by Track B.
