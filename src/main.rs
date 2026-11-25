mod cli;

use scenivox::{api, config, db};

use std::error::Error;

use clap::Parser;
use cli::{Cli, CliError, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _ = dotenvy::dotenv();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return std::process::ExitCode::SUCCESS;
        }
        Err(_) => {
            CliError::new(
                "invalid_arguments",
                "Invalid command arguments; use --help for usage",
            )
            .report();
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            // Serve retains stdout startup logs; one-shot commands reserve stdout for JSON.
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .with_target(false)
                .init();
            serve()
                .await
                .map_err(|_| CliError::new("startup_failed", "Unable to run the local API"))
        }
        command => cli::run(command).await,
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            error.report();
            std::process::ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), Box<dyn Error>> {
    let config = config::Config::from_env()?;
    let db = db::connect(&config.database_url).await?;
    let listener = tokio::net::TcpListener::bind(config.address).await?;
    tracing::info!(address = %listener.local_addr()?, "Scenivox API listening");
    axum::serve(
        listener,
        api::router_with_skill_store(db, config.skill_store_dir),
    )
    .await?;
    Ok(())
}
