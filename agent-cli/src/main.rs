fn main() {
    agent_cli::run(std::env::args().skip(1).collect());
}