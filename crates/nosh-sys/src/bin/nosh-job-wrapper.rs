//! `nosh-job-wrapper <exitFile> -- <command> [args...]`: the Job exit wrapper as its own binary,
//! for nosh-sys's tests. Releases run it as `nosh job-wrapper`; see [`nosh_sys::job_wrapper`].

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(nosh_sys::job_wrapper::run(&args));
}
