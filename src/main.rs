mod animation;
mod appearance;
mod cli;
mod client;
mod ipc;
mod island;
mod server;

use clap::Parser;

use cli::Cli;

fn main() {
    let cli = Cli::parse();

    match cli.command {
        // With a command we act as a client and hand the request to the server.
        Some(_) => {
            let line: Vec<String> = std::env::args().skip(1).collect();
            client::run(line.join(" "))
        }
        // Without one we are the server.
        None => server::run(),
    }
}
