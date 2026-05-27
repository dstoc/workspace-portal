use std::{
    fs,
    os::unix::ffi::OsStrExt,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use crate::error::{Error, Result};

use super::output::command_in_path;

pub(crate) fn wait_for_mount_state(workspace: &Path, expected: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if workspace_is_mounted(workspace)? == expected {
            return Ok(());
        }

        if Instant::now() > deadline {
            return Err(Error::DaemonNotRunning(workspace.to_path_buf()));
        }

        thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn workspace_is_mounted(workspace: &Path) -> Result<bool> {
    let workspace_bytes = workspace.as_os_str().as_bytes().to_vec();
    let mountinfo = match fs::read_to_string("/proc/self/mountinfo") {
        Ok(text) => text,
        Err(_) => return Ok(false),
    };

    for line in mountinfo.lines() {
        let Some(mount_point) = line.split_whitespace().nth(4) else {
            continue;
        };
        if decode_mount_path(mount_point.as_bytes()) == workspace_bytes {
            return Ok(true);
        }
    }

    Ok(false)
}

pub(crate) fn decode_mount_path(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'\\' && i + 3 < input.len() {
            let digits = &input[i + 1..i + 4];
            if digits
                .iter()
                .all(|byte| byte.is_ascii_digit() && *byte < b'8')
            {
                let value = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
                output.push(value);
                i += 4;
                continue;
            }
        }

        output.push(input[i]);
        i += 1;
    }

    output
}

pub(crate) fn unmount_workspace_from_cli(workspace: &Path, lazy: bool) -> Result<()> {
    if !workspace_is_mounted(workspace)? {
        return Ok(());
    }

    let mut attempts = Vec::new();
    if command_in_path("fusermount3") {
        attempts.push("fusermount3");
    }
    if command_in_path("umount") {
        attempts.push("umount");
    }

    for binary in attempts {
        let mut command = Command::new(binary);
        match binary {
            "fusermount3" => {
                command.arg("-u");
                if lazy {
                    command.arg("-z");
                }
                command.arg("--").arg(workspace);
            }
            "umount" => {
                if lazy {
                    command.arg("-l");
                }
                command.arg(workspace);
            }
            _ => unreachable!(),
        }

        if command
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
    }

    Err(Error::Cli(format!(
        "failed to unmount workspace {}",
        workspace.display()
    )))
}
