use crate::ffi;
use jni::{
    Env, EnvUnowned,
    objects::{JClass, JString},
};
fn result<'caller>(
    env: &mut EnvUnowned<'caller>,
    action: impl FnOnce(&mut Env<'caller>) -> String,
) -> JString<'caller> {
    env.with_env(|env| -> std::result::Result<_, jni::errors::Error> {
        {
            let value = action(env);
            JString::from_str(env, value)
        }
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_pty_NativePty_open<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    value: JString<'caller>,
) -> JString<'caller> {
    result(&mut env, |env| {
        let value = value.try_to_string(env);
        ffi::response(|| ffi::open(&value?))
    })
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_pty_NativePty_send<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    handle: i64,
    value: JString<'caller>,
) -> JString<'caller> {
    result(&mut env, |env| {
        let value = value.try_to_string(env);
        ffi::response(|| ffi::send(u64::try_from(handle)?, &value?))
    })
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_pty_NativePty_poll<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    handle: i64,
) -> JString<'caller> {
    result(&mut env, |_env| {
        ffi::response(|| ffi::poll(u64::try_from(handle)?))
    })
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_zxf_flowsplice_pty_NativePty_close<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    handle: i64,
) -> JString<'caller> {
    result(&mut env, |_env| {
        ffi::response(|| ffi::close(u64::try_from(handle)?))
    })
}
