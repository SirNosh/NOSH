//! The `nosh` command. The CLI proper is P7; this binary already runs the daemon (`nosh daemon`)
//! and the Job exit wrapper (`nosh job-wrapper`), the two commands NOSH starts itself.

fn main() {
    // Arguments come from NOSH or from JSON Job specs, so they are Unicode.
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
        _ => {
            println!(
                "nosh {} (Rust port in progress; see port-map.json)",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
    };
    std::process::exit(code);
}
