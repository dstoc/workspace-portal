use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use tracing::{debug, info};

use crate::{
    error::{Error, Result},
    fs::PortalFs,
    paths,
    protocol::{self, ControlRequest, ControlResponse, ProtocolErrorCode},
    state::{AccessMode, DaemonStatus, EntryRecord, PortalState},
};

use super::{mount::wait_for_mount_state, workspace::persist_workspace_state};

#[derive(Debug)]
pub(crate) struct Daemon {
    config: DaemonConfig,
    state: Arc<RwLock<PortalState>>,
    mount: Option<fuser::BackgroundSession>,
    shutdown: AtomicBool,
}

#[derive(Debug, Clone)]
pub(crate) struct DaemonConfig {
    pub(crate) state: PortalState,
    pub(crate) state_path: std::path::PathBuf,
    pub(crate) registry_path: std::path::PathBuf,
    pub(crate) allow_other: bool,
}

impl Daemon {
    pub(crate) fn new(config: DaemonConfig) -> Self {
        Self {
            state: Arc::new(RwLock::new(config.state.clone())),
            config,
            mount: None,
            shutdown: AtomicBool::new(false),
        }
    }

    pub(crate) fn run(mut self) -> Result<()> {
        self.prepare_runtime()?;

        let socket_path = self.config.state.socket.clone();
        if socket_path.exists() {
            if UnixStream::connect(&socket_path).is_ok() {
                return Err(Error::DaemonAlreadyRunning(
                    self.config.state.workspace.clone(),
                ));
            } else {
                let _ = fs::remove_file(&socket_path);
            }
        }

        self.mount_workspace()?;
        {
            let mut state = self.state.write().unwrap();
            state.daemon = DaemonStatus::Running;
            state.mounted = true;
        }
        self.persist_state()?;

        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(err) => {
                let _ = self.unmount_workspace();
                return Err(err.into());
            }
        };
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        info!(
            workspace = %self.config.state.workspace.display(),
            socket = %socket_path.display(),
            "control daemon started"
        );

        for connection in listener.incoming() {
            let stream = connection?;
            let response = self.handle_connection(stream);

            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            if let Err(err) = response {
                info!(error = %err, "control request failed");
            }
        }

