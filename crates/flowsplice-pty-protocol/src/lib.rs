//! Bounded application framing and schemas for `FlowSplice` PTY sessions.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub const APPLICATION_PROTOCOL: &str = "flowsplice.pty.v1";
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 128 * 1024;
pub const MAX_HISTORY_LINES: u32 = 50_256;
pub const MAX_HISTORY_PAGE_LINES: usize = 256;
pub const MAX_HISTORY_PAGE_BYTES: usize = 96 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: Uuid,
    pub created_at_unix_secs: u64,
    pub writer: Option<Writer>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDetails {
    pub id: Uuid,
    pub name: String,
    pub created_at_unix_secs: u64,
    pub last_connected_at_unix_secs: Option<u64>,
    pub connection_count: u32,
    pub writer: Option<Writer>,
}

/// Validate a user-facing session name before creation or renaming.
///
/// # Errors
/// Rejects blank, oversized or control-containing names.
pub fn validate_session_name(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > 256
        || value.chars().count() > 64
        || value.chars().any(char::is_control)
    {
        bail!("session name must contain 1 to 64 characters without controls");
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Writer {
    pub attachment_id: Uuid,
    pub travel_id: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Hello {
        version: u32,
        label: String,
    },
    Request {
        request_id: Uuid,
        operation: Operation,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    List,
    ListDetails,
    Rename {
        session_id: Uuid,
        name: String,
    },
    NewNamed {
        name: String,
        columns: u16,
        rows: u16,
    },
    New {
        columns: u16,
        rows: u16,
    },
    Join {
        session_id: Uuid,
        mode: Mode,
        columns: u16,
        rows: u16,
    },
    SetMode {
        attachment_id: Uuid,
        mode: Mode,
        force: bool,
        expected_epoch: Option<u64>,
    },
    Resize {
        attachment_id: Uuid,
        columns: u16,
        rows: u16,
    },
    Input {
        attachment_id: Uuid,
        writer_epoch: u64,
        data: Vec<u8>,
    },
    Detach {
        attachment_id: Uuid,
    },
    History {
        attachment_id: Uuid,
        capture_id: Uuid,
        before: Option<u32>,
    },
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Hello {
        version: u32,
        can_write: bool,
    },
    Response {
        request_id: Uuid,
        result: Reply,
    },
    Output {
        attachment_id: Uuid,
        data: Vec<u8>,
    },
    Ownership {
        session_id: Uuid,
        epoch: u64,
        writer: Option<Writer>,
    },
    Detached {
        attachment_id: Uuid,
        reason: String,
    },
    SessionEnded {
        session_id: Uuid,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    History {
        attachment_id: Uuid,
        capture_id: Uuid,
        total_lines: u32,
        start: u32,
        columns: u16,
        lines: Vec<String>,
    },
    Sessions {
        sessions: Vec<Session>,
    },
    SessionDetails {
        sessions: Vec<SessionDetails>,
    },
    Attached {
        session: Session,
        attachment_id: Uuid,
        mode: Mode,
        writer_epoch: u64,
    },
    Ok,
    TakeoverRequired {
        session_id: Uuid,
        epoch: u64,
        writer: Writer,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(
    remote = "Operation",
    tag = "op",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum OperationWire {
    List,
    ListDetails,
    Rename {
        session_id: Uuid,
        name: String,
    },
    NewNamed {
        name: String,
        columns: u16,
        rows: u16,
    },
    New {
        columns: u16,
        rows: u16,
    },
    Join {
        session_id: Uuid,
        mode: Mode,
        columns: u16,
        rows: u16,
    },
    SetMode {
        attachment_id: Uuid,
        mode: Mode,
        force: bool,
        expected_epoch: Option<u64>,
    },
    Resize {
        attachment_id: Uuid,
        columns: u16,
        rows: u16,
    },
    Input {
        attachment_id: Uuid,
        writer_epoch: u64,
        data: Vec<u8>,
    },
    Detach {
        attachment_id: Uuid,
    },
    History {
        attachment_id: Uuid,
        capture_id: Uuid,
        before: Option<u32>,
    },
}
impl<'de> Deserialize<'de> for Operation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        // Serde's internally tagged unit variants otherwise ignore additional fields.
        if matches!(
            value.get("op").and_then(serde_json::Value::as_str),
            Some("list" | "list_details")
        ) && value.as_object().is_some_and(|object| object.len() != 1)
        {
            return Err(serde::de::Error::custom("unknown field in unit variant"));
        }
        OperationWire::deserialize(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(
    remote = "Reply",
    tag = "status",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ReplyWire {
    History {
        attachment_id: Uuid,
        capture_id: Uuid,
        total_lines: u32,
        start: u32,
        columns: u16,
        lines: Vec<String>,
    },
    Sessions {
        sessions: Vec<Session>,
    },
    SessionDetails {
        sessions: Vec<SessionDetails>,
    },
    Attached {
        session: Session,
        attachment_id: Uuid,
        mode: Mode,
        writer_epoch: u64,
    },
    Ok,
    TakeoverRequired {
        session_id: Uuid,
        epoch: u64,
        writer: Writer,
    },
    Error {
        code: String,
        message: String,
    },
}
impl<'de> Deserialize<'de> for Reply {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        // Serde's internally tagged unit variants otherwise ignore additional fields.
        if value.get("status").and_then(serde_json::Value::as_str) == Some("ok")
            && value.as_object().is_some_and(|object| object.len() != 1)
        {
            return Err(serde::de::Error::custom("unknown field in unit variant"));
        }
        ReplyWire::deserialize(value).map_err(serde::de::Error::custom)
    }
}

fn identifier(id: Uuid) -> Result<()> {
    if id.is_nil() {
        bail!("nil identifier");
    }
    Ok(())
}
fn text(value: &str, max: usize) -> Result<()> {
    if value.len() > max {
        bail!("text exceeds {max} bytes");
    }
    Ok(())
}
fn label(value: &str) -> Result<()> {
    text(value, 64)?;
    if value.chars().any(char::is_control) {
        bail!("label contains control characters");
    }
    Ok(())
}
fn dimensions(columns: u16, rows: u16) -> Result<()> {
    if !(2..=512).contains(&columns) || !(1..=256).contains(&rows) {
        bail!("terminal dimensions out of range");
    }
    Ok(())
}
fn data(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > 16_384 {
        bail!("data length out of range");
    }
    Ok(())
}
fn version(value: u32) -> Result<()> {
    if value != PROTOCOL_VERSION {
        bail!("unsupported PTY protocol version");
    }
    Ok(())
}
impl Writer {
    fn validate(&self) -> Result<()> {
        identifier(self.attachment_id)?;
        text(&self.travel_id, 128)?;
        if self.travel_id.is_empty() {
            bail!("writer identity must be nonempty");
        }
        label(&self.label)
    }
}
impl Session {
    fn validate(&self) -> Result<()> {
        identifier(self.id)?;
        if let Some(writer) = &self.writer {
            writer.validate()?;
        }
        Ok(())
    }
}
impl Operation {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Rename { session_id, name } => {
                identifier(*session_id)?;
                validate_session_name(name)
            }
            Self::List | Self::ListDetails => Ok(()),
            Self::NewNamed {
                name,
                columns,
                rows,
            } => {
                validate_session_name(name)?;
                dimensions(*columns, *rows)
            }
            Self::New { columns, rows } => dimensions(*columns, *rows),
            Self::Join {
                session_id,
                columns,
                rows,
                ..
            } => {
                identifier(*session_id)?;
                dimensions(*columns, *rows)
            }
            Self::SetMode {
                attachment_id,
                mode,
                force,
                expected_epoch,
            } => {
                identifier(*attachment_id)?;
                if *force && (*mode != Mode::ReadWrite || expected_epoch.is_none()) {
                    bail!("forced mode change requires read-write mode and expected epoch");
                }
                Ok(())
            }
            Self::Resize {
                attachment_id,
                columns,
                rows,
            } => {
                identifier(*attachment_id)?;
                dimensions(*columns, *rows)
            }
            Self::Input {
                attachment_id,
                writer_epoch,
                data: bytes,
            } => {
                identifier(*attachment_id)?;
                if *writer_epoch == 0 {
                    bail!("input requires a positive writer epoch");
                }
                data(bytes)
            }
            Self::Detach { attachment_id } => identifier(*attachment_id),
            Self::History {
                attachment_id,
                capture_id,
                before,
            } => {
                identifier(*attachment_id)?;
                identifier(*capture_id)?;
                if before.is_some_and(|value| value > MAX_HISTORY_LINES) {
                    bail!("history cursor out of range");
                }
                Ok(())
            }
        }
    }
}
impl ClientMessage {
    /// Validate schema bounds without making authorization decisions.
    ///
    /// # Errors
    /// Rejects invalid identifiers, versions, dimensions, labels, payloads or force arguments.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Hello {
                version: value,
                label: name,
            } => {
                version(*value)?;
                label(name)
            }
            Self::Request {
                request_id,
                operation,
            } => {
                identifier(*request_id)?;
                operation.validate()
            }
        }
    }
}
impl Reply {
    fn validate(&self) -> Result<()> {
        match self {
            Self::History {
                attachment_id,
                capture_id,
                total_lines,
                start,
                columns,
                lines,
            } => {
                identifier(*attachment_id)?;
                identifier(*capture_id)?;
                if *total_lines > MAX_HISTORY_LINES
                    || *start > *total_lines
                    || lines.len() > MAX_HISTORY_PAGE_LINES
                    || lines.len() > (*total_lines - *start) as usize
                    || !(1..=512).contains(columns)
                    || (lines.is_empty() && *start != 0)
                    || lines.iter().any(|line| line.contains(['\n', '\r']))
                    || serde_json::to_vec(lines)?.len() > MAX_HISTORY_PAGE_BYTES
                {
                    bail!("invalid history page bounds");
                }
                Ok(())
            }
            Self::SessionDetails { sessions } => {
                if sessions.len() > 128 {
                    bail!("too many sessions");
                }
                for session in sessions {
                    identifier(session.id)?;
                    validate_session_name(&session.name)?;
                    if session.connection_count > 32 {
                        bail!("too many connections");
                    }
                    if let Some(writer) = &session.writer {
                        writer.validate()?;
                    }
                }
                Ok(())
            }
            Self::Sessions { sessions } => {
                if sessions.len() > 128 {
                    bail!("too many sessions");
                }
                for session in sessions {
                    session.validate()?;
                }
                Ok(())
            }
            Self::Attached {
                session,
                attachment_id,
                ..
            } => {
                session.validate()?;
                identifier(*attachment_id)
            }
            Self::Ok => Ok(()),
            Self::TakeoverRequired {
                session_id, writer, ..
            } => {
                identifier(*session_id)?;
                writer.validate()
            }
            Self::Error { code, message } => {
                text(code, 1024)?;
                text(message, 1024)
            }
        }
    }
}
impl ServerMessage {
    /// Validate response and event schema bounds without applying ownership policy.
    ///
    /// # Errors
    /// Rejects invalid identifiers, versions, writer metadata, payloads or list/text bounds.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Hello { version: value, .. } => version(*value),
            Self::Response { request_id, result } => {
                identifier(*request_id)?;
                result.validate()
            }
            Self::Output {
                attachment_id,
                data: bytes,
            } => {
                identifier(*attachment_id)?;
                data(bytes)
            }
            Self::Ownership {
                session_id, writer, ..
            } => {
                identifier(*session_id)?;
                if let Some(writer) = writer {
                    writer.validate()?;
                }
                Ok(())
            }
            Self::Detached {
                attachment_id,
                reason,
            } => {
                identifier(*attachment_id)?;
                text(reason, 1024)
            }
            Self::SessionEnded { session_id } => identifier(*session_id),
        }
    }
}

/// Read one JSON frame; only EOF before the first header byte is a clean end.
/// This function does not invoke schema-specific validation.
///
/// # Errors
/// Returns truncated header/body, invalid length, I/O or JSON errors.
pub async fn read_message<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<T>> {
    let mut header = [0_u8; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let length = usize::try_from(u32::from_be_bytes(header))?;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("invalid PTY frame length");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}
/// Serialize and flush one bounded JSON frame, without schema-specific validation.
///
/// # Errors
/// Returns serialization, invalid length or I/O errors.
pub async fn write_message<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    if body.is_empty() || body.len() > MAX_FRAME_BYTES {
        bail!("invalid PTY frame length");
    }
    writer
        .write_all(&u32::try_from(body.len())?.to_be_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(operation: Operation) -> ClientMessage {
        ClientMessage::Request {
            request_id: Uuid::new_v4(),
            operation,
        }
    }
    fn session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at_unix_secs: 0,
            writer: None,
        }
    }

    #[tokio::test]
    async fn framing_distinguishes_clean_eof_truncation_and_invalid_lengths() -> Result<()> {
        assert!(read_message::<ClientMessage>(&mut &[][..]).await?.is_none());
        for bytes in [
            vec![0],
            vec![0, 0],
            vec![0, 0, 0],
            vec![0, 0, 0, 2, b'{'],
            vec![0, 0, 0, 0],
            u32::try_from(MAX_FRAME_BYTES + 1)?.to_be_bytes().to_vec(),
        ] {
            assert!(
                read_message::<ClientMessage>(&mut bytes.as_slice())
                    .await
                    .is_err()
            );
        }
        Ok(())
    }
    #[tokio::test]
    async fn binary_output_roundtrip_preserves_bytes_and_frame_boundaries() -> Result<()> {
        let message = ServerMessage::Output {
            attachment_id: Uuid::new_v4(),
            data: (0..=255).collect(),
        };
        let next = ServerMessage::Hello {
            version: 1,
            can_write: false,
        };
        let mut wire = Vec::new();
        write_message(&mut wire, &message).await?;
        write_message(&mut wire, &next).await?;
        let mut reader = wire.as_slice();
        assert_eq!(
            read_message::<ServerMessage>(&mut reader).await?,
            Some(message)
        );
        assert_eq!(
            read_message::<ServerMessage>(&mut reader).await?,
            Some(next)
        );
        assert!(read_message::<ServerMessage>(&mut reader).await?.is_none());
        Ok(())
    }
    #[tokio::test]
    async fn oversized_serialization_writes_no_partial_frame() -> Result<()> {
        let mut wire = Vec::new();
        assert!(
            write_message(&mut wire, &"x".repeat(MAX_FRAME_BYTES))
                .await
                .is_err()
        );
        assert!(wire.is_empty());
        Ok(())
    }
    #[test]
    fn strict_schemas_reject_unknown_fields_and_variants() {
        for value in [
            r#"{"type":"hello","version":1,"label":"a","extra":true}"#,
            r#"{"type":"future"}"#,
        ] {
            assert!(serde_json::from_str::<ClientMessage>(value).is_err());
        }
        for value in [r#"{"op":"list","extra":true}"#, r#"{"op":"remove"}"#] {
            assert!(serde_json::from_str::<Operation>(value).is_err());
        }
        for value in [
            r#"{"type":"hello","version":1,"can_write":true,"extra":true}"#,
            r#"{"type":"future"}"#,
        ] {
            assert!(serde_json::from_str::<ServerMessage>(value).is_err());
        }
        for value in [r#"{"status":"ok","extra":true}"#, r#"{"status":"future"}"#] {
            assert!(serde_json::from_str::<Reply>(value).is_err());
        }
    }
    #[test]
    fn client_validation_bounds() -> Result<()> {
        ClientMessage::Hello {
            version: 1,
            label: "é".repeat(32),
        }
        .validate()?;
        for message in [
            ClientMessage::Hello {
                version: 2,
                label: String::new(),
            },
            ClientMessage::Hello {
                version: 1,
                label: "é".repeat(33),
            },
            ClientMessage::Hello {
                version: 1,
                label: "line\n".into(),
            },
            ClientMessage::Request {
                request_id: Uuid::nil(),
                operation: Operation::List,
            },
        ] {
            assert!(message.validate().is_err());
        }
        for (columns, rows, valid) in [
            (2, 1, true),
            (512, 256, true),
            (1, 1, false),
            (513, 1, false),
            (2, 0, false),
            (2, 257, false),
        ] {
            assert_eq!(
                request(Operation::New { columns, rows }).validate().is_ok(),
                valid
            );
        }
        for length in [0, 1, 16_384, 16_385] {
            assert_eq!(
                request(Operation::Input {
                    attachment_id: Uuid::new_v4(),
                    writer_epoch: 1,
                    data: vec![0; length]
                })
                .validate()
                .is_ok(),
                (1..=16_384).contains(&length)
            );
        }
        assert!(
            request(Operation::Input {
                attachment_id: Uuid::new_v4(),
                writer_epoch: 0,
                data: vec![1]
            })
            .validate()
            .is_err()
        );
        for (mode, force, expected_epoch, valid) in [
            (Mode::ReadOnly, false, None, true),
            (Mode::ReadWrite, true, Some(1), true),
            (Mode::ReadOnly, true, Some(1), false),
            (Mode::ReadWrite, true, None, false),
        ] {
            assert_eq!(
                request(Operation::SetMode {
                    attachment_id: Uuid::new_v4(),
                    mode,
                    force,
                    expected_epoch
                })
                .validate()
                .is_ok(),
                valid
            );
        }
        assert!(
            request(Operation::Detach {
                attachment_id: Uuid::nil()
            })
            .validate()
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn rename_requires_stable_identity_and_bounded_name() -> Result<()> {
        let operation = Operation::Rename {
            session_id: Uuid::new_v4(),
            name: "维护 😀 #{session_name}".into(),
        };
        operation.validate()?;
        let wire = serde_json::to_value(&operation)?;
        assert_eq!(wire["op"], "rename");
        assert_eq!(
            serde_json::from_value::<Operation>(wire.clone())?,
            operation
        );
        for (field, value) in [
            ("extra", serde_json::json!(true)),
            ("session_id", serde_json::json!("not-an-id")),
        ] {
            let mut invalid = wire.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<Operation>(invalid).is_err());
        }
        for (id, name) in [
            (Uuid::nil(), "valid".into()),
            (Uuid::new_v4(), "   ".into()),
            (Uuid::new_v4(), "line\nbreak".into()),
            (Uuid::new_v4(), "x".repeat(65)),
            (Uuid::new_v4(), "😀".repeat(65)),
        ] {
            assert!(
                Operation::Rename {
                    session_id: id,
                    name
                }
                .validate()
                .is_err()
            );
        }
        Operation::Rename {
            session_id: Uuid::new_v4(),
            name: "😀".repeat(64),
        }
        .validate()?;
        Ok(())
    }
    #[test]
    fn server_validation_bounds() -> Result<()> {
        let response = |result| ServerMessage::Response {
            request_id: Uuid::new_v4(),
            result,
        };
        response(Reply::Sessions {
            sessions: vec![session(); 128],
        })
        .validate()?;
        assert!(
            response(Reply::Sessions {
                sessions: vec![session(); 129]
            })
            .validate()
            .is_err()
        );
        response(Reply::Error {
            code: "c".repeat(1024),
            message: "m".repeat(1024),
        })
        .validate()?;
        assert!(
            response(Reply::Error {
                code: "c".repeat(1025),
                message: String::new()
            })
            .validate()
            .is_err()
        );
        for length in [0, 1, 16_384, 16_385] {
            assert_eq!(
                ServerMessage::Output {
                    attachment_id: Uuid::new_v4(),
                    data: vec![0; length]
                }
                .validate()
                .is_ok(),
                (1..=16_384).contains(&length)
            );
        }
        let mut writer = Writer {
            attachment_id: Uuid::new_v4(),
            travel_id: "t".repeat(128),
            label: "l".repeat(64),
        };
        writer.validate()?;
        writer.travel_id.push('t');
        assert!(writer.validate().is_err());
        writer.travel_id = "t".into();
        writer.label = "\t".into();
        assert!(writer.validate().is_err());
        writer.label.clear();
        writer.validate()?;
        assert!(
            ServerMessage::SessionEnded {
                session_id: Uuid::nil()
            }
            .validate()
            .is_err()
        );
        assert!(
            ServerMessage::Detached {
                attachment_id: Uuid::new_v4(),
                reason: "x".repeat(1025)
            }
            .validate()
            .is_err()
        );
        Ok(())
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn legacy_wire_shapes_stay_exact() -> Result<()> {
        let id = Uuid::parse_str("11111111-1111-4111-8111-111111111111")?;
        assert_eq!(serde_json::to_string(&Operation::List)?, r#"{"op":"list"}"#);
        assert_eq!(
            serde_json::to_string(&Operation::New {
                columns: 80,
                rows: 24
            })?,
            r#"{"op":"new","columns":80,"rows":24}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Sessions {
                sessions: vec![Session {
                    id,
                    created_at_unix_secs: 1,
                    writer: None
                }]
            })?,
            r#"{"status":"sessions","sessions":[{"id":"11111111-1111-4111-8111-111111111111","created_at_unix_secs":1,"writer":null}]}"#
        );
        Ok(())
    }
    #[test]
    fn named_operations_validate_unicode_and_strict_fields() -> Result<()> {
        for name in ["部署 $(touch nope); ' | #{x}", &"😀".repeat(64)] {
            validate_session_name(name)?;
        }
        for name in ["", "   ", "x\ny", "x\0y", &"😀".repeat(65)] {
            assert!(validate_session_name(name).is_err());
        }
        for wire in [
            r#"{"op":"list_details","extra":1}"#,
            r#"{"op":"new_named","name":"a","columns":80,"rows":24,"extra":1}"#,
        ] {
            assert!(serde_json::from_str::<Operation>(wire).is_err());
        }
        let operation: Operation =
            serde_json::from_str(r#"{"op":"new_named","name":"部署","columns":80,"rows":24}"#)?;
        operation.validate()?;
        let reply = Reply::SessionDetails {
            sessions: vec![SessionDetails {
                id: Uuid::new_v4(),
                name: "部署".into(),
                created_at_unix_secs: 1,
                last_connected_at_unix_secs: None,
                connection_count: 0,
                writer: None,
            }],
        };
        assert_eq!(
            serde_json::from_str::<Reply>(&serde_json::to_string(&reply)?)?,
            reply
        );
        reply.validate()?;
        Ok(())
    }
}
