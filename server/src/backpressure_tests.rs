use super::*;
use aws_lc_rs::{rand::SystemRandom, signature::ECDSA_P256_SHA256_ASN1_SIGNING};
use serde_json::json;

struct Fixture {
    state: Arc<State>,
    config: Config,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("test key generation failed"))?;
        // These tests exercise queue/lifecycle boundaries, not trust validation.
        // The signer and its signed payload are entirely isolated test material.
        let trust: DeploymentTrust = serde_json::from_value(json!({
            "version":1,"deployment_id":"test","generation":1,
            "not_before_unix_secs":1,"not_after_unix_secs":u64::MAX,
            "management_ca_certificate_pem":"test","business_ca_certificate_pem":"test",
            "server_control_keys":[],"home_endpoints":[],
            "home_enrollment_authorities":[],"travel_authorities":[]
        }))?;
        let payload = serde_json::to_vec(&trust)?;
        let signature = key
            .sign(&SystemRandom::new(), &payload)
            .map_err(|_| anyhow!("test signing failed"))?;
        let signer = ControlSigner {
            server_id: "server".into(),
            signer_epoch: 1,
            signed_trust: SignedDeploymentTrust {
                payload_hex: hex::encode(payload),
                signature_hex: hex::encode(signature.as_ref()),
            },
            root_public_key: hex::encode(key.public_key().as_ref()),
            trust,
            key,
            next_generation: StdMutex::new(1),
            generation_path: path.join("generation.json"),
            ttl_secs: 120,
        };
        let config: Config = serde_json::from_value(json!({
            "id":"server","control_listen":"127.0.0.1:0","relays":[],
            "cert":path.join("cert"),"key":path.join("key"),"management_ca":path.join("ca"),
            "deployment_root_public_key":path.join("root"),"deployment_trust":path.join("trust"),
            "control_signing_key":path.join("signer"),"homes":[],
            "travel_authorization_state":path.join("authorization.json"),
            "control_generation_state":path.join("generation.json"),"state_store":path.join("state.redb"),
            "ui_listen":"127.0.0.1:0","work_ttl_secs":0
        }))?;
        fs::write(
            &config.travel_authorization_state,
            serde_json::to_vec(&json!({
                "version":1,"snapshot":{"generation":1,"home_endpoint_credentials":[],"credentials":[],"revocations":[]},
                "used_enrollment_requests":[]
            }))?,
        )?;
        let authorization = ServerAuthorization::load(
            "test".into(),
            vec![
                flowsplice_core::authorization::TrustedTravelAuthority::Home {
                    id: "home-authority".into(),
                    epoch: 1,
                    home_id: "home".into(),
                    public_key: signer.root_public_key.clone(),
                },
            ],
            config.travel_authorization_state.clone(),
        )?;
        let state = Arc::new(State::new(
            authorization,
            &[],
            signer,
            StateStore::open(&config.state_store)?,
        )?);
        Ok(Self {
            state,
            config,
            directory,
        })
    }

    async fn pending(
        &self,
        id: Uuid,
        installed: bool,
        expired: bool,
    ) -> Result<mpsc::Receiver<ControlMessage>> {
        let (relay, rx) = mpsc::channel(64);
        self.state.pending_enrollments.lock().await.insert(
            id,
            PendingEnrollment {
                forwarding_id: Uuid::new_v4(),
                relay_id: "relay".into(),
                travel_id: "travel".into(),
                home_id: "home".into(),
                relay,
                bootstrap: false,
                installed,
                expires: if expired {
                    Instant::now()
                        .checked_sub(Duration::from_secs(1))
                        .context("test clock must permit an expired deadline")?
                } else {
                    Instant::now() + Duration::from_secs(60)
                },
            },
        );
        Ok(rx)
    }
}

fn relay() -> ConfiguredRelay {
    ConfiguredRelay {
        id: "relay".into(),
        connection_mode: RelayConnectionMode::Active,
        connect_addr: None,
    }
}

fn full_queue() -> (mpsc::Sender<ControlMessage>, mpsc::Receiver<ControlMessage>) {
    let (tx, rx) = mpsc::channel(64);
    for nonce in 0..64 {
        assert!(tx.try_send(ControlMessage::Heartbeat { nonce }).is_ok());
    }
    (tx, rx)
}

