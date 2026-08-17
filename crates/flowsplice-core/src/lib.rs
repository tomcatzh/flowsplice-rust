#![forbid(unsafe_code)]

pub mod config;
pub mod frame;
pub mod protocol;
pub mod route;
pub mod tls;

pub const CONTROL_FRAME_LIMIT: usize = 1024 * 1024;
pub const DATA_FRAME_LIMIT: usize = 1024 * 1024;
pub const MAX_DATA_PAYLOAD: usize = 64 * 1024;

pub fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
