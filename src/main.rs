mod api;
mod browser;
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
use tokio::signal::unix::{Signal, SignalKind, signal};

use browser::{Browser, BrowserSettings};

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
        /// Enable the browser method and start Chrome on its first query.
        #[arg(long)]
        browser: bool,
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
        Command::Serve { config, browser } => serve(config, browser).await,
        Command::Search { query, engines } => {
            cli::search(api::ADDR, &query, engines.as_deref()).await
        }
        Command::Status => cli::status(api::ADDR).await,
    }
}

async fn serve(config_path: Option<PathBuf>, browser: bool) -> ExitCode {
    let browser = browser.then(BrowserSettings::fixed);
    let daemon = match start_daemon(config_path.as_deref(), api::ADDR, browser).await {
        Ok(daemon) => daemon,
        Err(message) => {
            eprintln!("scour: {message}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = daemon.run().await {
        eprintln!("scour: the server stopped: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

struct Daemon {
    server: Serve<TcpListener, Router, Router>,
    searcher: Arc<search::Searcher>,
    interrupt: Signal,
    terminate: Signal,
}

impl Daemon {
    #[cfg(test)]
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.server.local_addr()
    }

    async fn run(mut self) -> std::io::Result<()> {
        let result = tokio::select! {
            result = self.server.into_future() => result,
            _ = self.interrupt.recv() => Ok(()),
            _ = self.terminate.recv() => Ok(()),
        };
        self.searcher.stop_browser().await;
        result
    }
}

async fn start_daemon(
    config_path: Option<&Path>,
    address: SocketAddr,
    browser: Option<BrowserSettings>,
) -> Result<Daemon, String> {
    let config = config::load(config_path, browser.is_some())?;
    let listener = TcpListener::bind(address)
        .await
        .map_err(|err| format!("cannot listen on {address}: {err}"))?;
    let browser = browser
        .map(|settings| Browser::open(settings, config.browser_idle))
        .transpose()?;
    let searcher = Arc::new(
        search::Searcher::new(&config, browser)
            .map_err(|_| "cannot start the search client".to_string())?,
    );
    let bound_address = listener
        .local_addr()
        .map_err(|err| format!("cannot read the listen address: {err}"))?;
    let proxy = config
        .proxy
        .as_ref()
        .map(|url| format!(" (proxy {})", config::proxy_address(url)))
        .unwrap_or_default();
    let interrupt =
        signal(SignalKind::interrupt()).map_err(|err| format!("cannot handle SIGINT: {err}"))?;
    let terminate =
        signal(SignalKind::terminate()).map_err(|err| format!("cannot handle SIGTERM: {err}"))?;
    println!("scour: listening on http://{bound_address}{proxy}");
    let server = axum::serve(listener, api::router(searcher.clone()));
    Ok(Daemon {
        server,
        searcher,
        interrupt,
        terminate,
    })
}
