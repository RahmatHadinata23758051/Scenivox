use scenivox::{api, config, db};

use std::error::Error;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "scenivox", about = "Local-first Scenivox backend")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the local HTTP API.
    Serve,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await?,
    }
    Ok(())
}

async fn serve() -> Result<(), Box<dyn Error>> {
    let config = config::Config::from_env()?;
    let db = db::connect(&config.database_url).await?;
    let listener = tokio::net::TcpListener::bind(config.address).await?;
    tracing::info!(address = %listener.local_addr()?, "Scenivox API listening");
    axum::serve(listener, api::router(db)).await?;
    Ok(())
}
