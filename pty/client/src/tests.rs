use super::*;
use flowsplice_pty_protocol::read_message;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

async fn pair() -> Result<(PtyClient, mpsc::Receiver<ServerMessage>, DuplexStream)> {
    let (client, mut server) = tokio::io::duplex(4096);
    let handshake = async {
        let hello: ClientMessage = read_message(&mut server).await?.context("missing Hello")?;
        assert_eq!(
            hello,
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                label: "test".to_owned()
            }
        );
        write_message(
            &mut server,
            &ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                can_write: true,
            },
        )
        .await?;
        let list: ClientMessage = read_message(&mut server).await?.context("missing List")?;
        let ClientMessage::Request {
            request_id,
            operation: Operation::List,
        } = list
        else {
            bail!("expected explicit List, never New");
        };
        write_message(
            &mut server,
            &ServerMessage::Response {
                request_id,
                result: Reply::Sessions { sessions: vec![] },
            },
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    };
    let (client, server_result) =
        tokio::join!(PtyClient::from_stream(client, "test".to_owned()), handshake);
    server_result?;
    let (client, events) = client?;
    Ok((client, events, server))
}

#[tokio::test]
async fn handshake_order_pending_new_and_supplied_epoch() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client,mut events,mut server) = pair().await?;
        assert!(matches!(events.recv().await,Some(ServerMessage::Hello { .. })));
        assert!(matches!(events.recv().await,Some(ServerMessage::Response { result: Reply::Sessions { .. }, .. })));
        let (first,second) = tokio::join!(client.send(Operation::New { columns:80,rows:24 }),client.send(Operation::New { columns:80,rows:24 }));
        assert_ne!(first.is_ok(),second.is_ok());
        let request_id = first.or(second)?;
        let request: ClientMessage = read_message(&mut server).await?.context("missing New")?;
        assert!(matches!(request,ClientMessage::Request { request_id:id, operation:Operation::New { .. } } if id==request_id));
        write_message(&mut server,&ServerMessage::Response { request_id,result:Reply::Ok }).await?;
        events.recv().await.context("missing response")?;
        client.send(Operation::New { columns:80,rows:24 }).await?;
        let _: ClientMessage = read_message(&mut server).await?.context("missing next New")?;
        let operation = Operation::Input { attachment_id:Uuid::new_v4(),writer_epoch:7,data:vec![0,255,10] };
        let id = client.send(operation.clone()).await?;
        assert_eq!(read_message::<ClientMessage>(&mut server).await?,Some(ClientMessage::Request { request_id:id,operation }));
        let peer_close = async move {
            assert_eq!(server.read(&mut [0]).await?,0);
            drop(server);
            Ok::<_, anyhow::Error>(())
        };
        let ((), peer_result) = tokio::join!(client.shutdown(), peer_close);
        peer_result?;
        assert!(client.send(Operation::List).await.is_err());
        Ok::<_,anyhow::Error>(())
    }).await.context("test timed out")?
}

#[tokio::test]
async fn overflowing_events_closes_io_and_releases_pending_new() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, _events, mut server) = pair().await?;
        client
            .send(Operation::New {
                columns: 80,
                rows: 24,
            })
            .await?;
        let _: ClientMessage = read_message(&mut server).await?.context("missing New")?;
        for _ in 0..CAPACITY {
            let message = ServerMessage::Output {
                attachment_id: Uuid::new_v4(),
                data: vec![42],
            };
            if write_message(&mut server, &message).await.is_err() {
                break;
            }
        }
        let peer_close = tokio::spawn(async move {
            let mut remaining = Vec::new();
            server.read_to_end(&mut remaining).await?;
            drop(server);
            Ok::<_, anyhow::Error>(())
        });
        let mut finished = client.finished.clone();
        finished.wait_for(|done| *done).await?;
        peer_close.await??;
        assert!(
            client
                .pending_new
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .is_none()
        );
        assert!(client.send(Operation::List).await.is_err());
        client.shutdown().await;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("overflow test timed out")?
}

#[tokio::test]
async fn peer_eof_and_drop_close_connection() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, _events, server) = pair().await?;
        drop(server);
        let mut finished = client.finished.clone();
        finished.wait_for(|done| *done).await?;
        assert!(client.send(Operation::List).await.is_err());
        client.shutdown().await;
        let (client, _events, mut server) = pair().await?;
        drop(client);
        assert_eq!(server.read(&mut [0]).await?, 0);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("closure test timed out")?
}

