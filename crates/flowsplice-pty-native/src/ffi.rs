use crate::{Action, NativeOptions, NativeSession};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_char},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::runtime::Runtime;

static RUNTIME: LazyLock<Result<Runtime, String>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("flowsplice-pty")
        .build()
        .map_err(|error| error.to_string())
});
static SESSIONS: Mutex<BTreeMap<u64, Arc<NativeSession>>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(1);
fn runtime() -> Result<&'static Runtime> {
    RUNTIME.as_ref().map_err(|error| anyhow!(error.clone()))
}
fn session(id: u64) -> Result<Arc<NativeSession>> {
    SESSIONS
        .lock()
        .map_err(|_| anyhow!("native handle registry unavailable"))?
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("native terminal handle closed"))
}
pub(crate) fn open(value: &str) -> Result<Value> {
    if value.len() > 512 * 1024 {
        bail!("private terminal configuration too large");
    }
    let options: NativeOptions = serde_json::from_str(value)?;
    let _entered = runtime()?.enter();
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| anyhow!("native handle registry unavailable"))?;
    if sessions.len() >= 8 {
        bail!("native terminal handle limit reached");
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        bail!("native handle identifiers exhausted");
    }
    let handle = Arc::new(NativeSession::open(options)?);
    sessions.insert(id, handle);
    Ok(json!({"handle":id}))
}
pub(crate) fn send(id: u64, value: &str) -> Result<Value> {
    if value.len() > 128 * 1024 {
        bail!("native operation too large");
    }
    let action: Action = serde_json::from_str(value)?;
    session(id)?.send(action)?;
    Ok(json!({}))
}
pub(crate) fn poll(id: u64) -> Result<Value> {
    Ok(serde_json::to_value(session(id)?.poll())?)
}
pub(crate) fn close(id: u64) -> Result<Value> {
    let handle = SESSIONS
        .lock()
        .map_err(|_| anyhow!("native handle registry unavailable"))?
        .remove(&id);
    if let Some(handle) = handle {
        handle.close();
        runtime()?.spawn(async move {
            handle.shutdown().await;
        });
    }
    Ok(json!({}))
}
pub(crate) fn response(call: impl FnOnce() -> Result<Value> + std::panic::UnwindSafe) -> String {
    match std::panic::catch_unwind(call) {
        Ok(Ok(data)) => json!({"ok":true,"data":data}).to_string(),
        Ok(Err(error)) => {
            json!({"ok":false,"error":error.to_string().chars().take(512).collect::<String>()})
                .to_string()
        }
        Err(_) => json!({"ok":false,"error":"Native terminal operation failed"}).to_string(),
    }
}
fn owned(value: String) -> *mut c_char {
    CString::new(value).map_or(std::ptr::null_mut(), CString::into_raw)
}
unsafe fn text<'a>(value: *const c_char) -> Result<&'a str> {
    if value.is_null() {
        bail!("missing native JSON input");
    }
    // SAFETY: each exported caller contract requires an accessible, terminated string.
    Ok(unsafe { CStr::from_ptr(value) }.to_str()?)
}
/// Creates a handle from immutable, privately packaged native resources.
/// # Safety
/// `value` must be a valid NUL-terminated UTF-8 C string for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flowsplice_pty_open(value: *const c_char) -> *mut c_char {
    owned(response(|| open(unsafe { text(value) }?)))
}
/// Enqueues an action without waiting for network I/O.
/// # Safety
/// `value` must be a valid NUL-terminated UTF-8 C string for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flowsplice_pty_send(id: u64, value: *const c_char) -> *mut c_char {
    owned(response(|| send(id, unsafe { text(value) }?)))
}
/// Returns up to 64 queued events; it never waits for network I/O.
#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_pty_poll(id: u64) -> *mut c_char {
    owned(response(|| poll(id)))
}
/// Cancels a handle and asynchronously releases all network activity.
#[unsafe(no_mangle)]
pub extern "C" fn flowsplice_pty_close(id: u64) -> *mut c_char {
    owned(response(|| close(id)))
}
/// Frees a response returned by this library.
/// # Safety
/// `value` must be null or an unfreed pointer returned by a `flowsplice_pty_*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flowsplice_pty_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}
