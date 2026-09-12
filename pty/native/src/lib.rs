//! Audited C/JNI boundary for the shared native terminal actor.
mod class_engine;
mod engine;
pub use class_engine::ClassNativeOptions;
pub use engine::{Action, NativeOptions, NativeSession};
#[cfg(target_os = "android")]
mod android;
mod ffi;

#[cfg(test)]
mod ffi_tests;
