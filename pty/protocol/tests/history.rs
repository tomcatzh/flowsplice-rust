use anyhow::Result;
use flowsplice_pty_protocol::{
    ClientMessage, MAX_FRAME_BYTES, Operation, Reply, ServerMessage, read_message, write_message,
};
use uuid::Uuid;
fn request(attachment_id: Uuid, capture_id: Uuid, before: Option<u32>) -> ClientMessage {
    ClientMessage::Request {
        request_id: Uuid::new_v4(),
        operation: Operation::History {
            attachment_id,
            capture_id,
            before,
        },
    }
}
fn response(lines: Vec<String>, start: u32, total_lines: u32) -> ServerMessage {
    ServerMessage::Response {
        request_id: Uuid::new_v4(),
        result: Reply::History {
            attachment_id: Uuid::new_v4(),
            capture_id: Uuid::new_v4(),
            total_lines,
            start,
            columns: 80,
            lines,
        },
    }
}
#[tokio::test]
async fn history_roundtrip_and_frame_boundaries() -> Result<()> {
    let request = request(Uuid::new_v4(), Uuid::new_v4(), Some(256));
    request.validate()?;
    let response = response(vec!["\u{1b}[32m中文😀\u{1b}[0m".into(); 256], 0, 256);
    response.validate()?;
    let mut wire = Vec::new();
    write_message(&mut wire, &request).await?;
    write_message(&mut wire, &response).await?;
    assert!(wire.len() < MAX_FRAME_BYTES);
    let mut reader = wire.as_slice();
    assert_eq!(
        read_message::<ClientMessage>(&mut reader).await?,
        Some(request)
    );
    assert_eq!(
        read_message::<ServerMessage>(&mut reader).await?,
        Some(response)
    );
    assert!(reader.is_empty());
    Ok(())
}
#[test]
fn history_rejects_invalid_ids_cursors_pages_and_unknown_fields() -> Result<()> {
    for message in [
        request(Uuid::nil(), Uuid::new_v4(), None),
        request(Uuid::new_v4(), Uuid::nil(), None),
        request(Uuid::new_v4(), Uuid::new_v4(), Some(50_257)),
    ] {
        assert!(message.validate().is_err());
    }
    for message in [
        response(vec!["x".into(); 257], 0, 257),
        response(vec!["x".into()], 2, 2),
        response(vec!["x".into()], 0, 50_257),
        response(vec!["\u{1b}".repeat(20_000)], 0, 1),
    ] {
        assert!(message.validate().is_err());
    }
    let valid = serde_json::to_value(request(Uuid::new_v4(), Uuid::new_v4(), None))?;
    for (field, value) in [
        ("extra", serde_json::json!(true)),
        ("attachment_id", serde_json::json!("bad")),
        ("before", serde_json::json!(-1)),
    ] {
        let mut invalid = valid.clone();
        invalid["operation"][field] = value;
        assert!(serde_json::from_value::<ClientMessage>(invalid).is_err());
    }
    let mut invalid = serde_json::to_value(response(vec!["中文".into()], 0, 1))?;
    invalid["result"]["extra"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ServerMessage>(invalid).is_err());
    Ok(())
}