#[tokio::test]
async fn initial_latest_snapshot_cannot_be_followed_by_older_queued_authorization() -> Result<()> {
    let f = Fixture::new()?;
    // Clone the template outside any async lock; the wire messages below are independent.
    let mut template = f.state.authorization.read().await.snapshot();
    template.generation = 2;
    let old = ControlMessage::TravelAuthorizationSnapshot {
        snapshot: template.clone(),
    };
    let (tx, mut rx) = mpsc::channel(64);
    for _ in 0..63 {
        tx.try_send(old.clone())?;
    }
    tx.try_send(ControlMessage::Heartbeat { nonce: 99 })?;
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    let mut generation = 1;
    template.generation = 3;
    write_relay_control(
        &mut writer,
        &ControlMessage::TravelAuthorizationSnapshot {
            snapshot: template.clone(),
        },
        &mut generation,
    )
    .await?;
    while let Ok(message) = rx.try_recv() {
        write_relay_control(&mut writer, &message, &mut generation).await?;
    }
    template.generation = 4;
    write_relay_control(
        &mut writer,
        &ControlMessage::TravelAuthorizationSnapshot { snapshot: template },
        &mut generation,
    )
    .await?;
    drop(writer);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    assert!(
        matches!(reader.read::<ControlMessage>().await?, ControlMessage::TravelAuthorizationSnapshot { snapshot } if snapshot.generation == 3)
    );
    assert!(matches!(
        reader.read::<ControlMessage>().await?,
        ControlMessage::Heartbeat { nonce: 99 }
    ));
    assert!(
        matches!(reader.read::<ControlMessage>().await?, ControlMessage::TravelAuthorizationSnapshot { snapshot } if snapshot.generation == 4)
    );
    assert!(reader.read::<ControlMessage>().await.is_err());
    assert_eq!(generation, 4);
    Ok(())
}

