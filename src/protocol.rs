use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::state::{AccessMode, DaemonStatus, EntryRecord, WorkspaceSnapshot};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    Add {
        name: String,
        target: PathBuf,
        mode: AccessMode,
        replace: bool,
    },
    Remove {
        name: String,
    },
    Status,
    Stop,
    Ping,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponse {
    Ack {
        message: String,
    },
    Status {
        workspace: WorkspaceSnapshot,
    },
    Error {
        code: ProtocolErrorCode,
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    EntryExists,
    EntryNotFound,
    InvalidName,
    InvalidTarget,
    PermissionDenied,
    StaleState,
    WorkspaceNotFound,
    DaemonNotRunning,
    DaemonAlreadyRunning,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EntryState {
    pub name: String,
    pub target: PathBuf,
    pub mode: AccessMode,
}

impl From<EntryRecord> for EntryState {
    fn from(value: EntryRecord) -> Self {
        Self {
            name: value.name,
            target: value.target,
            mode: value.mode,
        }
    }
}

impl From<EntryState> for EntryRecord {
    fn from(value: EntryState) -> Self {
        EntryRecord::new(value.name, value.target, value.mode)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StatusPayload {
    pub workspace: PathBuf,
    pub mounted: bool,
    pub daemon: DaemonStatus,
    pub socket: PathBuf,
    pub entries: Vec<EntryState>,
}

impl From<WorkspaceSnapshot> for StatusPayload {
    fn from(value: WorkspaceSnapshot) -> Self {
        Self {
            workspace: value.workspace,
            mounted: value.mounted,
            daemon: value.daemon,
            socket: value.socket,
            entries: value.entries.into_iter().map(Into::into).collect(),
        }
    }
}

pub fn decode_request(line: &str) -> crate::Result<ControlRequest> {
    Ok(serde_json::from_str(line)?)
}

pub fn encode_request(request: &ControlRequest) -> crate::Result<String> {
    Ok(serde_json::to_string(request)?)
}

pub fn encode_response(response: &ControlResponse) -> crate::Result<String> {
    Ok(serde_json::to_string(response)?)
}

pub fn decode_response(line: &str) -> crate::Result<ControlResponse> {
    Ok(serde_json::from_str(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AccessMode, DaemonStatus, EntryRecord, WorkspaceSnapshot};

    #[test]
    fn request_roundtrips_through_json() {
        let request = ControlRequest::Add {
            name: "docs".to_owned(),
            target: PathBuf::from("/tmp/docs"),
            mode: AccessMode::ReadWrite,
            replace: true,
        };

        let encoded = encode_request(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"op":"add","name":"docs","target":"/tmp/docs","mode":"rw","replace":true}"#
        );
        assert_eq!(decode_request(&encoded).unwrap(), request);
    }

    #[test]
    fn response_roundtrips_through_json() {
        let response = ControlResponse::Status {
            workspace: WorkspaceSnapshot {
                workspace: PathBuf::from("/workspace"),
                mounted: false,
                daemon: DaemonStatus::Running,
                socket: PathBuf::from("/run/socket.sock"),
                entries: vec![EntryRecord::new(
                    "docs",
                    PathBuf::from("/tmp/docs"),
                    AccessMode::ReadOnly,
                )],
                generation: 7,
            },
        };

        let encoded = encode_response(&response).unwrap();
        assert!(encoded.contains(r#""kind":"status""#));
        assert_eq!(decode_response(&encoded).unwrap(), response);
    }

    #[test]
    fn status_payload_preserves_snapshot_fields() {
        let snapshot = WorkspaceSnapshot {
            workspace: PathBuf::from("/workspace"),
            mounted: true,
            daemon: DaemonStatus::Stopped,
            socket: PathBuf::from("/run/socket.sock"),
            entries: vec![EntryRecord::new(
                "docs",
                PathBuf::from("/tmp/docs"),
                AccessMode::ReadWrite,
            )],
            generation: 3,
        };

        let payload = StatusPayload::from(snapshot);
        assert_eq!(payload.entries.len(), 1);
        assert_eq!(payload.entries[0].name, "docs");
        assert_eq!(payload.daemon, DaemonStatus::Stopped);
    }
}
