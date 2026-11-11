//! The `run_script` child. Spawned by `sandbox::Sandbox::run` inside a Job Object with
//! `CREATE_NO_WINDOW`; talks JSON lines on stdin/stdout. Not meant to be run by hand.

fn main() {
    std::process::exit(sandbox::child::main());
}
