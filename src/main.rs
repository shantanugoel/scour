mod api;
mod cli;
mod config;
mod engines;
mod search;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

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
    Serve {
        /// Read this TOML file instead of the default config file.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Ask the local daemon to search.
    Search { query: String },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve { config } => serve(config).await,
        Command::Search { query } => cli::search(&query).await,
    }
}

async fn serve(config_path: Option<PathBuf>) -> ExitCode {
    let config = match config::load(config_path.as_deref()) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("scour: {message}");
            return ExitCode::from(1);
        }
    };
    let searcher = match search::Searcher::new(&config) {
        Ok(searcher) => searcher,
        Err(err) => {
            eprintln!("scour: cannot start the client: {err}");
            return ExitCode::from(1);
        }
    };
    let listener = match TcpListener::bind(api::ADDR).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("scour: cannot listen on {}: {err}", api::ADDR);
            return ExitCode::from(1);
        }
    };
    println!("scour: listening on http://{}", api::ADDR);
    if let Err(err) = axum::serve(listener, api::router(Arc::new(searcher))).await {
        eprintln!("scour: the server stopped: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
