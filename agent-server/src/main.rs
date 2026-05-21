fn main() {
    agent_server::run(std::env::args().skip(1).collect());
}
