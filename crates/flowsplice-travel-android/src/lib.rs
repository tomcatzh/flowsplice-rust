//! Audited JNI boundary for the Android Travel shell.
//!
//! The exported functions only translate owned strings and JSON. Runtime, credential, mapping,
//! and transport behavior remains in the safe `flowsplice-travel-core` crate.

use std::{
    path::Path,
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::{Context, Result, anyhow, bail};
use flowsplice_core::protocol::ServiceProtocol;
use flowsplice_travel_core::{Mapping, TravelCore};
use jni::{
    Env, EnvUnowned,
    objects::{JClass, JString},
};
use serde::Serialize;
use tokio::runtime::Runtime;

static RUNTIME: LazyLock<Result<Runtime, String>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("flowsplice-travel")
        .build()
        .map_err(|error| error.to_string())
});
static ENGINE: Mutex<Option<Arc<TravelCore>>> = Mutex::new(None);

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

fn status() -> Result<serde_json::Value> {
    let engine = with_engine()?;
    Ok(serde_json::to_value(runtime()?.block_on(engine.status()))?)
}

fn catalog() -> Result<serde_json::Value> {
    let engine = with_engine()?;
    Ok(serde_json::to_value(runtime()?.block_on(engine.catalog()))?)
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

// These six symbols are the entire unsafe ABI surface. `EnvUnowned::with_env` catches panics and
// prevents unwinding across the foreign boundary before any JNI operation is attempted.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_start<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    config_path: JString<'caller>,
    password: JString<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| {
        response_json(start_engine(
            &config_path.to_string(),
            &password.to_string(),
        ))
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
pub extern "system" fn Java_io_zxf_flowsplice_travel_NativeTravel_status<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    jni_string(&mut unowned_env, |_env| response_json(status()))
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
    jni_string(&mut unowned_env, |_env| {
        response_json(upsert_mapping(&mapping_json.to_string()))
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
    jni_string(&mut unowned_env, |_env| {
        response_json(delete_mapping(
            &home_id.to_string(),
            &service_id.to_string(),
            &protocol.to_string(),
        ))
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
