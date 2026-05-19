fn main() {
    let mut args = std::env::args();
    let _ = args.next();
    let sub = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    match sub.as_str() {
        "server" => agent_server::run(rest),
        "cli" => agent_cli::run(rest),
        "worker" => {
            if let Err(e) = agent_worker::run() {
                eprintln!("worker: {}", e);
                std::process::exit(1);
            }
        }
        other => {
            eprintln!(
                "unknown subcommand: {}\nusage: agent <server|cli|worker> ...",
                other
            );
            std::process::exit(2);
        }
    }
}