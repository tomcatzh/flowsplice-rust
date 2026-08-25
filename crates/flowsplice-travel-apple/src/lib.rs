//! Audited C ABI boundary for the Apple Travel shell.
//!
//! The exported functions exchange owned UTF-8 strings and JSON. Runtime, enrollment, mapping,
//! and transport behavior stays in the safe `flowsplice-travel-core` crate.

use std::{
    ffi::{CStr, CString, c_char},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, LazyLock, Mutex},
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
            current.error = Some(format!("{error:#}"));
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

fn required_string(pointer: *const c_char, name: &str) -> Result<String> {
    if pointer.is_null() {
        bail!("{name} is null");
    }
    // SAFETY: The Swift bridge passes a valid NUL-terminated pointer for the duration of this call.
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

/// Releases a string returned by another exported function.
///
/// # Safety
///
/// `value` must either be null or a pointer returned exactly once by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flowsplice_travel_string_free(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: The caller promises this pointer came from `CString::into_raw` in this library.
        drop(unsafe { CString::from_raw(value) });
    }
}

#[cfg(test)]
mod tests {
    use super::{flowsplice_travel_stop, flowsplice_travel_string_free, stop_engine};
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
        // SAFETY: The pointer is live until it is released below.
        let response = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .map(str::to_owned);
        assert!(response.is_ok());
        assert!(response.is_ok_and(|json| json.contains(r#""ok":true"#)));
        // SAFETY: The pointer was returned by this library and has not been freed yet.
        unsafe { flowsplice_travel_string_free(pointer) };
    }
}
