use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use clap::{Args, Parser, Subcommand};
use scenivox::{
    asset::{AssetKind, RegisterAsset},
    config,
    cost::{CreateCostConsent, RevokeCostConsent, SetModelPricing},
    db,
    domain::{
        CreateProject, CreateRenderJob, CreateRevision, JobCancelCommand, ListQuery,
        PlanStoryboard, RegisterModel, ResolveModelSelection, RetryRenderJob, ReviewCommand,
        SetModelSettings, UpdateProject,
    },
    store::{Store, StoreError},
    voice::AudioProvenance,
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
    /// Inspect and configure model metadata without provider calls.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Inspect render jobs; no renderer is invoked.
    RenderJob {
        #[command(subcommand)]
        command: RenderJobCommand,
    },
    /// Validate local narration audio; no synthesis or alignment is invoked.
    Voice {
        #[command(subcommand)]
        command: VoiceCommand,
    },
    /// Import and inspect local skill bundles without executing skill code.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Register, inspect, verify, and diagnose local content-addressed WAV assets.
    Asset {
        #[command(subcommand)]
        command: AssetCommand,
    },
    /// Manage model pricing snapshots and scoped remote cost consents.
    Cost {
        #[command(subcommand)]
        command: CostCommand,
    },
}

#[derive(Subcommand)]
pub enum CostCommand {
    /// Register or update a model's verified pricing schedule.
    PricingSet {
        #[arg(long)]
        file: PathBuf,
    },
    /// Inspect a model's pricing schedule.
    PricingGet { provider: String, model: String },
    /// Create a scoped remote cost consent ceiling.
    ConsentCreate {
        #[arg(long)]
        file: PathBuf,
    },
    /// Inspect a cost consent record by UUID.
    ConsentGet { id: String },
    /// Revoke an active cost consent ceiling.
    ConsentRevoke {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum AssetCommand {
    /// Validate and register a local WAV asset into content-addressed storage.
    Register {
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value = "narration_wav")]
        kind: String,
        #[arg(long, default_value = "user_supplied")]
        provenance: String,
    },
    /// List registered assets (read-only).
    List {
        #[command(flatten)]
        paging: Paging,
    },
    /// Read a registered asset record by UUID.
    Get { id: String },
    /// Re-verify a registered asset's on-disk WAV bytes and SHA-256 digest (read-only).
    Verify { id: String },
    /// Diagnose staging dirs, unregistered asset blobs, and missing registered assets (read-only).
    Reconcile,
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Validate and import a local skill directory into the SQLite registry as unaudited.
    Import {
        #[arg(long)]
        path: PathBuf,
    },
    /// List imported skills (read-only).
    List {
        #[command(flatten)]
        paging: Paging,
    },
    /// Read a single imported skill record by id and version.
    Get { id: String, version: String },
    /// Re-verify a registered skill bundle's on-disk files and SHA-256 digest (read-only).
    Verify { id: String, version: String },
    /// Diagnose orphaned staging dirs, unregistered bundles, and missing registered bundles (read-only).
    Reconcile,
}