#[tokio::test]
async fn invalid_enrollment_requests_return_errors_without_self_enqueue() -> Result<()> {
    let f = Fixture::new()?;
    let (tx, mut rx) = full_queue();
    let relay = relay();
    let reply = tokio::time::timeout(
        Duration::from_millis(100),
        handle_remote_enrollment_submit(
            &f.state,
            &relay,
            &tx,
            Uuid::nil(),
            "travel".into(),
            Uuid::nil(),
            Uuid::nil(),
            "home".into(),
            vec![],
            &f.config,
        ),
    )
    .await?;
    assert!(
        matches!(reply, Some(ControlMessage::RemoteEnrollmentResult { request_id, accepted: false, error: Some(_), .. }) if request_id.is_nil())
    );
    let reply = tokio::time::timeout(
        Duration::from_millis(100),
        handle_remote_enrollment_installed(
            &f.state,
            &relay,
            &tx,
            Uuid::nil(),
            "travel".into(),
            Uuid::nil(),
            Uuid::nil(),
            "home".into(),
            &f.config,
        ),
    )
    .await?;
    assert!(
        matches!(reply, Some(ControlMessage::RemoteEnrollmentInstalledAck { request_id, accepted: false, error: Some(_) }) if request_id.is_nil())
    );
    let reply = tokio::time::timeout(
        Duration::from_millis(100),
        handle_bootstrap_enrollment_submit(
            &f.state,
            &relay,
            &tx,
            CONTROL_PROTOCOL_VERSION,
            Uuid::nil(),
            "travel".into(),
            "home".into(),
            vec![],
            vec![],
            &f.config,
        ),
    )
    .await?;
    assert!(
        matches!(reply, Some(ControlMessage::BootstrapEnrollmentResult { request_id, accepted: false, error: Some(_), .. }) if request_id.is_nil())
    );
    assert!(f.state.pending_enrollments.lock().await.is_empty());
    for expected in 0..64 {
        assert!(matches!(rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected));
    }
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn responders_cannot_consume_another_home_or_lifecycle_request() -> Result<()> {
    let f = Fixture::new()?;
    for installed in [false, true] {
        let id = Uuid::new_v4();
        let mut rx = f.pending(id, installed, false).await?;
        if installed {
            assert!(
                complete_remote_enrollment_installed(&f.state, "wrong", id, true, None)
                    .await
                    .is_err()
            );
            assert!(
                complete_remote_enrollment(&f.state, "home", id, true, None, None)
                    .await
                    .is_err()
            );
        } else {
            assert!(
                complete_remote_enrollment(&f.state, "wrong", id, true, None, None)
                    .await
                    .is_err()
            );
            assert!(
                complete_remote_enrollment_installed(&f.state, "home", id, true, None)
                    .await
                    .is_err()
            );
        }
        assert!(f.state.pending_enrollments.lock().await.contains_key(&id));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        if installed {
            complete_remote_enrollment_installed(&f.state, "home", id, true, None).await?;
            assert!(
                matches!(rx.try_recv()?, ControlMessage::RemoteEnrollmentInstalledAck { request_id, accepted: true, .. } if request_id == id)
            );
            complete_remote_enrollment_installed(&f.state, "home", id, true, None).await?;
        } else {
            complete_remote_enrollment(&f.state, "home", id, true, Some(vec![1]), None).await?;
            assert!(
                matches!(rx.try_recv()?, ControlMessage::RemoteEnrollmentResult { request_id, accepted: true, response_json: Some(body), .. } if request_id == id && body == vec![1])
            );
            complete_remote_enrollment(&f.state, "home", id, true, None, None).await?;
        }
        assert!(rx.try_recv().is_err());
        assert!(!f.state.pending_enrollments.lock().await.contains_key(&id));
    }
    Ok(())
}

#[tokio::test]
async fn expired_and_missing_responses_are_ignored_without_session_error() -> Result<()> {
    let f = Fixture::new()?;
    for installed in [false, true] {
        let id = Uuid::new_v4();
        let mut rx = f.pending(id, installed, true).await?;
        for _ in 0..2 {
            if installed {
                complete_remote_enrollment_installed(&f.state, "home", id, true, None).await?;
            } else {
                complete_remote_enrollment(&f.state, "home", id, true, None, None).await?;
            }
        }
        assert!(rx.try_recv().is_err());
    }
    complete_remote_enrollment(&f.state, "home", Uuid::new_v4(), true, None, None).await?;
    complete_remote_enrollment_installed(&f.state, "home", Uuid::new_v4(), true, None).await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn control_queue_deadline_cancels_send_without_late_delivery() -> Result<()> {
    let (tx, mut rx) = full_queue();
    let start = tokio::time::Instant::now();
    assert!(
        send_control_until(
            &tx,
            ControlMessage::Heartbeat { nonce: 100 },
            (start + Duration::from_secs(2)).into_std()
        )
        .await
        .is_err()
    );
    assert!(tokio::time::Instant::now() >= start + Duration::from_secs(2));
    for expected in 0..64 {
        assert!(matches!(rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected));
    }
    tokio::task::yield_now().await;
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    send_control_until(
        &tx,
        ControlMessage::Heartbeat { nonce: 101 },
        (tokio::time::Instant::now() + Duration::from_secs(2)).into_std(),
    )
    .await?;
    assert!(matches!(
        rx.try_recv()?,
        ControlMessage::Heartbeat { nonce: 101 }
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn bootstrap_home_backpressure_expires_and_cleans_pending_request() -> Result<()> {
    let f = Fixture::new()?;
    let (home_tx, mut home_rx) = full_queue();
    let (relay_tx, mut relay_rx) = full_queue();
    f.state.homes.lock().await.sessions.insert(
        "home".into(),
        HomeSession {
            session_id: Uuid::new_v4(),
            tx: home_tx,
            shutdown: watch::channel(false).0,
        },
    );
    let id = Uuid::new_v4();
    let start = tokio::time::Instant::now();
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        handle_bootstrap_enrollment_submit(
            &f.state,
            &relay(),
            &relay_tx,
            CONTROL_PROTOCOL_VERSION,
            id,
            "travel".into(),
            "home".into(),
            vec![1; 32],
            vec![1],
            &f.config,
        ),
    )
    .await?;
    assert!(
        matches!(reply, Some(ControlMessage::BootstrapEnrollmentResult { request_id, accepted: false, error: Some(_), .. }) if request_id == id)
    );
    assert!(tokio::time::Instant::now() >= start + Duration::from_secs(15));
    assert!(!f.state.pending_enrollments.lock().await.contains_key(&id));
    for expected in 0..64 {
        assert!(
            matches!(home_rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected)
        );
        assert!(
            matches!(relay_rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected)
        );
    }
    tokio::task::yield_now().await;
    assert!(matches!(
        home_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        relay_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn response_backpressure_expires_without_failing_home_session() -> Result<()> {
    let f = Fixture::new()?;
    for installed in [false, true] {
        let id = Uuid::new_v4();
        let (tx, mut rx) = full_queue();
        let start = tokio::time::Instant::now();
        f.state.pending_enrollments.lock().await.insert(
            id,
            PendingEnrollment {
                forwarding_id: Uuid::new_v4(),
                relay_id: "relay".into(),
                travel_id: "travel".into(),
                home_id: "home".into(),
                relay: tx,
                bootstrap: false,
                installed,
                expires: (start + Duration::from_secs(2)).into_std(),
            },
        );
        let completion = async {
            if installed {
                complete_remote_enrollment_installed(&f.state, "home", id, true, None).await
            } else {
                complete_remote_enrollment(&f.state, "home", id, true, None, None).await
            }
        };
        tokio::time::timeout(Duration::from_secs(3), completion).await??;
        assert!(tokio::time::Instant::now() >= start + Duration::from_secs(2));
        assert!(!f.state.pending_enrollments.lock().await.contains_key(&id));
        for expected in 0..64 {
            assert!(
                matches!(rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected)
            );
        }
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
    }
    Ok(())
}

impl Fixture {
    async fn home_issuers(&self, ids: &[&str]) -> Result<()> {
        let authorities = ids
            .iter()
            .map(
                |id| flowsplice_core::authorization::TrustedTravelAuthority::Global {
                    id: format!("{id}-authority"),
                    epoch: 1,
                    home_id: (*id).into(),
                    public_key: self.state.control_signer.root_public_key.clone(),
                },
            )
            .collect();
        *self.state.authorization.write().await = ServerAuthorization::load(
            "test".into(),
            authorities,
            self.config.travel_authorization_state.clone(),
        )?;
        Ok(())
    }

    async fn home_sender(&self, id: &str, tx: mpsc::Sender<ControlMessage>) {
        self.state.homes.lock().await.sessions.insert(
            id.into(),
            HomeSession {
                session_id: Uuid::new_v4(),
                tx,
                shutdown: watch::channel(false).0,
            },
        );
    }

    fn home_request(&self) -> Result<HomeRequestEnvelope> {
        Ok(HomeRequestEnvelope::Legacy(
            flowsplice_enrollment::home::create_home_enrollment_request(
                "new-home",
                &self.directory.path().join("home-enrollment"),
                unix_time_secs()?,
            )?,
        ))
    }
}

#[tokio::test(start_paused = true)]
async fn home_enrollment_dispatch_reaches_healthy_issuer_despite_full_queue() -> Result<()> {
    let mut f = Fixture::new()?;
    f.config.handshake_timeout_secs = 1;
    f.home_issuers(&["blocked", "healthy"]).await?;
    let (blocked_tx, blocked_rx) = full_queue();
    let (healthy_tx, mut healthy_rx) = mpsc::channel(64);
    f.home_sender("blocked", blocked_tx).await;
    f.home_sender("healthy", healthy_tx).await;
    let request = f.home_request()?;
    let id = request.request_id;
    let request_json = serde_json::to_vec(&request)?;
    let dispatch = dispatch_home_enrollment(
        &f.state,
        &f.config,
        CONTROL_PROTOCOL_VERSION,
        id,
        "new-home".into(),
        vec![1; 32],
        request_json.clone(),
    );
    let responder = async {
        let message = tokio::time::timeout(Duration::from_millis(100), healthy_rx.recv())
            .await?
            .ok_or_else(|| anyhow!("healthy issuer queue closed"))?;
        assert!(
            matches!(message, ControlMessage::HomeEnrollmentSubmit { request_id, request_json: body, .. } if request_id == id && body == request_json)
        );
        complete_home_enrollment(
            &f.state,
            "healthy",
            id,
            false,
            None,
            Some("rejected by healthy issuer".into()),
        )
        .await?;
        assert_eq!(
            f.state
                .pending_home_enrollments
                .lock()
                .await
                .get(&id)
                .context("pending healthy response")?
                .remaining_responders,
            1
        );
        // Closing the other queue completes its failed dispatch without inventing
        // a Home responder; the actual healthy response has already been counted.
        drop(blocked_rx);
        Ok::<_, anyhow::Error>(())
    };
    let (result, response) = tokio::join!(dispatch, responder);
    response?;
    assert!(
        matches!(result?, ControlMessage::HomeBootstrapEnrollmentResult { request_id, accepted: false, error: Some(_), .. } if request_id == id)
    );
    assert!(f.state.pending_home_enrollments.lock().await.is_empty());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn home_enrollment_dispatch_uses_one_budget_for_all_issuers_and_response() -> Result<()> {
    let mut f = Fixture::new()?;
    f.config.handshake_timeout_secs = 1;
    f.home_issuers(&["blocked-a", "blocked-b", "silent"])
        .await?;
    let (first_tx, mut first_rx) = full_queue();
    let (second_tx, mut second_rx) = full_queue();
    let (silent_tx, mut silent_rx) = mpsc::channel(64);
    f.home_sender("blocked-a", first_tx).await;
    f.home_sender("blocked-b", second_tx).await;
    f.home_sender("silent", silent_tx).await;
    let request = f.home_request()?;
    let start = tokio::time::Instant::now();
    let result = dispatch_home_enrollment(
        &f.state,
        &f.config,
        CONTROL_PROTOCOL_VERSION,
        request.request_id,
        "new-home".into(),
        vec![1; 32],
        serde_json::to_vec(&request)?,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(2));
    assert!(f.state.pending_home_enrollments.lock().await.is_empty());
    assert!(
        matches!(silent_rx.try_recv()?, ControlMessage::HomeEnrollmentSubmit { request_id, .. } if request_id == request.request_id)
    );
    for expected in 0..64 {
        assert!(
            matches!(first_rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected)
        );
        assert!(
            matches!(second_rx.try_recv()?, ControlMessage::Heartbeat { nonce } if nonce == expected)
        );
    }
    tokio::task::yield_now().await;
    assert!(matches!(
        first_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        second_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn stale_home_dispatch_failure_does_not_consume_replacement() -> Result<()> {
    let f = Fixture::new()?;
    let request_id = Uuid::new_v4();
    let old_dispatch_id = Uuid::new_v4();
    let replacement_id = Uuid::new_v4();
    let (response, mut receiver) = oneshot::channel();
    f.state.pending_home_enrollments.lock().await.insert(
        request_id,
        PendingHomeEnrollment {
            dispatch_id: replacement_id,
            home_id: "new-home".into(),
            request_digest_sha256: "digest".into(),
            remaining_responders: 1,
            any_accepted: false,
            response: Some(response),
            last_error: None,
            expires: Instant::now() + Duration::from_secs(10),
        },
    );
    fail_home_enrollment_dispatch(
        &f.state,
        request_id,
        old_dispatch_id,
        &anyhow!("stale failure"),
    )
    .await;
    {
        let pending = f.state.pending_home_enrollments.lock().await;
        let record = pending.get(&request_id).context("replacement request")?;
        assert_eq!(record.dispatch_id, replacement_id);
        assert_eq!(record.remaining_responders, 1);
        assert!(record.last_error.is_none());
        assert!(record.response.is_some());
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    fail_home_enrollment_dispatch(
        &f.state,
        request_id,
        replacement_id,
        &anyhow!("current failure"),
    )
    .await;
    assert!(
        !f.state
            .pending_home_enrollments
            .lock()
            .await
            .contains_key(&request_id)
    );
    assert!(
        matches!(receiver.await?, ControlMessage::HomeBootstrapEnrollmentResult { request_id: id, accepted: false, error: Some(_), .. } if id == request_id)
    );
    Ok(())
}
