use std::{
    ffi::{CStr, CString, c_char},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use flowsplice_core::protocol::ServiceProtocol;
use flowsplice_travel_core::{
    Mapping, RemoteEnrollmentOptions, RemoteEnrollmentProgress, TravelCore, enroll_remote,
};
use serde::Serialize;
use tokio::{runtime::Runtime, task::JoinHandle};

static RUNTIME: LazyLock<Result<Runtime, String>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("flowsplice-travel-apple")
        .build()
        .map_err(|error| error.to_string())
});
static ENGINE: Mutex<Option<Arc<TravelCore>>> = Mutex::new(None);
static ENGINE_LIFECYCLE: Mutex<()> = Mutex::new(());
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
    error_code: Option<&'static str>,
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
    let _lifecycle = ENGINE_LIFECYCLE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?;
    if let Some(engine) = ENGINE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?
        .clone()
    {
        return Ok(serde_json::to_value(runtime()?.block_on(engine.status()))?);
    }
    let engine = Arc::new(
        runtime()?
            .block_on(TravelCore::start(Path::new(config_path), password))
            .context("failed to start Travel Core")?,
    );
    let status = runtime()?.block_on(engine.status());
    *ENGINE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))? = Some(engine);
    Ok(serde_json::to_value(status)?)
}

fn stop_engine() -> Result<serde_json::Value> {
    let _lifecycle = ENGINE_LIFECYCLE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?;
    let Some(engine) = ENGINE
        .lock()
        .map_err(|_| anyhow!("Travel runtime lock is poisoned"))?
        .take()
    else {
        return Ok(serde_json::json!({ "running": false }));
    };
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

fn wait_for_status_change(known_generation: u64, timeout_millis: u64) -> Result<serde_json::Value> {
    let engine = with_engine()?;
    let timeout_millis = timeout_millis.clamp(250, 30_000);
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

fn idle_enrollment() -> serde_json::Value {
    serde_json::json!({
        "phase": "idle",
        "travel_id": "",
        "request_id": null,
        "verification_code": null,
        "config_path": null,
        "credential_id": null,
        "error": null
    })
}

fn enrollment_status() -> Result<serde_json::Value> {
    let slot = ENROLLMENT
        .lock()
        .map_err(|_| anyhow!("Travel enrollment lock is poisoned"))?;
    let Some(session) = slot.as_ref() else {
        return Ok(idle_enrollment());
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
    let enrollment_status = Arc::new(Mutex::new(initial.clone()));
    let task_status = Arc::clone(&enrollment_status);
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
            current.error = Some(public_error(&error).1);
        }
    });
    *slot = Some(EnrollmentSession {
        status: enrollment_status,
        task,
    });
    Ok(serde_json::to_value(initial)?)
}

fn cancel_enrollment() -> Result<serde_json::Value> {
    let mut slot = ENROLLMENT
        .lock()
        .map_err(|_| anyhow!("Travel enrollment lock is poisoned"))?;
    let Some(session) = slot.take() else {
        return Ok(idle_enrollment());
    };
    session.task.abort();
    drop(slot);
    let _ = runtime()?.block_on(session.task);
    let mut current = session
        .status
        .lock()
        .map_err(|_| anyhow!("Travel enrollment status lock is poisoned"))?;
    "cancelled".clone_into(&mut current.phase);
    current.error = None;
    Ok(serde_json::to_value(current.clone())?)
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
            error_code: None,
            error: None,
        },
        Err(error) => {
            let (error_code, message) = public_error(&error);
            NativeResponse {
                ok: false,
                data: None,
                error_code: Some(error_code),
                error: Some(message),
            }
        }
    };
    serde_json::to_string(&response).unwrap_or_else(|error| {
        format!(r#"{{"ok":false,"data":null,"error":"JSON encoding failed: {error}"}}"#)
    })
}

