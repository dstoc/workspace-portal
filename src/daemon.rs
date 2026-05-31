pub(crate) mod background;
pub(crate) mod entry_format;
pub(crate) mod mount;
pub(crate) mod output;
pub(crate) mod runtime;
pub(crate) mod workspace;

use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use crate::{
    error::{Error, Result},
    paths,
    protocol::{self, ControlRequest, ControlResponse, EntryState, StatusPayload},
    state::{AccessMode, DaemonStatus, RevocationMode},
};

use self::{
    background::{socket_is_live, spawn_background_daemon, wait_for_socket_ready},
    mount::{unmount_workspace_from_cli, workspace_is_mounted},
    output::{print_prerequisite_report, print_status},
    runtime::{Daemon, DaemonConfig},
    workspace::{
        WorkspaceContext, canonicalize_target, load_workspace_context, load_workspace_state,
        persist_workspace_state, prepare_workspace_dir,
    },
};

#[derive(Debug, Clone)]
pub struct StartArgs {
    pub workspace: PathBuf,
    pub socket: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub bg: bool,
    pub daemon_child: bool,
    pub allow_other: bool,
    pub read_only: bool,
    pub adopt: bool,
    pub force: bool,
    pub log_level: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AddArgs {
    pub workspace: Option<PathBuf>,
    pub target: PathBuf,
    pub mount_point: String,
    pub read_only: bool,
    pub read_write: bool,
    pub replace: bool,
    pub follow_symlinks: bool,
    pub no_follow_symlinks: bool,
}

#[derive(Debug, Clone)]
pub struct RemoveArgs {
    pub workspace: Option<PathBuf>,
    pub mount_point: String,
    pub revocation: RevocationMode,
}

#[derive(Debug, Clone)]
pub struct StatusArgs {
    pub workspace: Option<PathBuf>,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct StopArgs {
    pub workspace: Option<PathBuf>,
    pub lazy: bool,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct ListArgs;

#[derive(Debug, Clone)]
pub struct CheckArgs {
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct EditArgs {
    pub workspace: Option<PathBuf>,
}

pub async fn start(args: StartArgs) -> Result<()> {
    let workspace = paths::canonical_workspace_path(&args.workspace)?;
    let workspace_ctx = WorkspaceContext::from_workspace(
        workspace.clone(),
        args.state_dir.as_deref(),
        args.socket.clone(),
    );
    if args.daemon_child {
        if !workspace.exists() {
            fs::create_dir_all(&workspace)?;
        }
    } else {
        prepare_workspace_dir(&workspace, args.adopt, args.force)?;
    }

    let existing_state = load_workspace_state(&workspace)?;
    let mut state = existing_state.unwrap_or_else(|| {
        crate::state::PortalState::new(
            workspace.clone(),
            workspace_ctx.workspace_id.clone(),
            workspace_ctx.socket.clone(),
        )
        .with_defaults(args.read_only)
    });

    state.version = 1;
    state.workspace = workspace.clone();
    state.workspace_id = workspace_ctx.workspace_id.clone();
    state.socket = workspace_ctx.socket.clone();
    state.state_file = workspace_ctx.state_path.clone();
    state.mounted = false;
    state.daemon = DaemonStatus::Running;
    state.read_only_default = args.read_only;

    if !args.force && socket_is_live(&workspace_ctx.socket)? {
        return Err(Error::DaemonAlreadyRunning(workspace_ctx.workspace));
    }

    persist_workspace_state(
        &state,
        &workspace_ctx.state_path,
        &workspace_ctx.registry_path,
        &workspace_ctx.socket,
    )?;

    if args.bg && !args.daemon_child {
        spawn_background_daemon(&args, &workspace_ctx)?;
        wait_for_socket_ready(&workspace_ctx.socket, &workspace_ctx.workspace)?;
        return Ok(());
    }

    let daemon = Daemon::new(DaemonConfig {
        state,
        state_path: workspace_ctx.state_path.clone(),
        registry_path: workspace_ctx.registry_path.clone(),
        allow_other: args.allow_other,
    });
    daemon.run()
}

pub async fn add(args: AddArgs) -> Result<()> {
    let (ctx, state) = load_workspace_context(args.workspace.clone())?;
    let target = canonicalize_target(args.target)?;
    let mode = if args.read_only {
        AccessMode::ReadOnly
    } else if args.read_write {
        AccessMode::ReadWrite
    } else if state.read_only_default {
        AccessMode::ReadOnly
    } else {
        AccessMode::ReadWrite
    };
    let mount_point = args.mount_point.clone();

    let request = ControlRequest::Add {
        name: mount_point.clone(),
        target,
        mode,
        replace: args.replace,
    };
    match send_request(&ctx.socket, &request) {
        Ok(response) => ensure_response_ok(response),
        Err(err) => {
            let (_, persisted) = load_workspace_context(args.workspace)?;
            if persisted.entry(&mount_point).is_some() {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

pub async fn remove(args: RemoveArgs) -> Result<()> {
    let (ctx, _) = load_workspace_context(args.workspace.clone())?;
    let mount_point = args.mount_point.clone();
    let request = ControlRequest::Remove {
        name: mount_point.clone(),
        revocation: args.revocation,
    };
    match send_request(&ctx.socket, &request) {
        Ok(response) => ensure_response_ok(response),
        Err(err) => {
            let (_, persisted) = load_workspace_context(args.workspace)?;
            if persisted.entry(&mount_point).is_none() {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

pub async fn status(args: StatusArgs) -> Result<()> {
    let (ctx, live_state) = load_workspace_context(args.workspace)?;
    let socket_live = socket_is_live(&ctx.socket)?;
    let mut snapshot = if socket_live {
        match send_request(&ctx.socket, &ControlRequest::Status)? {
            ControlResponse::Status { workspace } => workspace,
            other => return Err(response_unexpected(other)),
        }
    } else {
        live_state.snapshot()
    };

    if !socket_live {
        snapshot.daemon = DaemonStatus::Stopped;
    }

    snapshot.mounted = workspace_is_mounted(&ctx.workspace).unwrap_or(snapshot.mounted);

    if args.json {
        let payload: StatusPayload = snapshot.clone().into();
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        print_status(snapshot);
    }

    Ok(())
}

pub async fn stop(args: StopArgs) -> Result<()> {
    let (ctx, state) = load_workspace_context(args.workspace)?;
    if !socket_is_live(&ctx.socket)? {
        let mounted = workspace_is_mounted(&ctx.workspace).unwrap_or(state.mounted);
        if !mounted {
            let mut stopped = state.clone();
            stopped.daemon = DaemonStatus::Stopped;
            stopped.mounted = false;
            persist_workspace_state(&stopped, &ctx.state_path, &ctx.registry_path, &ctx.socket)?;
            let _ = fs::remove_file(&ctx.socket);
            return Ok(());
        }

        if args.force {
            if mounted {
                unmount_workspace_from_cli(&ctx.workspace, args.lazy)?;
            }
            let mut stopped = state.clone();
            stopped.daemon = DaemonStatus::Stopped;
            stopped.mounted = workspace_is_mounted(&ctx.workspace).unwrap_or(false);
            persist_workspace_state(&stopped, &ctx.state_path, &ctx.registry_path, &ctx.socket)?;
            let _ = fs::remove_file(&ctx.socket);
            return Ok(());
        }

        return Err(Error::DaemonNotRunning(ctx.workspace));
    }

    let _ = send_request(&ctx.socket, &ControlRequest::Stop);

    let socket_live = || socket_is_live(&ctx.socket).unwrap_or(false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !socket_live() && !workspace_is_mounted(&ctx.workspace).unwrap_or(false) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    if args.lazy {
        unmount_workspace_from_cli(&ctx.workspace, true)?;
        return Ok(());
    }

    if !socket_live() && !workspace_is_mounted(&ctx.workspace).unwrap_or(false) {
        return Ok(());
    }

    Err(Error::DaemonNotRunning(ctx.workspace))
}

pub async fn list(_args: ListArgs) -> Result<()> {
    let mut rows = Vec::new();
    let root = paths::state_root().join("workspaces");
    if root.exists() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }

            let state = match crate::state::PortalState::load_from_path(&path) {
                Ok(state) => state,
                Err(err) => {
                    rows.push((
                        path.clone(),
                        DaemonStatus::Unknown,
                        0usize,
                        Some(err.to_string()),
                    ));
                    continue;
                }
            };

            let live = socket_is_live(&state.socket).unwrap_or(false);
            let status = if live {
                DaemonStatus::Running
            } else {
                DaemonStatus::Stopped
            };
            rows.push((state.workspace.clone(), status, state.entries.len(), None));
        }
    }

    rows.sort_by(|left, right| left.0.cmp(&right.0));
    println!("{:<38} {:<8} {}", "WORKSPACE", "STATUS", "ENTRIES");
    for (workspace, status, entries, error) in rows {
        if let Some(error) = error {
            println!(
                "{:<38} {:<8} {} ({error})",
                workspace.display(),
                "corrupt",
                0
            );
            continue;
        }

        let status = match status {
            DaemonStatus::Running => "running",
            DaemonStatus::Stopped => "stopped",
            DaemonStatus::Unknown => "unknown",
        };
        println!("{:<38} {:<8} {}", workspace.display(), status, entries);
    }

    Ok(())
}

pub async fn check(args: CheckArgs) -> Result<()> {
    let candidate = match args.workspace {
        Some(workspace) => paths::canonical_workspace_path(workspace)?,
        None => match paths::discover_workspace(env::current_dir()?) {
            Ok(workspace) => workspace,
            Err(err) => {
                print_prerequisite_report(None, None, false, false, Some(err.to_string()));
                return Ok(());
            }
        },
    };

    let (ctx, state) = match load_workspace_context(Some(candidate.clone())) {
        Ok(result) => result,
        Err(err) => {
            print_prerequisite_report(Some(candidate), None, false, false, Some(err.to_string()));
            return Ok(());
        }
    };

    let fuse_device = Path::new("/dev/fuse");
    let fuse_available = fuse_device.exists()
        && fs::metadata(fuse_device)
            .map(|m| m.file_type().is_char_device())
            .unwrap_or(false);
    let fusermount3_available = output::command_in_path("fusermount3");
    let socket_live = socket_is_live(&ctx.socket).unwrap_or(false);

    print_prerequisite_report(
        Some(state.workspace.clone()),
        Some(ctx.socket.clone()),
        fuse_available,
        fusermount3_available,
        None,
    );

    println!(
        "State file: {}",
        if ctx.state_path.exists() {
            ctx.state_path.display().to_string()
        } else {
            "<missing>".to_owned()
        }
    );
    println!(
        "Socket: {} ({})",
        ctx.socket.display(),
        if socket_live {
            "reachable"
        } else {
            "unreachable"
        }
    );
    println!("Entries: {}", state.entries.len());

    Ok(())
}

pub async fn edit(args: EditArgs) -> Result<()> {
    let (ctx, live_state) = load_workspace_context(args.workspace)?;

    // Step 2: Determine the authoritative "before" entry set.
    let before: Vec<EntryState> = if socket_is_live(&ctx.socket)? {
        match send_request(&ctx.socket, &ControlRequest::Status)? {
            ControlResponse::Status { workspace } => {
                workspace.entries.into_iter().map(Into::into).collect()
            }
            other => return Err(response_unexpected(other)),
        }
    } else {
        live_state
            .snapshot()
            .entries
            .into_iter()
            .map(Into::into)
            .collect()
    };

    // Step 3: Render the initial buffer.
    let original_buffer = entry_format::render_entries(&before, true);
    let original_bytes = original_buffer.as_bytes().to_vec();

    // Step 4: Write to a temp file.
    let temp_path = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "workspace-portal-edit-{}-{}.conf",
            std::process::id(),
            nanos
        ))
    };

    fs::write(&temp_path, &original_bytes)?;
    let _guard = TempFileGuard {
        path: temp_path.clone(),
    };

    // Step 5: Resolve the editor.
    let editor = std::env::var_os("VISUAL")
        .or_else(|| std::env::var_os("EDITOR"))
        .unwrap_or_else(|| std::ffi::OsString::from("vi"));

    // Step 6: Editor round-trip loop.
    let mut prev_bytes: Option<Vec<u8>> = None;
    let after: Vec<EntryState> = loop {
        // Launch the editor.
        let status = std::process::Command::new(&editor)
            .arg(&temp_path)
            .status()
            .map_err(Error::Io)?;

        if !status.success() {
            println!("no changes");
            return Ok(());
        }

        // Read the file back.
        let current_bytes = fs::read(&temp_path)?;

        // If byte-identical to the ORIGINAL buffer on first pass → no changes.
        if prev_bytes.is_none() && current_bytes == original_bytes {
            println!("no changes");
            return Ok(());
        }

        // Parse and validate.
        let text = String::from_utf8_lossy(&current_bytes);
        match entry_format::parse_entries(&text).and_then(|parsed| {
            entry_format::validate_entries(&parsed)?;
            Ok(parsed)
        }) {
            Ok(parsed) => break parsed,
            Err(err) => {
                // If bytes are unchanged from the previous failed iteration → give up.
                // (main prints the returned error, so don't echo it here.)
                if let Some(ref prev) = prev_bytes {
                    if current_bytes == *prev {
                        return Err(err);
                    }
                }
                eprintln!("{err}");
                eprintln!("(The buffer will be reopened for further editing.)");
                prev_bytes = Some(current_bytes);
                // Loop: re-launch the editor on the same temp file.
            }
        }
    };

    // Step 7: Compute the diff.
    let plan = entry_format::plan_edit(&before, &after);
    if plan.is_empty() {
        println!("no changes");
        return Ok(());
    }

    // Step 8: Send each request in the plan.
    for req in &plan {
        let response = send_request(&ctx.socket, req)?;
        ensure_response_ok(response)?;
    }

    // Step 9: Report success.
    println!("applied {} change(s)", plan.len());
    Ok(())
}

/// RAII guard that removes a temporary file when dropped.
struct TempFileGuard {
    path: PathBuf,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn send_request(socket: &Path, request: &ControlRequest) -> Result<ControlResponse> {
    let mut stream =
        UnixStream::connect(socket).map_err(|_| Error::DaemonNotRunning(socket.to_path_buf()))?;
    let encoded = protocol::encode_request(request)?;
    stream.write_all(encoded.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(Error::Protocol(
            "daemon closed the connection without a response".to_owned(),
        ));
    }

    let response = protocol::decode_response(line.trim_end())?;
    Ok(response)
}

fn ensure_response_ok(response: ControlResponse) -> Result<()> {
    match response {
        ControlResponse::Ack { .. } => Ok(()),
        ControlResponse::Error { code, error } => {
            Err(Error::Protocol(format!("{code:?}: {error}")))
        }
        ControlResponse::Status { .. } => Err(Error::Protocol(
            "unexpected status response from control operation".to_owned(),
        )),
    }
}

fn response_unexpected(response: ControlResponse) -> Error {
    Error::Protocol(format!("unexpected control response: {response:?}"))
}
