//! Exercises exported C ownership and lifecycle without network or installed credentials.
use std::{
    ffi::{CStr, CString, c_char},
    path::Path,
    ptr, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ffi::{
    flowsplice_pty_close, flowsplice_pty_open, flowsplice_pty_poll, flowsplice_pty_send,
    flowsplice_pty_string_free,
};

struct OwnedResponse(*mut c_char);
impl Drop for OwnedResponse {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns one pointer returned by the C ABI.
        unsafe { flowsplice_pty_string_free(self.0) };
    }
}
fn response(pointer: *mut c_char) -> Result<Value> {
    ensure!(!pointer.is_null(), "C ABI returned a null response");
    let owned = OwnedResponse(pointer);
    // SAFETY: ABI responses are live NUL-terminated strings until OwnedResponse drops.
    let bytes = unsafe { CStr::from_ptr(owned.0) }.to_bytes();
    // Decoding creates owned JSON before the single free, including on parse failure.
    Ok(serde_json::from_slice(bytes)?)
}
fn rejected(value: &Value) {
    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    assert!(value.get("data").is_none());
}
fn send(handle: u64, action: &str) -> Result<Value> {
    let text = CString::new(action)?;
    // SAFETY: CString stays live for the complete call and contains a terminal NUL.
    response(unsafe { flowsplice_pty_send(handle, text.as_ptr()) })
}
struct Handle(u64);
impl Drop for Handle {
    fn drop(&mut self) {
        let pointer = flowsplice_pty_close(self.0);
        // SAFETY: close returns a new owned ABI string, freed once here.
        unsafe { flowsplice_pty_string_free(pointer) };
    }
}
fn open_uninstalled(path: &Path) -> Result<Handle> {
    assert!(!path.exists());
    // This is schema-valid only, intentionally unsigned and uninstalled. No enroll action
    // is sent, and connect must fail on the absent installation before networking.
    let signed = json!({"authority_id":"fixture", "payload_hex":"00", "signature_hex":"00"});
    let options = json!({
        "install_dir":path,
        "root_public_key":"fixture-not-a-real-root",
        "descriptor":{
            "version":1, "approving_home_id":"fixture-super-home", "endpoint":signed,
            "grant":signed, "service_id":"fixture-pty"
        },
        "travel_id":format!("pty-{}", Uuid::new_v4()), "label":"FFI test"
    });
    let text = CString::new(serde_json::to_vec(&options)?)?;
    // SAFETY: text is live, correctly NUL-terminated UTF-8 JSON for this call.
    let result = response(unsafe { flowsplice_pty_open(text.as_ptr()) })?;
    ensure!(result["ok"] == true, "schema-valid open failed: {result}");
    let id = result["data"]["handle"]
        .as_u64()
        .context("missing handle")?;
    ensure!(id != 0, "zero handle returned");
    Ok(Handle(id))
}
fn events_until(handle: u64, predicate: impl Fn(&[Value]) -> bool) -> Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    loop {
        let result = response(flowsplice_pty_poll(handle))?;
        ensure!(result["ok"] == true, "poll failed: {result}");
        let events = result["data"]
            .as_array()
            .context("poll did not return array")?;
        assert!(events.len() <= 64);
        collected.extend(events.iter().cloned());
        if predicate(&collected) {
            return Ok(collected);
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for local actor events"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn idle_uninstalled(event: &Value) -> bool {
    event["type"] == "state"
        && event["installed"] == false
        && event["connected"] == false
        && event["busy"] == false
}
fn exercise_handle(handle: u64) -> Result<()> {
    events_until(handle, |events| events.iter().any(idle_uninstalled))?;
    rejected(&send(handle, "{broken")?);
    rejected(&send(handle, r#"{"op":"unknown"}"#)?);
    // SAFETY: null is an explicitly rejected input; no memory is dereferenced by the ABI.
    rejected(&response(unsafe {
        flowsplice_pty_send(handle, ptr::null())
    })?);
    let result = send(handle, r#"{"op":"connect","password":"test-only-unused"}"#)?;
    assert_eq!(result["ok"], true);
    let events = events_until(handle, |events| {
        events.iter().any(|event| event["type"] == "error") && events.iter().any(idle_uninstalled)
    })?;
    assert!(events.iter().all(|event| event["connected"] != true));
    assert!(events.iter().any(|event| {
        event["type"] == "error"
            && event["message"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
    }));
    assert_eq!(response(flowsplice_pty_close(handle))?["ok"], true);
    assert_eq!(response(flowsplice_pty_close(handle))?["ok"], true);
    rejected(&response(flowsplice_pty_poll(handle))?);
    rejected(&send(handle, r#"{"op":"disconnect"}"#)?);
    Ok(())
}

#[test]
fn c_abi_owned_responses_and_uninstalled_lifecycle() -> Result<()> {
    // A single test owns this registry exercise: no parallel handle-limit races.
    // SAFETY: null input is explicitly supported as an error case.
    rejected(&response(unsafe { flowsplice_pty_open(ptr::null()) })?);
    for text in ["{broken", "null", "{}", r#"{"install_dir":42}"#] {
        let text = CString::new(text)?;
        // SAFETY: each CString is valid and live for the duration of open.
        rejected(&response(unsafe { flowsplice_pty_open(text.as_ptr()) })?);
    }
    let path = std::env::temp_dir().join(format!("flowsplice-pty-ffi-{}", Uuid::new_v4()));
    let first = open_uninstalled(&path)?;
    exercise_handle(first.0)?;
    let second_path = std::env::temp_dir().join(format!("flowsplice-pty-ffi-{}", Uuid::new_v4()));
    let second = open_uninstalled(&second_path)?;
    assert!(
        second.0 > first.0,
        "closed handle identifiers must not be reused"
    );
    events_until(second.0, |events| events.iter().any(idle_uninstalled))?;
    assert!(
        !path.exists(),
        "uninstalled connect must not create an identity"
    );
    assert!(!second_path.exists());
    Ok(())
}

#[test]
fn class_open_emits_global_identity_and_rejects_mixed_options() -> Result<()> {
    let path = std::env::temp_dir().join(format!("pty-class-ffi-{}", Uuid::new_v4()));
    let options = json!({"install_dir":path,"root_public_key":"not-a-real-root","travel_id":format!("pty-{}",Uuid::new_v4()),"label":"Device · PTY","service_class":{"version":1,"approving_home_id":"fixture-super-home","application_protocol":"flowsplice.pty.v1","protocol":"tcp"}});
    let text = CString::new(serde_json::to_vec(&options)?)?;
    // SAFETY: CString retains the JSON for the duration of the ABI call.
    let opened = response(unsafe { flowsplice_pty_open(text.as_ptr()) })?;
    assert_eq!(opened["ok"], true);
    let handle = Handle(opened["data"]["handle"].as_u64().context("class handle")?);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let polled = response(flowsplice_pty_poll(handle.0))?;
        let events = polled["data"].as_array().context("class event array")?;
        if let Some(identity) = events.iter().find(|event| event["type"] == "identity") {
            assert_eq!(identity["installed"], false);
            assert_eq!(identity["connected"], false);
            assert_eq!(identity["label"], "Device · PTY");
            assert!(identity.get("home_id").is_none());
            break;
        }
        ensure!(Instant::now() < deadline, "class identity missing");
        thread::sleep(Duration::from_millis(5));
    }
    let mut mixed = options;
    mixed["descriptor"] = json!({});
    let text = CString::new(serde_json::to_vec(&mixed)?)?;
    // SAFETY: CString remains live throughout the call.
    rejected(&response(unsafe { flowsplice_pty_open(text.as_ptr()) })?);
    assert!(
        !path.exists(),
        "opening an uninstalled identity must not write credentials"
    );
    Ok(())
}
