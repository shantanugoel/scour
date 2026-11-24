mod api;
mod cli;
mod engines;
mod search;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(name = "scour")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen on 127.0.0.1:7268 and answer searches.
    Serve,
    /// Ask the local daemon to search.
    Search { query: String },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve => serve().await,
        Command::Search { query } => cli::search(&query).await,
    }
}

async fn serve() -> ExitCode {
    let listener = match TcpListener::bind(api::ADDR).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("scour: cannot listen on {}: {err}", api::ADDR);
            return ExitCode::from(1);
        }
    };
    let client = match search::http_client() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("scour: cannot start the client: {err}");
            return ExitCode::from(1);
        }
    };
    println!("scour: listening on http://{}", api::ADDR);
    if let Err(err) = axum::serve(listener, api::router(client)).await {
        eprintln!("scour: the server stopped: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