        let _ = self.unmount_workspace();
        {
            let mut state = self.state.write().unwrap();
            state.daemon = DaemonStatus::Stopped;
            state.mounted = false;
        }
        self.persist_state()?;
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    fn handle_connection(&mut self, mut stream: UnixStream) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }

        let request = protocol::decode_request(line.trim_end())?;
        let response = match self.handle_request(request.clone()) {
            Ok(response) => response,
            Err(err) => ControlResponse::Error {
                code: protocol_error_code(&err),
                error: err.to_string(),
            },
        };
        trace_control_request_result(&request, &response);

        let encoded = format!("{}\n", protocol::encode_response(&response)?);
        stream.write_all(encoded.as_bytes())?;
        stream.flush()?;
        stream.shutdown(std::net::Shutdown::Write)?;
        Ok(())
    }

    pub(crate) fn handle_request(&mut self, request: ControlRequest) -> Result<ControlResponse> {
        match request {
            ControlRequest::Ping => Ok(ControlResponse::Ack {
                message: "pong".to_owned(),
            }),
            ControlRequest::Status => Ok(ControlResponse::Status {
                workspace: self.state.read().unwrap().snapshot(),
            }),
            ControlRequest::Add {
                name,
                target,
                mode,
                replace,
            } => {
                paths::validate_entry_name(&name)?;
                let target = super::workspace::canonicalize_target(target)?;
                let entry = EntryRecord::new(name.clone(), target, mode);
                {
                    let mut state = self.state.write().unwrap();
                    state.add_entry(entry, replace)?;
                }
                self.persist_state()?;
                Ok(ControlResponse::Ack {
                    message: format!("added {name}"),
                })
            }
            ControlRequest::Freeze { segment } => {
                paths::validate_immutable_segment_name(&segment)?;
                {
                    let mut state = self.state.write().unwrap();
                    state.freeze_segment(segment.clone());
                }
                self.persist_state()?;
                Ok(ControlResponse::Ack {
                    message: format!("froze {segment}"),
                })
            }
            ControlRequest::Remove { name } => {
                paths::validate_entry_name(&name)?;
                let removed = {
                    let mut state = self.state.write().unwrap();
                    state.remove_entry(&name)?
                };
                self.persist_state()?;
                Ok(ControlResponse::Ack {
                    message: format!("removed {}", removed.name),
                })
            }
            ControlRequest::Thaw { segment } => {
                paths::validate_immutable_segment_name(&segment)?;
                {
                    let mut state = self.state.write().unwrap();
                    state.thaw_segment(&segment);
                }
                self.persist_state()?;
                Ok(ControlResponse::Ack {
                    message: format!("thawed {segment}"),
                })
            }
            ControlRequest::Stop => {
                self.unmount_workspace()?;
                {
                    let mut state = self.state.write().unwrap();
                    state.daemon = DaemonStatus::Stopped;
                    state.mounted = false;
                }
                self.persist_state()?;
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(ControlResponse::Ack {
                    message: "stopping".to_owned(),
                })
            }
        }
    }

    fn prepare_runtime(&self) -> Result<()> {
        if let Some(parent) = self.config.state.socket.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        if let Some(parent) = self.config.state_path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        if let Some(parent) = self.config.registry_path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        Ok(())
    }

    fn persist_state(&self) -> Result<()> {
        let mut state = self.state.read().unwrap().clone();
        state.state_file = self.config.state_path.clone();
        state.socket = self.config.state.socket.clone();
        persist_workspace_state(
            &state,
            &self.config.state_path,
            &self.config.registry_path,
            &self.config.state.socket,
        )
    }

    fn mount_workspace(&mut self) -> Result<()> {
        if self.mount.is_some() {
            return Ok(());
        }

        let mount_session = PortalFs::new(self.state.clone())
            .mount(&self.config.state.workspace, self.config.allow_other)?;
        self.mount = Some(mount_session);
        Ok(())
    }

    fn unmount_workspace(&mut self) -> Result<()> {
        if self.mount.take().is_some() {
            wait_for_mount_state(&self.config.state.workspace, false)?;
        }
        Ok(())
    }
}

fn protocol_error_code(error: &Error) -> ProtocolErrorCode {
    match error {
        Error::EntryExists(_) => ProtocolErrorCode::EntryExists,
        Error::EntryNotFound(_) => ProtocolErrorCode::EntryNotFound,
        Error::InvalidEntryName(_) => ProtocolErrorCode::InvalidName,
        Error::InvalidPortalPath(_) => ProtocolErrorCode::InvalidTarget,
        Error::PermissionDenied(_) | Error::ImmutablePath { .. } => {
            ProtocolErrorCode::PermissionDenied
        }
        Error::TargetNotFound(_) | Error::TargetNotDirectory(_) => ProtocolErrorCode::InvalidTarget,
        Error::DaemonNotRunning(_) => ProtocolErrorCode::DaemonNotRunning,
        Error::DaemonAlreadyRunning(_) => ProtocolErrorCode::DaemonAlreadyRunning,
        Error::WorkspaceNotFound(_) => ProtocolErrorCode::WorkspaceNotFound,
        Error::StateCorrupt(_) => ProtocolErrorCode::StaleState,
        Error::Cli(_) | Error::Protocol(_) | Error::Unsupported(_) => ProtocolErrorCode::Internal,
        Error::Io(_) | Error::Json(_) | Error::InvalidWorkspace(_) => ProtocolErrorCode::Internal,
    }
}

