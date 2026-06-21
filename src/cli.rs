use std::{env, path::PathBuf};

use clap::{Parser, Subcommand};

use crate::{
    daemon::{self, CheckArgs, EditArgs, ForgetArgs, ListArgs, StartArgs, StatusArgs, StopArgs},
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
    Status(StatusCommand),
    Stop(StopCommand),
    List,
    Check(CheckCommand),
    Edit(EditCommand),
    Forget(ForgetCommand),
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

    #[arg(long, help = "Disable symlink traversal through the portal mount")]
    pub nosymfollow: bool,

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
pub struct StatusCommand {
    /// Workspace to inspect. Defaults to discovering one from the current directory.
    pub workspace: Option<PathBuf>,

    #[arg(
        long,
        help = "Print machine-readable JSON instead of human-readable text"
    )]
    pub json: bool,
}

#[derive(Debug, Parser)]
pub struct StopCommand {
    /// Workspace to stop. Defaults to discovering one from the current directory.
    pub workspace: Option<PathBuf>,

    #[arg(long, help = "Use lazy unmount when stopping the workspace")]
    pub lazy: bool,

    #[arg(long, help = "Override stale state or existing mount conditions")]
    pub force: bool,
}

#[derive(Debug, Parser)]
pub struct CheckCommand {
    /// Workspace to check. Defaults to discovering one from the current directory.
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct EditCommand {
    /// Workspace to edit. Defaults to discovering one from the current directory.
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct ForgetCommand {
    /// Workspace whose stored metadata should be removed.
    pub workspace: PathBuf,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let log_level = match &cli.command {
        Commands::Start(cmd) => cmd.log_level.as_deref(),
        _ => None,
    };
    init_tracing(log_level);

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
                nosymfollow: cmd.nosymfollow,
                adopt: cmd.adopt,
                force: cmd.force,
                log_level: cmd.log_level,
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
        Commands::Forget(cmd) => {
            daemon::forget(ForgetArgs {
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

fn init_tracing(log_level: Option<&str>) {
    let env_filter = if env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    } else {
        tracing_subscriber::EnvFilter::new(log_level.unwrap_or("info"))
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
}
