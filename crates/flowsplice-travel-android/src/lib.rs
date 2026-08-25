//! Audited JNI boundary for the Android Travel shell.
//!
//! The exported functions only translate owned strings and JSON. Runtime, credential, mapping,
//! and transport behavior remains in the safe `flowsplice-travel-core` crate.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use flowsplice_core::protocol::ServiceProtocol;
use flowsplice_travel_core::{
    Mapping, RemoteEnrollmentOptions, RemoteEnrollmentProgress, TravelCore, enroll_remote,
};
use jni::{
    Env, EnvUnowned,
    objects::{JClass, JString},
};
use serde::Serialize;
use tokio::{runtime::Runtime, task::JoinHandle};

static RUNTIME: LazyLock<Result<Runtime, String>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("flowsplice-travel")
        .build()
        .map_err(|error| error.to_string())
});
static ENGINE: Mutex<Option<Arc<TravelCore>>> = Mutex::new(None);
static ENROLLMENT: Mutex<Option<EnrollmentSession>> = Mutex::new(None);

struct EnrollmentSession {
    status: Arc<Mutex<NativeEnrollmentStatus>>,
    task: JoinHandle<()>,
}

#[derive(Clone, Serialize)]
struct NativeEnrollmentStatus {
    phase: String,
    travel_id: String,
    request_id: Option<String>,
    verification_code: Option<String>,
    config_path: Option<PathBuf>,
    credential_id: Option<String>,
    error: Option<String>,
}

impl From<RemoteEnrollmentProgress> for NativeEnrollmentStatus {
    fn from(progress: RemoteEnrollmentProgress) -> Self {
        Self {
            phase: serde_json::to_value(progress.phase)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned()),
            travel_id: progress.travel_id,
            request_id: progress.request_id.map(|id| id.to_string()),
            verification_code: progress.verification_code,
            config_path: progress.config_path,
            credential_id: progress.credential_id.map(|id| id.to_string()),
            error: None,
        }
    }
}

#[derive(Serialize)]
struct NativeResponse<T> {
    ok: bool,
    data: Option<T>,
    error: Option<String>,
}

fn runtime() -> Result<&'static Runtime> {
    RUNTIME.as_ref().map_err(|error| anyhow!(error.clone()))
}

fn with_engine() -> Result<Arc<TravelCore>> {
    ENGINE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?
        .clone()
        .ok_or_else(|| anyhow!("Travel is not running"))
}

fn start_engine(config_path: &str, password: &str) -> Result<serde_json::Value> {
    let mut slot = ENGINE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?;
    if let Some(engine) = slot.as_ref() {
        return Ok(serde_json::to_value(runtime()?.block_on(engine.status()))?);
    }
    let engine = Arc::new(
        runtime()?
            .block_on(TravelCore::start(Path::new(config_path), password))
            .context("failed to start Travel Core")?,
    );
    let status = runtime()?.block_on(engine.status());
    *slot = Some(engine);
    Ok(serde_json::to_value(status)?)
}

fn stop_engine() -> Result<serde_json::Value> {
    let mut slot = ENGINE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?;
    let Some(engine) = slot.take() else {
        return Ok(serde_json::json!({ "running": false }));
    };
    // Keep the lifecycle lock until every listener owned by the old engine has closed. A Start
    // arriving immediately after Stop must not race those listeners for the same local ports.
    runtime()?.block_on(engine.shutdown());
    Ok(serde_json::json!({ "running": false }))
}

fn notify_network_changed() -> Result<serde_json::Value> {
    let engine = with_engine()?;
    engine.notify_network_changed();
    Ok(serde_json::json!({ "reconnect_requested": true }))
}

fn status() -> Result<serde_json::Value> {
    let engine = with_engine()?;
    Ok(serde_json::to_value(runtime()?.block_on(engine.status()))?)
}

fn wait_for_status_change(known_generation: i64, timeout_millis: i64) -> Result<serde_json::Value> {
    let engine = with_engine()?;
    let known_generation = u64::try_from(known_generation).unwrap_or_default();
    let timeout_millis = u64::try_from(timeout_millis)
        .unwrap_or(30_000)
        .clamp(250, 30_000);
    Ok(serde_json::to_value(runtime()?.block_on(
        engine.wait_for_status_change(known_generation, Duration::from_millis(timeout_millis)),
    ))?)
}