#[tokio::test]
async fn full_request_queue_rejects_unadmitted_input_without_disconnect() -> Result<()> {
    let (client, _events, mut server) = pair().await?;
    // No yield while enqueueing: force backpressure without a broken transport.
    for _ in 0..CAPACITY {
        client.send(Operation::List).await?;
    }
    assert!(
        client
            .send(Operation::Input {
                attachment_id: Uuid::new_v4(),
                writer_epoch: 1,
                data: b"must not be silently skipped".to_vec()
            })
            .await
            .is_err()
    );
    assert!(!*client.cancel.borrow());
    // Drain the admitted operations; the rejected input must never appear.
    for _ in 0..CAPACITY {
        assert!(matches!(
            read_message::<ClientMessage>(&mut server).await?,
            Some(ClientMessage::Request {
                operation: Operation::List,
                ..
            })
        ));
    }
    let id = client.send(Operation::ListDetails).await?;
    assert_eq!(
        read_message::<ClientMessage>(&mut server).await?,
        Some(ClientMessage::Request {
            request_id: id,
            operation: Operation::ListDetails
        })
    );
    let peer_close = async move {
        let mut remaining = Vec::new();
        server.read_to_end(&mut remaining).await?;
        drop(server);
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        let ((), peer_result) = tokio::join!(client.shutdown(), peer_close);
        peer_result
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn shutdown_half_closes_before_waiting_for_live_peer_eof() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, _events, mut server) = pair().await?;
        let shutdown = client.shutdown();
        tokio::pin!(shutdown);
        // The peer still owns both halves: it observes a byte-level EOF before
        // closing its own stream, as a real PTY Home must to release its writer.
        let mut byte = [0];
        tokio::select! {
            () = &mut shutdown => bail!("shutdown completed before peer observed half-close"),
            result = server.read(&mut byte) => assert_eq!(result?, 0),
        }
        // EOF on the peer's read half must not mean the client's read half was
        // dropped: final peer bytes can still be delivered during orderly close.
        server.write_all(&[0x5a]).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
                .await
                .is_err()
        );
        drop(server);
        tokio::time::timeout(Duration::from_secs(1), &mut shutdown)
            .await
            .context("shutdown did not finish after peer EOF")?;
        assert!(client.send(Operation::List).await.is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("half-close regression timed out")?
}

#[tokio::test]
async fn named_and_legacy_create_share_one_pending_guard() -> Result<()> {
    let (client, mut events, mut server) = pair().await?;
    events.recv().await.context("missing Hello")?;
    events.recv().await.context("missing initial list")?;
    let named = Operation::NewNamed {
        name: "部署终端".into(),
        columns: 80,
        rows: 24,
    };
    let id = client.send(named.clone()).await?;
    assert!(client.send(named.clone()).await.is_err());
    assert!(
        client
            .send(Operation::New {
                columns: 80,
                rows: 24
            })
            .await
            .is_err()
    );
    assert_eq!(
        read_message::<ClientMessage>(&mut server).await?,
        Some(ClientMessage::Request {
            request_id: id,
            operation: named.clone()
        })
    );
    write_message(
        &mut server,
        &ServerMessage::Response {
            request_id: id,
            result: Reply::Error {
                code: "test".into(),
                message: "not created".into(),
            },
        },
    )
    .await?;
    events.recv().await.context("missing create result")?;
    client.send(named).await?;
    drop(server);
    client.shutdown().await;
    Ok(())
}

/// Holds cleanup at an explicit boundary, independent of scheduler timing.
struct ShutdownGate {
    stream: DuplexStream,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: tokio::sync::oneshot::Receiver<()>,
}

impl AsyncRead for ShutdownGate {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for ShutdownGate {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        assert!(
            std::task::ready!(std::future::Future::poll(
                std::pin::Pin::new(&mut self.release),
                cx
            ))
            .is_ok(),
            "shutdown gate must be explicitly released"
        );
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn peer_eof_rejects_input_while_shutdown_cleanup_is_blocked() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (stream, mut server) = tokio::io::duplex(4096);
        let (entered, cleanup_entered) = tokio::sync::oneshot::channel();
        let (release, cleanup_release) = tokio::sync::oneshot::channel();
        let stream = ShutdownGate {
            stream,
            entered: Some(entered),
            release: cleanup_release,
        };
        let handshake = async {
            assert!(matches!(
                read_message::<ClientMessage>(&mut server).await?,
                Some(ClientMessage::Hello { .. })
            ));
            write_message(
                &mut server,
                &ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    can_write: true,
                },
            )
            .await?;
            let Some(ClientMessage::Request {
                request_id,
                operation: Operation::List,
            }) = read_message::<ClientMessage>(&mut server).await?
            else {
                bail!("missing initial List");
            };
            write_message(
                &mut server,
                &ServerMessage::Response {
                    request_id,
                    result: Reply::Sessions { sessions: vec![] },
                },
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        };
        let (client, handshake) = tokio::join!(
            PtyClient::from_stream(stream, "cleanup-test".to_owned()),
            handshake
        );
        handshake?;
        let (client, mut events) = client?;
        assert!(matches!(
            events.recv().await,
            Some(ServerMessage::Hello { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ServerMessage::Response { .. })
        ));
        server.shutdown().await?;
        // The receive loop has ended, but cleanup cannot finish until released.
        cleanup_entered.await?;
        assert!(!*client.finished.borrow());
        assert!(
            client
                .send(Operation::Input {
                    attachment_id: Uuid::new_v4(),
                    writer_epoch: 1,
                    data: b"must reject".to_vec(),
                })
                .await
                .is_err(),
            "closed transport accepted input during cleanup"
        );
        assert!(client.send(Operation::List).await.is_err());
        assert!(!*client.finished.borrow());
        assert!(release.send(()).is_ok(), "cleanup gate dropped early");
        assert!(events.recv().await.is_none());
        client.shutdown().await;
        assert!(*client.finished.borrow());
        assert_eq!(server.read(&mut [0]).await?, 0);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("cleanup admission test timed out")?
}
