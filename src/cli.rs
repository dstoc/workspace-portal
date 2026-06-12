use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{
    daemon::{
        self, AddArgs, CheckArgs, EditArgs, FreezeArgs, ListArgs, RemoveArgs, StartArgs,
        StatusArgs, StopArgs, ThawArgs,
    },
    error::{Error, Result},
};

#[derive(Debug, Parser)]
#[command(
    name = "workspace-portal",
    version,
    about = "Controlled workspace portal"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Start(StartCommand),
    Add(AddCommand),
    Freeze(FreezeCommand),
    Rm(RmCommand),
    Status(StatusCommand),
    Stop(StopCommand),
    Thaw(ThawCommand),
    List,
    Check(CheckCommand),
    Edit(EditCommand),
}

#[derive(Debug, Parser)]
pub struct StartCommand {
    /// Workspace directory to mount and manage.
    pub workspace: PathBuf,

    #[arg(long, help = "Run the daemon in the background")]
    pub bg: bool,

    #[arg(long, help = "Override the control socket path")]
    pub socket: Option<PathBuf>,

    #[arg(long = "state-dir", help = "Override the state directory path")]
    pub state_dir: Option<PathBuf>,

    #[arg(long, help = "Enable the FUSE allow_other mount option")]
    pub allow_other: bool,

    #[arg(
        long = "no-allow-other",
        help = "Disable the FUSE allow_other mount option"
    )]
    pub no_allow_other: bool,

    #[arg(long = "read-only", help = "Mount the workspace read-only by default")]
    pub read_only: bool,

    #[arg(
        long,
        help = "Adopt an existing workspace directory instead of requiring it to be empty"
    )]
    pub adopt: bool,

    #[arg(long, help = "Override stale state or existing mount conditions")]
    pub force: bool,

    #[arg(long = "log-level", help = "Set the daemon log level")]
    pub log_level: Option<String>,

    #[arg(
        long,
        hide = true,
        help = "Internal flag used when spawning the daemon child process"
    )]
    pub daemon_child: bool,
}

#[derive(Debug, Parser)]
pub struct AddCommand {
    /// Host directory to expose in the workspace.
    pub target: PathBuf,
    /// Top-level workspace entry name to create.
    pub mount_point: String,

    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,

    #[arg(long, help = "Add the entry as read-only")]
    pub ro: bool,

    #[arg(long, help = "Add the entry as read-write")]
    pub rw: bool,

    #[arg(long, help = "Replace an existing entry with the same name")]
    pub replace: bool,

    #[arg(long, help = "Deprecated alias for the mount-point name")]
    pub name: Option<String>,
}

#[derive(Debug, Parser)]
pub struct RmCommand {
    /// Top-level workspace entry name to remove.
    pub mount_point: String,

    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct FreezeCommand {
    /// Immutable path segment name to freeze workspace-wide.
    pub segment: String,

    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct StatusCommand {
    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,

    #[arg(
        long,
        help = "Print machine-readable JSON instead of human-readable text"
    )]
    pub json: bool,
}

#[derive(Debug, Parser)]
pub struct StopCommand {
    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,

    #[arg(long, help = "Use lazy unmount when stopping the workspace")]
    pub lazy: bool,

    #[arg(long, help = "Override stale state or existing mount conditions")]
    pub force: bool,
}

#[derive(Debug, Parser)]
pub struct ThawCommand {
    /// Immutable path segment name to unfreeze workspace-wide.
    pub segment: String,

    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct CheckCommand {
    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct EditCommand {
    #[arg(
        long,
        help = "Override workspace discovery with an explicit workspace path"
    )]
    pub workspace: Option<PathBuf>,
}

pub async fn run() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Start(cmd) => {
            validate_start(&cmd)?;
            daemon::start(StartArgs {
                workspace: cmd.workspace,
                socket: cmd.socket,
                state_dir: cmd.state_dir,
                bg: cmd.bg,
                daemon_child: cmd.daemon_child,
                allow_other: cmd.allow_other && !cmd.no_allow_other,
                read_only: cmd.read_only,
                adopt: cmd.adopt,
                force: cmd.force,
                log_level: cmd.log_level,
            })
            .await
        }
        Commands::Add(cmd) => {
            validate_add(&cmd)?;
            let mount_point = cmd.name.unwrap_or(cmd.mount_point);
            daemon::add(AddArgs {
                workspace: cmd.workspace,
                target: cmd.target,
                mount_point,
                read_only: cmd.ro,
                read_write: cmd.rw,
                replace: cmd.replace,
            })
            .await
        }
        Commands::Freeze(cmd) => {
            validate_freeze(&cmd)?;
            daemon::freeze(FreezeArgs {
                workspace: cmd.workspace,
                segment: cmd.segment,
            })
            .await
        }
        Commands::Rm(cmd) => {
            daemon::remove(RemoveArgs {
                workspace: cmd.workspace,
                mount_point: cmd.mount_point,
            })
            .await
        }
        Commands::Status(cmd) => {
            daemon::status(StatusArgs {
                workspace: cmd.workspace,
                json: cmd.json,
            })
            .await
        }
        Commands::Stop(cmd) => {
            daemon::stop(StopArgs {
                workspace: cmd.workspace,
                lazy: cmd.lazy,
                force: cmd.force,
            })
            .await
        }
        Commands::Thaw(cmd) => {
            validate_thaw(&cmd)?;
            daemon::thaw(ThawArgs {
                workspace: cmd.workspace,
                segment: cmd.segment,
            })
            .await
        }
        Commands::List => daemon::list(ListArgs).await,
        Commands::Check(cmd) => {
            daemon::check(CheckArgs {
                workspace: cmd.workspace,
            })
            .await
        }
        Commands::Edit(cmd) => {
            daemon::edit(EditArgs {
                workspace: cmd.workspace,
            })
            .await
        }
    }
}

fn validate_start(cmd: &StartCommand) -> Result<()> {
    if cmd.allow_other && cmd.no_allow_other {
        return Err(Error::Cli(
            "choose either --allow-other or --no-allow-other".to_owned(),
        ));
    }

    Ok(())
}

fn validate_add(cmd: &AddCommand) -> Result<()> {
    if cmd.ro && cmd.rw {
        return Err(Error::Cli("choose either --ro or --rw".to_owned()));
    }

    Ok(())
}

fn validate_freeze(cmd: &FreezeCommand) -> Result<()> {
    crate::paths::validate_immutable_segment_name(&cmd.segment)
}

fn validate_thaw(cmd: &ThawCommand) -> Result<()> {
    crate::paths::validate_immutable_segment_name(&cmd.segment)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
