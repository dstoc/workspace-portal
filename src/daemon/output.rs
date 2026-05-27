use std::{env, path::PathBuf};

use crate::state::{AccessMode, DaemonStatus, WorkspaceSnapshot};

pub(crate) fn print_status(snapshot: WorkspaceSnapshot) {
    println!("Workspace: {}", snapshot.workspace.display());
    println!(
        "Mount:     {}",
        if snapshot.mounted {
            "mounted"
        } else {
            "not mounted"
        }
    );
    println!(
        "Daemon:    {}",
        match snapshot.daemon {
            DaemonStatus::Running => "running",
            DaemonStatus::Stopped => "stopped",
            DaemonStatus::Unknown => "unknown",
        }
    );
    println!("Socket:    {}", snapshot.socket.display());
    println!();
    println!("{:<10} {:<4} TARGET", "ENTRY", "MODE");
    println!("{:<10} {:<4} ------", "-----", "----");
    for entry in snapshot.entries {
        let mode = match entry.mode {
            AccessMode::ReadOnly => "ro",
            AccessMode::ReadWrite => "rw",
        };
        println!("{:<10} {:<4} {}", entry.name, mode, entry.target.display());
    }
}

pub(crate) fn print_prerequisite_report(
    workspace: Option<PathBuf>,
    socket: Option<PathBuf>,
    fuse_available: bool,
    fusermount3_available: bool,
    note: Option<String>,
) {
    if let Some(workspace) = workspace {
        println!("Workspace: {}", workspace.display());
    } else {
        println!("Workspace: <none>");
    }

    if let Some(socket) = socket {
        println!("Socket:    {}", socket.display());
    }

    println!(
        "/dev/fuse: {}",
        if fuse_available {
            "available"
        } else {
            "unavailable"
        }
    );
    println!(
        "fusermount3: {}",
        if fusermount3_available {
            "available"
        } else {
            "unavailable"
        }
    );

    if let Some(note) = note {
        println!("Note: {note}");
    }
}

pub(crate) fn command_in_path(binary: &str) -> bool {
    let path = match env::var_os("PATH") {
        Some(path) => path,
        None => return false,
    };

    env::split_paths(&path).any(|candidate| candidate.join(binary).exists())
}
