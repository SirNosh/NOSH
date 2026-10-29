//! Research spawning helpers, as `apps/noshd/src/research-spawner.ts`.

/// Whether `argv` runs the same program as the frozen evaluation command: the same non-flag
/// arguments, in order.
pub fn is_evaluation_variant(argv: &[String], evaluation: &[String]) -> bool {
    let program = |args: &[String]| -> Vec<String> {
        args.iter()
            .filter(|arg| !arg.starts_with('-'))
            .cloned()
            .collect()
    };
    program(argv) == program(evaluation)
}
