//! The `nosh` command (`apps/cli/src/main.ts`), plus the two commands NOSH starts itself: the
//! daemon (`nosh daemon`) and the Job exit wrapper (`nosh job-wrapper`).

fn main() {
    // Arguments come from NOSH, from JSON Job specs, or from a terminal, so they are Unicode.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let code = match args.first().map(String::as_str) {
        Some("daemon") => match std::env::current_exe() {
            Ok(executable) => nosh_server::noshd::main(
                &args[1..],
                vec![executable.into_os_string(), "job-wrapper".into()],
            ),
            Err(error) => {
                eprintln!("{error}");
                1
            }
        },
        Some("job-wrapper") => nosh_sys::job_wrapper::run(&args[1..]),
        _ => nosh::cli::run(&args),
    };
    std::process::exit(code);
}