fn public_error(error: &anyhow::Error) -> (&'static str, String) {
    let detail = format!("{error:#}");
    let lower = detail.to_ascii_lowercase();
    if lower.contains("travel is not running") {
        return ("not_running", "Travel is not running.".to_owned());
    }
    if lower.contains("tls") || lower.contains("certificate") || lower.contains("secure") {
        return (
            "secure_connection_failed",
            "Could not establish a secure connection. Check the Relay address and FlowSplice versions."
                .to_owned(),
        );
    }
    if lower.contains("failed to bind") || lower.contains("address already in use") {
        return (
            "local_port_unavailable",
            "The local mapping port is unavailable. Choose another port or stop the conflicting listener."
                .to_owned(),
        );
    }
    if lower.contains("invalid") || lower.contains("must be") || lower.contains("is null") {
        return (
            "invalid_request",
            "The Travel request is invalid.".to_owned(),
        );
    }
    (
        "travel_core_failed",
        "Travel Core could not complete the operation. Check Diagnostics and try again.".to_owned(),
    )
}

fn required_string(pointer: *const c_char, name: &str) -> Result<String> {
    if pointer.is_null() {
        bail!("{name} is null");
    }
    let value = unsafe { CStr::from_ptr(pointer) };
    Ok(value
        .to_str()
        .with_context(|| format!("{name} is not valid UTF-8"))?
        .to_owned())
}

fn owned_response(result: Result<serde_json::Value>) -> *mut c_char {
    match CString::new(response_json(result)) {
        Ok(value) => value.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_begin_enrollment(
    install_dir: *const c_char,
    travel_id: *const c_char,
    home_id: *const c_char,
    selected_relay: *const c_char,
    password: *const c_char,
) -> *mut c_char {
    owned_response((|| {
        begin_enrollment(
            &required_string(install_dir, "install directory")?,
            &required_string(travel_id, "Travel id")?,
            &required_string(home_id, "Home id")?,
            &required_string(selected_relay, "Relay")?,
            &required_string(password, "private-key password")?,
        )
    })())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_enrollment_status() -> *mut c_char {
    owned_response(enrollment_status())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_cancel_enrollment() -> *mut c_char {
    owned_response(cancel_enrollment())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_start(
    config_path: *const c_char,
    password: *const c_char,
) -> *mut c_char {
    owned_response((|| {
        start_engine(
            &required_string(config_path, "config path")?,
            &required_string(password, "private-key password")?,
        )
    })())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_stop() -> *mut c_char {
    owned_response(stop_engine())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_network_changed() -> *mut c_char {
    owned_response(notify_network_changed())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_status() -> *mut c_char {
    owned_response(status())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_wait_for_status_change(
    known_generation: u64,
    timeout_millis: u64,
) -> *mut c_char {
    owned_response(wait_for_status_change(known_generation, timeout_millis))
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_wake_status_waiters() -> *mut c_char {
    owned_response(wake_status_waiters())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_catalog() -> *mut c_char {
    owned_response(catalog())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_upsert_mapping(mapping_json: *const c_char) -> *mut c_char {
    owned_response((|| {
        upsert_mapping(&required_string(mapping_json, "mapping JSON")?)
    })())
}

#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_travel_delete_mapping(
    home_id: *const c_char,
    service_id: *const c_char,
    protocol: *const c_char,
) -> *mut c_char {
    owned_response((|| {
        delete_mapping(
            &required_string(home_id, "Home id")?,
            &required_string(service_id, "service id")?,
            &required_string(protocol, "protocol")?,
        )
    })())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn flowsplice_travel_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        flowsplice_travel_stop, flowsplice_travel_string_free, public_error, response_json,
        stop_engine,
    };
    use anyhow::anyhow;
    use std::ffi::CStr;

    #[test]
    fn stopping_an_idle_engine_is_idempotent() -> anyhow::Result<()> {
        assert_eq!(stop_engine()?, serde_json::json!({ "running": false }));
        Ok(())
    }

    #[test]
    fn ffi_response_is_owned_utf8_json() {
        let pointer = flowsplice_travel_stop();
        assert!(!pointer.is_null());
        let response = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .map(str::to_owned);
        assert!(response.is_ok());
        assert!(response.is_ok_and(|json| json.contains(r#""ok":true"#)));
        unsafe { flowsplice_travel_string_free(pointer) };
    }

    #[test]
    fn ffi_errors_do_not_expose_local_paths() {
        let response = response_json(Err(anyhow!(
            "failed to read /private/var/mobile/Containers/Data/Application/SECRET/config.toml"
        )));
        assert!(!response.contains("/private/var"));
        assert!(response.contains(r#""error_code":"travel_core_failed""#));
        assert_eq!(
            public_error(&anyhow!("Travel is not running")).0,
            "not_running"
        );
    }
}