fn wake_status_waiters() -> Result<serde_json::Value> {
    let engine = with_engine()?;
    engine.wake_status_observers();
    Ok(serde_json::json!({ "woken": true }))
}

fn catalog() -> Result<serde_json::Value> {
    let engine = with_engine()?;
    Ok(serde_json::to_value(runtime()?.block_on(engine.catalog()))?)
}

fn enrollment_status() -> Result<serde_json::Value> {
    let slot = ENROLLMENT
        .lock()
        .map_err(|_| anyhow!("Travel enrollment lock is poisoned"))?;
    let Some(session) = slot.as_ref() else {
        return Ok(serde_json::json!({
            "phase": "idle",
            "travel_id": "",
            "request_id": null,
            "verification_code": null,
            "config_path": null,
            "credential_id": null,
            "error": null
        }));
    };
    let status = session
        .status
        .lock()
        .map_err(|_| anyhow!("Travel enrollment status lock is poisoned"))?
        .clone();
    Ok(serde_json::to_value(status)?)
}

fn begin_enrollment(
    install_dir: &str,
    travel_id: &str,
    home_id: &str,
    selected_relay: &str,
    password: &str,
) -> Result<serde_json::Value> {
    let mut slot = ENROLLMENT
        .lock()
        .map_err(|_| anyhow!("Travel enrollment lock is poisoned"))?;
    if let Some(session) = slot.as_ref()
        && !session.task.is_finished()
    {
        let current = session
            .status
            .lock()
            .map_err(|_| anyhow!("Travel enrollment status lock is poisoned"))?
            .clone();
        return Ok(serde_json::to_value(current)?);
    }

    let initial = NativeEnrollmentStatus {
        phase: "preparing".to_owned(),
        travel_id: travel_id.to_owned(),
        request_id: None,
        verification_code: None,
        config_path: None,
        credential_id: None,
        error: None,
    };
    let status = Arc::new(Mutex::new(initial.clone()));
    let task_status = Arc::clone(&status);
    let options = RemoteEnrollmentOptions {
        travel_id: travel_id.to_owned(),
        home_id: home_id.to_owned(),
        install_dir: PathBuf::from(install_dir),
        bootstrap_config: None,
        selected_relay: (!selected_relay.is_empty()).then(|| selected_relay.to_owned()),
        ui_listen: None,
        private_key_password: password.to_owned(),
        wait_timeout_secs: 900,
    };
    let task = runtime()?.spawn(async move {
        let progress_status = Arc::clone(&task_status);
        let result = enroll_remote(options, move |progress| {
            if let Ok(mut current) = progress_status.lock() {
                *current = progress.into();
            }
        })
        .await;
        if let Err(error) = result
            && let Ok(mut current) = task_status.lock()
        {
            "error".clone_into(&mut current.phase);
            current.error = Some(format!("{error:#}"));
        }
    });
    *slot = Some(EnrollmentSession { status, task });
    Ok(serde_json::to_value(initial)?)
}

fn cancel_enrollment() -> Result<serde_json::Value> {
    let mut slot = ENROLLMENT
        .lock()
        .map_err(|_| anyhow!("Travel enrollment lock is poisoned"))?;
    let Some(session) = slot.take() else {
        return Ok(serde_json::json!({
            "phase": "idle",
            "travel_id": "",
            "request_id": null,
            "verification_code": null,
            "config_path": null,
            "credential_id": null,
            "error": null
        }));
    };
    session.task.abort();
    drop(slot);
    let _ = runtime()?.block_on(session.task);
    let mut status = session
        .status
        .lock()
        .map_err(|_| anyhow!("Travel enrollment status lock is poisoned"))?;
    "cancelled".clone_into(&mut status.phase);
    status.error = None;
    Ok(serde_json::to_value(status.clone())?)
}

fn upsert_mapping(mapping_json: &str) -> Result<serde_json::Value> {
    let engine = with_engine()?;
    let mapping: Mapping = serde_json::from_str(mapping_json).context("invalid mapping request")?;
    Ok(serde_json::to_value(
        runtime()?.block_on(engine.upsert_mapping(mapping))?,
    )?)
}

