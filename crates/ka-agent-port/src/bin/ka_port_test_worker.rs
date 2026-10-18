//! The scripted-speaker test worker (feature `test-support`).
//!
//! Same worker loop as `ka-agent __worker`, as its own tiny binary so
//! the e2e suite can spawn it via `CARGO_BIN_EXE_ka-port-test-worker`
//! and the console chat can point `WorkerPort::binary` at it for
//! deterministic, network-free sessions.

fn main() {
    let mut scenario = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--scenario"
            && let Some(path) = args.next()
        {
            scenario = Some(path.into());
        }
    }
    if let Err(e) = ka_agent_port::run_worker(ka_agent_port::WorkerRun { scenario }) {
        eprintln!("ka-port-test-worker: {e}");
        std::process::exit(1);
    }
}
