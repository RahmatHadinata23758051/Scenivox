use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand};
use scenivox::{
    config, db,
    domain::{
        CreateProject, CreateRevision, ListQuery, PlanStoryboard, RegisterModel,
        ResolveModelSelection, SetModelSettings, UpdateProject,
    },
    store::{Store, StoreError},
};
use serde::{Serialize, de::DeserializeOwned};

const MAX_JSON_BYTES: u64 = 1024 * 1024;

#[derive(Parser)]
#[command(name = "scenivox", about = "Local-first Scenivox backend")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the local HTTP API.
    Serve,
    /// Persist and inspect projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Persist and inspect immutable revisions.
    Revision {
        #[command(subcommand)]
        command: RevisionCommand,
    },
    /// Generate and persist a storyboard draft offline.
    Storyboard {
        #[command(subcommand)]
        command: StoryboardCommand,
    },
    /// Inspect and configure model metadata and selections without provider calls.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
}

#[derive(Subcommand)]
pub enum ModelCommand {
    List,
    SettingsGet,
    SettingsSet {
        #[arg(long)]
        file: PathBuf,
    },
    Register {
        #[arg(long)]
        file: PathBuf,
    },
    Resolve {
        #[arg(long)]
        file: PathBuf,
    },
    ProjectSet {
        project: String,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum StoryboardCommand {
    Generate {
        project: String,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum ProjectCommand {
    Create {
        #[arg(long)]
        file: PathBuf,
    },
    Get {
        id: String,
    },
    List {
        #[command(flatten)]
        paging: Paging,
    },
    Update {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand)]
pub enum RevisionCommand {
    Create {
        project: String,
        #[arg(long)]
        file: PathBuf,
    },
    Get {
        project: String,
        revision: String,
    },
    List {
        project: String,
        #[command(flatten)]
        paging: Paging,
    },
}

#[derive(Args)]
pub struct Paging {
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long)]
    cursor: Option<String>,
}

impl From<Paging> for ListQuery {
    fn from(value: Paging) -> Self {
        Self {
            limit: value.limit,
            cursor: value.cursor,
        }
    }
}

pub struct CliError {
    code: String,
    message: String,
}

impl CliError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn report(&self) {
        // No raw IO, configuration, SQL, input, or argument errors cross this boundary.
        let envelope = serde_json::json!({
            "schema_version": "1",
            "error": { "code": self.code, "message": self.message, "details": [] }
        });
        let mut stderr = io::stderr().lock();
        let _ = serde_json::to_writer(&mut stderr, &envelope);
        let _ = writeln!(stderr);
    }
}

impl From<StoreError> for CliError {
    fn from(error: StoreError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    let file = File::open(path)
        .map_err(|_| CliError::new("input_unavailable", "Unable to open input JSON file"))?;
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CliError::new("input_unavailable", "Unable to read input JSON file"))?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(CliError::new(
            "payload_too_large",
            "Input JSON exceeds 1 MiB",
        ));
    }
    scenivox::json::parse::<T>(&bytes).map_err(|_| {
        CliError::new(
            "malformed_json",
            "Input must be valid JSON matching the command schema, without duplicate keys",
        )
    })
}

fn emit<T: Serialize>(value: &T) -> Result<(), CliError> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)
        .map_err(|_| CliError::new("output_unavailable", "Unable to write JSON output"))?;
    writeln!(stdout).map_err(|_| CliError::new("output_unavailable", "Unable to write JSON output"))
}

async fn connect() -> Result<Store, CliError> {
    let config = config::Config::from_env()
        .map_err(|_| CliError::new("invalid_configuration", "Invalid application configuration"))?;
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|_| CliError::new("database_unavailable", "Database is unavailable"))?;
    Ok(Store::new(pool))
}

pub async fn run(command: Command) -> Result<(), CliError> {
    match command {
        Command::Project { command } => match command {
            ProjectCommand::Create { file } => {
                let input = load_json::<CreateProject>(&file)?;
                emit(&connect().await?.create_project(input).await?)
            }
            ProjectCommand::Get { id } => emit(&connect().await?.get_project(&id).await?),
            ProjectCommand::List { paging } => {
                emit(&connect().await?.list_projects(paging.into()).await?)
            }
            ProjectCommand::Update { id, file } => {
                let input = load_json::<UpdateProject>(&file)?;
                emit(&connect().await?.update_project(&id, input).await?)
            }
            ProjectCommand::Delete { id } => emit(&connect().await?.delete_project(&id).await?),
        },
        Command::Revision { command } => match command {
            RevisionCommand::Create { project, file } => {
                let input = load_json::<CreateRevision>(&file)?;
                emit(&connect().await?.create_revision(&project, input).await?)
            }
            RevisionCommand::Get { project, revision } => {
                emit(&connect().await?.get_revision(&project, &revision).await?)
            }
            RevisionCommand::List { project, paging } => emit(
                &connect()
                    .await?
                    .list_revisions(&project, paging.into())
                    .await?,
            ),
        },
        Command::Storyboard { command } => match command {
            StoryboardCommand::Generate { project, file } => {
                let input = load_json::<PlanStoryboard>(&file)?;
                emit(
                    &connect()
                        .await?
                        .plan_storyboard(
                            &project,
                            None,
                            input,
                            &scenivox::planner::DeterministicMockPlanner,
                        )
                        .await?,
                )
            }
        },
        Command::Model { command } => match command {
            ModelCommand::List => emit(&connect().await?.list_models().await?),
            ModelCommand::SettingsGet => emit(&connect().await?.get_model_settings().await?),
            ModelCommand::SettingsSet { file } => {
                let input = load_json::<SetModelSettings>(&file)?;
                emit(&connect().await?.set_model_settings(input).await?)
            }
            ModelCommand::Register { file } => {
                let input = load_json::<RegisterModel>(&file)?;
                emit(&connect().await?.register_model(input).await?)
            }
            ModelCommand::Resolve { file } => {
                let input = load_json::<ResolveModelSelection>(&file)?;
                emit(&connect().await?.resolve_model_selection(input).await?)
            }
            ModelCommand::ProjectSet { project, file } => {
                let input = load_json::<SetModelSettings>(&file)?;
                emit(
                    &connect()
                        .await?
                        .set_project_model_selection(&project, input)
                        .await?,
                )
            }
        },
        Command::Serve => Err(CliError::new(
            "internal_error",
            "Serve must run through the server entrypoint",
        )),
    }
}
