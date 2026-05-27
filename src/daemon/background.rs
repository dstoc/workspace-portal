use std::{
    env, fs,
    io::{BufRead, Read, Seek, SeekFrom, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::{
    error::{Error, Result},
    paths,
    protocol::{self, ControlRequest, ControlResponse},
};

use super::{
    StartArgs, mount::workspace_is_mounted, output::command_in_path, workspace::WorkspaceContext,
};

static CAPTURE_ID: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn socket_is_live(socket: &Path) -> Result<bool> {
    match UnixStream::connect(socket) {
        Ok(mut stream) => {
            let request = protocol::encode_request(&ControlRequest::Ping)?;
            stream.write_all(request.as_bytes())?;
            stream.write_all(b"\n")?;
            stream.shutdown(std::net::Shutdown::Write)?;
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(false);
            }
            match protocol::decode_response(line.trim_end())? {
                ControlResponse::Ack { .. } => Ok(true),
                _ => Ok(false),
            }
        }
        Err(_) => Ok(false),
    }
}

pub(crate) fn wait_for_socket_ready(socket: &Path, workspace: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket_is_live(socket)? && workspace_is_mounted(workspace).unwrap_or(false) {
            return Ok(());
        }

        if Instant::now() > deadline {
            return Err(Error::DaemonNotRunning(socket.to_path_buf()));
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn captured_daemon_output_path(state_root: &Path, label: &str) -> PathBuf {
    let id = CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    state_root.join(format!(
        ".workspace-portal-{label}-{}-{stamp}-{id}.log",
        std::process::id()
    ))
}

fn create_captured_daemon_output_file(path: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)?)
}

fn read_captured_daemon_output(file: &mut fs::File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut output = String::new();
    file.read_to_string(&mut output)?;
    Ok(output)
}

fn format_background_daemon_error(
    status: std::process::ExitStatus,
    stdout: &str,
    stderr: &str,
) -> PathBuf {
    let mut message = format!("background daemon exited before becoming ready: {status}");
    if !stdout.trim().is_empty() {
        message.push_str("\nstdout:\n");
        message.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        message.push_str("\nstderr:\n");
        message.push_str(stderr.trim_end());
    }
    PathBuf::from(message)
}

pub(crate) fn spawn_background_daemon(args: &StartArgs, ctx: &WorkspaceContext) -> Result<()> {
    let exe = env::current_exe()?;
    let state_root = ctx
        .state_path
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .unwrap_or_else(paths::state_root);
    let stdout_path = captured_daemon_output_path(&state_root, "stdout");
    let stderr_path = captured_daemon_output_path(&state_root, "stderr");
    let mut stdout = create_captured_daemon_output_file(&stdout_path)?;
    let mut stderr = create_captured_daemon_output_file(&stderr_path)?;
    let use_setsid = command_in_path("setsid");
    let mut command = if use_setsid {
        let mut command = Command::new("setsid");
        command.arg(exe);
        command
    } else {
        Command::new(exe)
    };
    command
        .arg("start")
        .arg("--daemon-child")
        .arg(&ctx.workspace)
        .arg("--socket")
        .arg(&ctx.socket)
        .arg("--state-dir")
        .arg(state_root)
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .stdin(Stdio::null());

    if args.allow_other {
        command.arg("--allow-other");
    } else {
        command.arg("--no-allow-other");
    }

    if args.read_only {
        command.arg("--read-only");
    }
    if args.adopt {
        command.arg("--adopt");
    }
    if args.force {
        command.arg("--force");
    }
    if let Some(level) = &args.log_level {
        command.arg("--log-level").arg(level);
    }

    let mut child = command.spawn()?;
    for _ in 0..100 {
        if socket_is_live(&ctx.socket)? {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let stdout = read_captured_daemon_output(&mut stdout)?;
            let stderr = read_captured_daemon_output(&mut stderr)?;
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Err(Error::DaemonNotRunning(format_background_daemon_error(
                status, &stdout, &stderr,
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }

    if let Some(status) = child.try_wait()? {
        let stdout = read_captured_daemon_output(&mut stdout)?;
        let stderr = read_captured_daemon_output(&mut stderr)?;
        let _ = fs::remove_file(&stdout_path);
        let _ = fs::remove_file(&stderr_path);
        return Err(Error::DaemonNotRunning(format_background_daemon_error(
            status, &stdout, &stderr,
        )));
    }

    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);
    Err(Error::DaemonNotRunning(ctx.workspace.clone()))
}