#[derive(Subcommand)]
pub enum VoiceCommand {
    /// Validate a user-supplied WAV and emit measured metadata without storing or dispatching it.
    Inspect {
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum RenderJobCommand {
    Enqueue {
        project: String,
        #[arg(long)]
        file: PathBuf,
    },
    Get {
        id: String,
    },
    Retry {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    Cancel {
        id: String,
        #[arg(long)]
        file: PathBuf,
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
    Review {
        project: String,
        revision: String,
        #[arg(long)]
        file: PathBuf,
    },
    WithdrawReview {
        project: String,
        revision: String,
        #[arg(long)]
        file: PathBuf,
    },
    Approve {
        project: String,
        revision: String,
        #[arg(long)]
        file: PathBuf,
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
fn open_local_audio(path: &Path) -> Result<(File, std::fs::Metadata), CliError> {
    // Reject empty paths, absolute paths, and non-normal components (including `.` and `..`).
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(CliError::new(
            "unsafe_audio_path",
            "Audio path must be a normal relative path inside the current directory",
        ));
    }
    let root = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .map_err(|_| CliError::new("input_unavailable", "Audio input is unavailable"))?;
    let resolved = path
        .canonicalize()
        .map_err(|_| CliError::new("input_unavailable", "Audio input is unavailable"))?;
    if !resolved.starts_with(&root) {
        return Err(CliError::new(
            "unsafe_audio_path",
            "Audio path must resolve to a file inside the current directory",
        ));
    }
    let file = File::open(&resolved)
        .map_err(|_| CliError::new("input_unavailable", "Audio input is unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| CliError::new("input_unavailable", "Audio input is unavailable"))?;
    if !metadata.is_file() {
        return Err(CliError::new(
            "input_unavailable",
            "Audio path must resolve to a regular file",
        ));
    }
    Ok((file, metadata))
}

fn emit<T: Serialize>(value: &T) -> Result<(), CliError> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)
        .map_err(|_| CliError::new("output_unavailable", "Unable to write JSON output"))?;
    writeln!(stdout).map_err(|_| CliError::new("output_unavailable", "Unable to write JSON output"))
}

async fn connect() -> Result<Store, CliError> {
    let (store, _) = connect_with_config().await?;
    Ok(store)
}

async fn connect_with_config() -> Result<(Store, config::Config), CliError> {
    let config = config::Config::from_env()
        .map_err(|_| CliError::new("invalid_configuration", "Invalid application configuration"))?;
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|_| CliError::new("database_unavailable", "Database is unavailable"))?;
    Ok((Store::new(pool), config))
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
            RevisionCommand::Review {
                project,
                revision,
                file,
            } => {
                let input = load_json::<ReviewCommand>(&file)?;
                emit(
                    &connect()
                        .await?
                        .review_revision(&project, &revision, None, input)
                        .await?,
                )
            }
            RevisionCommand::WithdrawReview {
                project,
                revision,
                file,
            } => {
                let input = load_json::<ReviewCommand>(&file)?;
                emit(
                    &connect()
                        .await?
                        .withdraw_review(&project, &revision, None, input)
                        .await?,
                )
            }
            RevisionCommand::Approve {
                project,
                revision,
                file,
            } => {
                let input = load_json::<ReviewCommand>(&file)?;
                let (approval, _, _) = connect()
                    .await?
                    .approve_revision(&project, &revision, None, input)
                    .await?;
                emit(&approval)
            }
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
        Command::RenderJob { command } => match command {
            RenderJobCommand::Enqueue { project, file } => {
                let input = load_json::<CreateRenderJob>(&file)?;
                let (job, _) = connect()
                    .await?
                    .enqueue_render_job(&project, None, input)
                    .await?;
                emit(&job)
            }
            RenderJobCommand::Get { id } => emit(&connect().await?.get_render_job(&id).await?),
            RenderJobCommand::Retry { id, file } => {
                let input = load_json::<RetryRenderJob>(&file)?;
                emit(&connect().await?.retry_render_job(&id, input).await?)
            }
            RenderJobCommand::Cancel { id, file } => {
                let input = load_json::<JobCancelCommand>(&file)?;
                emit(&connect().await?.cancel_render_job(&id, input).await?)
            }
        },
        Command::Voice { command } => match command {
            VoiceCommand::Inspect { file } => {
                let (audio, metadata) = open_local_audio(&file)?;
                if metadata.len() > scenivox::voice::MAX_WAV_BYTES as u64 {
                    return Err(CliError::new(
                        "audio_too_large",
                        "WAV exceeds the supported size limit",
                    ));
                }
                let narration = scenivox::voice::read_wav(audio).map_err(|error| {
                    let code = match error {
                        scenivox::voice::VoiceError::AudioTooLarge => "audio_too_large",
                        scenivox::voice::VoiceError::UnsupportedFormat => {
                            "unsupported_audio_format"
                        }
                        scenivox::voice::VoiceError::AudioLimits => "audio_limits_exceeded",
                        scenivox::voice::VoiceError::ReadFailure => "input_unavailable",
                        scenivox::voice::VoiceError::InvalidWav
                        | scenivox::voice::VoiceError::InvalidAudioData
                        | scenivox::voice::VoiceError::NarrationOverlong
                        | scenivox::voice::VoiceError::InvalidTiming => "invalid_audio",
                        scenivox::voice::VoiceError::ProviderUnavailable => "provider_unavailable",
                        scenivox::voice::VoiceError::UnsupportedCapability => {
                            "unsupported_capability"
                        }
                        scenivox::voice::VoiceError::ProviderFailure => "provider_failure",
                    };
                    CliError::new(code, error.to_string())
                })?;
                emit(&narration)
            }
        },
        Command::Skill { command } => match command {
            SkillCommand::Import { path } => {
                let (store, config) = connect_with_config().await?;
                let (record, _) = store
                    .import_skill(&config.skill_store_dir, None, &path)
                    .await?;
                emit(&record)
            }
            SkillCommand::List { paging } => {
                emit(&connect().await?.list_skills(paging.into()).await?)
            }
            SkillCommand::Get { id, version } => {
                emit(&connect().await?.get_skill(&id, &version).await?)
            }
            SkillCommand::Verify { id, version } => {
                let (store, config) = connect_with_config().await?;
                emit(
                    &store
                        .verify_skill(&config.skill_store_dir, &id, &version)
                        .await?,
                )
            }
            SkillCommand::Reconcile => {
                let (store, config) = connect_with_config().await?;
                emit(&store.reconcile_skill_store(&config.skill_store_dir).await?)
            }
        },
        Command::Asset { command } => match command {
            AssetCommand::Register {
                path,
                kind,
                provenance,
            } => {
                let asset_kind = AssetKind::parse(&kind).ok_or_else(|| {
                    CliError::new(
                        "invalid_schema",
                        "Asset kind must be narration_wav or sfx_wav",
                    )
                })?;
                let prov = match provenance.as_str() {
                    "user_supplied" => AudioProvenance::UserSupplied,
                    "synthetic_test_fixture" => AudioProvenance::SyntheticTestFixture,
                    _ => {
                        return Err(CliError::new(
                            "invalid_schema",
                            "Asset provenance must be user_supplied or synthetic_test_fixture",
                        ));
                    }
                };
                let raw_path = path.to_str().ok_or_else(|| {
                    CliError::new("unsafe_asset_path", "Asset path must be valid UTF-8")
                })?;
                let (store, config) = connect_with_config().await?;
                let (record, _, _) = store
                    .register_asset(
                        &config.asset_store_dir,
                        None,
                        RegisterAsset {
                            schema_version: "1".into(),
                            source_path: raw_path.to_owned(),
                            kind: asset_kind,
                            provenance: Some(prov),
                        },
                    )
                    .await?;
                emit(&record)
            }
            AssetCommand::List { paging } => {
                emit(&connect().await?.list_assets(paging.into()).await?)
            }
            AssetCommand::Get { id } => emit(&connect().await?.get_asset(&id).await?),
            AssetCommand::Verify { id } => {
                let (store, config) = connect_with_config().await?;
                emit(&store.verify_asset(&config.asset_store_dir, &id).await?)
            }
            AssetCommand::Reconcile => {
                let (store, config) = connect_with_config().await?;
                emit(&store.reconcile_asset_store(&config.asset_store_dir).await?)
            }
        },
        Command::Cost { command } => match command {
            CostCommand::PricingSet { file } => {
                let input = load_json::<SetModelPricing>(&file)?;
                emit(&connect().await?.set_model_pricing(input).await?)
            }
            CostCommand::PricingGet { provider, model } => emit(
                &connect()
                    .await?
                    .get_model_pricing(&provider, &model)
                    .await?,
            ),
            CostCommand::ConsentCreate { file } => {
                let input = load_json::<CreateCostConsent>(&file)?;
                let (consent, _) = connect().await?.create_cost_consent(None, input).await?;
                emit(&consent)
            }
            CostCommand::ConsentGet { id } => emit(&connect().await?.get_cost_consent(&id).await?),
            CostCommand::ConsentRevoke { id, file } => {
                let input = load_json::<RevokeCostConsent>(&file)?;
                emit(&connect().await?.revoke_cost_consent(&id, input).await?)
            }
        },
        Command::Serve => Err(CliError::new(
            "internal_error",
            "Serve must run through the server entrypoint",
        )),
    }
}