fn trace_control_request_result(request: &ControlRequest, response: &ControlResponse) {
    match request {
        ControlRequest::Add {
            name,
            target,
            mode,
            replace,
        } => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "add",
                    entry_name = %name,
                    target_path = %target.display(),
                    access_mode = access_mode_label(*mode),
                    replace = *replace,
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "add",
                    entry_name = %name,
                    target_path = %target.display(),
                    access_mode = access_mode_label(*mode),
                    replace = *replace,
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "add",
                    entry_name = %name,
                    target_path = %target.display(),
                    access_mode = access_mode_label(*mode),
                    replace = *replace,
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
        ControlRequest::Freeze { segment } => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "freeze",
                    immutable_segment = %segment,
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "freeze",
                    immutable_segment = %segment,
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "freeze",
                    immutable_segment = %segment,
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
        ControlRequest::Remove { name } => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "remove",
                    entry_name = %name,
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "remove",
                    entry_name = %name,
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "remove",
                    entry_name = %name,
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
        ControlRequest::Thaw { segment } => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "thaw",
                    immutable_segment = %segment,
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "thaw",
                    immutable_segment = %segment,
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "thaw",
                    immutable_segment = %segment,
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
        ControlRequest::Status => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "status",
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "status",
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "status",
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
        ControlRequest::Stop => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "stop",
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "stop",
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "stop",
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
        ControlRequest::Ping => match response {
            ControlResponse::Ack { .. } => {
                debug!(
                    operation = "ping",
                    result = "ok",
                    response = "ack",
                    "control request result"
                );
            }
            ControlResponse::Status { .. } => {
                debug!(
                    operation = "ping",
                    result = "ok",
                    response = "status",
                    "control request result"
                );
            }
            ControlResponse::Error { code, error } => {
                debug!(
                    operation = "ping",
                    result = "error",
                    error_code = protocol_error_code_label(code),
                    error = %error,
                    "control request result"
                );
            }
        },
    }
}

fn access_mode_label(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::ReadOnly => "ro",
        AccessMode::ReadWrite => "rw",
    }
}

fn protocol_error_code_label(code: &ProtocolErrorCode) -> &'static str {
    match code {
        ProtocolErrorCode::EntryExists => "entry_exists",
        ProtocolErrorCode::EntryNotFound => "entry_not_found",
        ProtocolErrorCode::InvalidName => "invalid_name",
        ProtocolErrorCode::InvalidTarget => "invalid_target",
        ProtocolErrorCode::PermissionDenied => "permission_denied",
        ProtocolErrorCode::StaleState => "stale_state",
        ProtocolErrorCode::WorkspaceNotFound => "workspace_not_found",
        ProtocolErrorCode::DaemonNotRunning => "daemon_not_running",
        ProtocolErrorCode::DaemonAlreadyRunning => "daemon_already_running",
        ProtocolErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::state::PortalState;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn unique_path(prefix: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        env::temp_dir().join(format!(
            "workspace-portal-runtime-{prefix}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn freeze_and_thaw_requests_persist_state() {
        let workspace = unique_path("workspace");
        let state_path = unique_path("state").join("portal.json");
        let registry_path = unique_path("registry").join("workspace.json");
        let socket = unique_path("runtime").join("portal.sock");
        let state = PortalState::new(workspace.clone(), "abc123".to_owned(), socket.clone())
            .with_storage_paths(state_path.clone());
        let mut daemon = Daemon::new(DaemonConfig {
            state,
            state_path: state_path.clone(),
            registry_path,
            allow_other: false,
        });

        let response = daemon
            .handle_request(ControlRequest::Freeze {
                segment: "vendor".to_owned(),
            })
            .unwrap();
        assert_eq!(
            response,
            ControlResponse::Ack {
                message: "froze vendor".to_owned()
            }
        );

        let persisted = PortalState::load_from_path(&state_path).unwrap();
        assert!(persisted.immutable_segments.contains("vendor"));

        let response = daemon
            .handle_request(ControlRequest::Thaw {
                segment: "vendor".to_owned(),
            })
            .unwrap();
        assert_eq!(
            response,
            ControlResponse::Ack {
                message: "thawed vendor".to_owned()
            }
        );

        let persisted = PortalState::load_from_path(&state_path).unwrap();
        assert!(!persisted.immutable_segments.contains("vendor"));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_file(&state_path);
        let _ = fs::remove_dir_all(state_path.parent().unwrap());
    }
}