fn delete_mapping(home_id: &str, service_id: &str, protocol: &str) -> Result<serde_json::Value> {
    let protocol = match protocol {
        "tcp" => ServiceProtocol::Tcp,
        "udp" => ServiceProtocol::Udp,
        _ => bail!("protocol must be tcp or udp"),
    };
    let engine = with_engine()?;
    Ok(serde_json::to_value(runtime()?.block_on(
        engine.delete_mapping(home_id.to_owned(), service_id.to_owned(), protocol),
    )?)?)
}

fn response_json(result: Result<serde_json::Value>) -> String {
    let response = match result {
        Ok(data) => NativeResponse {
            ok: true,
            data: Some(data),
            error: None,
        },
        Err(error) => NativeResponse {
            ok: false,
            data: None,
            error: Some(format!("{error:#}")),
        },
    };
    serde_json::to_string(&response).unwrap_or_else(|error| {
        format!(r#"{{"ok":false,"data":null,"error":"JSON encoding failed: {error}"}}"#)
    })
}

fn required_string(env: &mut Env<'_>, value: &JString<'_>, name: &str) -> Result<String> {
    value
        .try_to_string(env)
        .map_err(|error| anyhow!("could not read JNI {name}: {error}"))
}

fn jni_string<'caller>(
    unowned_env: &mut EnvUnowned<'caller>,
    action: impl FnOnce(&mut Env<'caller>) -> String,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> std::result::Result<_, jni::errors::Error> {
            let result = action(env);
            JString::from_str(env, result)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// These symbols are the entire unsafe ABI surface. `EnvUnowned::with_env` catches panics and
// prevents unwinding across the foreign boundary before any JNI operation is attempted.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_beginEnrollment<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    install_dir: JString<'caller>,
    travel_id: JString<'caller>,
    home_id: JString<'caller>,
    selected_relay: JString<'caller>,
    password: JString<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |env| {
        response_json((|| {
            let install_dir = required_string(env, &install_dir, "install directory")?;
            let travel_id = required_string(env, &travel_id, "Travel id")?;
            let home_id = required_string(env, &home_id, "Home id")?;
            let selected_relay = required_string(env, &selected_relay, "Relay")?;
            let password = required_string(env, &password, "private-key password")?;
            begin_enrollment(
                &install_dir,
                &travel_id,
                &home_id,
                &selected_relay,
                &password,
            )
        })())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_enrollmentStatus<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| response_json(enrollment_status()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_cancelEnrollment<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| response_json(cancel_enrollment()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_start<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    config_path: JString<'caller>,
    password: JString<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |env| {
        response_json((|| {
            let config_path = required_string(env, &config_path, "config path")?;
            let password = required_string(env, &password, "private-key password")?;
            start_engine(&config_path, &password)
        })())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_stop<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| response_json(stop_engine()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_networkChanged<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| {
        response_json(notify_network_changed())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_status<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| response_json(status()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_waitForStatusChange<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    known_generation: i64,
    timeout_millis: i64,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| {
        response_json(wait_for_status_change(known_generation, timeout_millis))
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_wakeStatusWaiters<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(
        &mut unowned_env,
        |_env| response_json(wake_status_waiters()),
    )
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_catalog<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| response_json(catalog()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_upsertMapping<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    mapping_json: JString<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |env| {
        response_json((|| {
            let mapping_json = required_string(env, &mapping_json, "mapping JSON")?;
            upsert_mapping(&mapping_json)
        })())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_deleteMapping<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    home_id: JString<'caller>,
    service_id: JString<'caller>,
    protocol: JString<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |env| {
        response_json((|| {
            let home_id = required_string(env, &home_id, "Home id")?;
            let service_id = required_string(env, &service_id, "service id")?;
            let protocol = required_string(env, &protocol, "protocol")?;
            delete_mapping(&home_id, &service_id, &protocol)
        })())
    })
}

#[cfg(test)]
mod tests {
    use super::stop_engine;

    #[test]
    fn stopping_an_idle_engine_is_idempotent() -> anyhow::Result<()> {
        assert_eq!(stop_engine()?, serde_json::json!({ "running": false }),);
        Ok(())
    }
}
