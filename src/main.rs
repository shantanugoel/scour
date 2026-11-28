mod api;
mod cli;
mod config;
#[cfg(test)]
mod daemon_tests;
mod engines;
mod limits;
mod mcp;
mod search;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use axum::{Router, serve::Serve};
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
    Search {
        /// The words to search for.
        query: String,
        /// Ask only these engines, as a comma list of `name` or `name@method`.
        #[arg(long)]
        engines: Option<String>,
    },
    /// Show engine health and daily counts.
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve { config } => serve(config).await,
        Command::Search { query, engines } => {
            cli::search(api::ADDR, &query, engines.as_deref()).await
        }
        Command::Status => cli::status(api::ADDR).await,
    }
}

async fn serve(config_path: Option<PathBuf>) -> ExitCode {
    let server = match start_daemon(config_path.as_deref(), api::ADDR).await {
        Ok(server) => server,
        Err(message) => {
            eprintln!("scour: {message}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = server.await {
        eprintln!("scour: the server stopped: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

pub(crate) async fn start_daemon(
    config_path: Option<&Path>,
    address: SocketAddr,
) -> Result<Serve<TcpListener, Router, Router>, String> {
    let config = config::load(config_path)?;
    let searcher =
        search::Searcher::new(&config).map_err(|_| "cannot start the search client".to_string())?;
    let listener = TcpListener::bind(address)
        .await
        .map_err(|err| format!("cannot listen on {address}: {err}"))?;
    let bound_address = listener
        .local_addr()
        .map_err(|err| format!("cannot read the listen address: {err}"))?;
    let proxy = config
        .proxy
        .as_ref()
        .map(|url| format!(" (proxy {})", config::proxy_address(url)))
        .unwrap_or_default();
    println!("scour: listening on http://{bound_address}{proxy}");
    Ok(axum::serve(listener, api::router(Arc::new(searcher))))
}
