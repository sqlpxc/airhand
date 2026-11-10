//! Airhand 的 Android 核心：Kotlin 经 JNI（`app.airhand.android.core.Native`）调用。
//!
//! * [`ctl`]：控制端（看 / 操作别的设备），包装 `airhand_link` 的控制端服务；
//! * [`host`]：被控端（手机被别人看 / 操作）：网络部分在这里，采集、编码、注入输入在 Kotlin 那边；
//! * 句柄：`start` 返回一个 `long`，之后的调用都带着它；`stop` 之后作废。
//!
//! 数据交换尽量简单：命令和事件是 JSON 字符串，画面 / 声音是字节数组（Rust → Kotlin）或直接缓冲区（Kotlin → Rust）。
//! Rust 不回调 Java：Kotlin 的线程用 `poll*` 阻塞地取（见 [`queue`]）。

pub mod ctl;
pub mod host;
pub mod queue;
pub mod rt;
mod session_queue;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use airhand_proto::session::Codec;
use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jbyteArray, jint, jlong, jstring};

use ctl::Ctl;
use host::Host;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

// ───────────────────────────── 句柄 ─────────────────────────────

static NEXT: AtomicI64 = AtomicI64::new(1);
static CTLS: LazyLock<Mutex<HashMap<i64, Arc<Ctl>>>> = LazyLock::new(Mutex::default);
static HOSTS: LazyLock<Mutex<HashMap<i64, Arc<Host>>>> = LazyLock::new(Mutex::default);

fn ctl(h: jlong) -> Option<Arc<Ctl>> {
    CTLS.lock().unwrap().get(&h).cloned()
}

fn host(h: jlong) -> Option<Arc<Host>> {
    HOSTS.lock().unwrap().get(&h).cloned()
}

fn get_string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    env.get_string(s).ok().map(Into::into)
}

fn new_string(env: &mut JNIEnv, s: Option<String>) -> jstring {
    match s {
        Some(s) => env
            .new_string(s)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

fn throw(env: &mut JNIEnv, msg: impl AsRef<str>) {
    let _ = env.throw_new("java/lang/IllegalArgumentException", msg.as_ref());
}

fn timeout(ms: jint) -> Duration {
    Duration::from_millis(ms.max(0) as u64)
}

/// JNI 入口不能让 panic 穿出去：记日志，返回默认值
fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(default)
}

// ───────────────────────────── 通用 ─────────────────────────────

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_init(
    mut env: JNIEnv,
    _: JClass,
    filter: JString,
) {
    let filter = get_string(&mut env, &filter).unwrap_or_else(|| "info".into());
    rt::init_logging(&filter);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol = airhand_proto::SIGNAL_VERSION,
        "airhand native library loaded"
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_protocolVersion(
    _: JNIEnv,
    _: JClass,
) -> jint {
    airhand_proto::SIGNAL_VERSION as jint
}

/// 校验服务器证书指纹：合法返回 null，否则返回原因（英文）
#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_checkFingerprint(
    mut env: JNIEnv,
    _: JClass,
    fp: JString,
) -> jstring {
    let fp = get_string(&mut env, &fp).unwrap_or_default();
    let r = airhand_link::net::parse_fingerprint(&fp)
        .err()
        .map(|e| e.to_string());
    new_string(&mut env, r)
}

// ───────────────────────────── 控制端 ─────────────────────────────

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_ctlStart(
    mut env: JNIEnv,
    _: JClass,
    cfg: JString,
) -> jlong {
    let Some(cfg) = get_string(&mut env, &cfg) else {
        throw(&mut env, "null config");
        return 0;
    };
    let cfg: ctl::LinkSettings = match serde_json::from_str(&cfg) {
        Ok(c) => c,
        Err(e) => {
            throw(&mut env, format!("bad config: {e}"));
            return 0;
        }
    };
    match guard(Err(anyhow::anyhow!("panic")), || Ctl::start(&cfg)) {
        Ok(c) => {
            let h = NEXT.fetch_add(1, Ordering::Relaxed);
            CTLS.lock().unwrap().insert(h, c);
            h
        }
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_ctlStop(
    _: JNIEnv,
    _: JClass,
    h: jlong,
) {
    let c = CTLS.lock().unwrap().remove(&h);
    // 在运行时外面 drop：ControllerService 的 Drop 只是中止任务
    drop(c);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_ctlPollEvent(
    mut env: JNIEnv,
    _: JClass,
    h: jlong,
    timeout_ms: jint,
) -> jstring {
    let Some(c) = ctl(h) else {
        return std::ptr::null_mut();
    };
    let e = guard(None, || c.poll_event(timeout(timeout_ms)));
    new_string(&mut env, e)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_ctlCall(
    mut env: JNIEnv,
    _: JClass,
    h: jlong,
    json: JString,
) -> jstring {
    let json = get_string(&mut env, &json).unwrap_or_default();
    let r = match ctl(h) {
        Some(c) => guard(
            r#"{"err":{"kind":"failed","detail":"panic"}}"#.to_string(),
            || c.call(&json),
        ),
        None => r#"{"err":{"kind":"notConnected"}}"#.to_string(),
    };
    new_string(&mut env, Some(r))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_ctlPollSession(
    env: JNIEnv,
    _: JClass,
    h: jlong,
    sid: jint,
    timeout_ms: jint,
) -> jbyteArray {
    let Some(c) = ctl(h) else {
        return std::ptr::null_mut();
    };
    match guard(None, || c.poll_session(sid as u32, timeout(timeout_ms))) {
        Some(b) => env
            .byte_array_from_slice(&b)
            .map(|a| a.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_ctlAck(
    _: JNIEnv,
    _: JClass,
    h: jlong,
    sid: jint,
    seq: jint,
) {
    if let Some(c) = ctl(h) {
        c.ack(sid as u32, seq as u32);
    }
}

// ───────────────────────────── 被控端 ─────────────────────────────

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostStart(
    mut env: JNIEnv,
    _: JClass,
    cfg: JString,
) -> jlong {
    let Some(cfg) = get_string(&mut env, &cfg) else {
        throw(&mut env, "null config");
        return 0;
    };
    let cfg: host::HostConfig = match serde_json::from_str(&cfg) {
        Ok(c) => c,
        Err(e) => {
            throw(&mut env, format!("bad config: {e}"));
            return 0;
        }
    };
    match guard(Err(anyhow::anyhow!("panic")), || Host::start(cfg)) {
        Ok(host) => {
            let h = NEXT.fetch_add(1, Ordering::Relaxed);
            HOSTS.lock().unwrap().insert(h, host);
            h
        }
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostStop(
    _: JNIEnv,
    _: JClass,
    h: jlong,
) {
    let host = HOSTS.lock().unwrap().remove(&h);
    drop(host);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostPollEvent(
    mut env: JNIEnv,
    _: JClass,
    h: jlong,
    timeout_ms: jint,
) -> jstring {
    let Some(host) = host(h) else {
        return std::ptr::null_mut();
    };
    let e = guard(None, || host.poll_event(timeout(timeout_ms)));
    new_string(&mut env, e)
}

/// 执行 [`host::Op`]。参数不对时抛 IllegalArgumentException
#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostCall(
    mut env: JNIEnv,
    _: JClass,
    h: jlong,
    json: JString,
) {
    let json = get_string(&mut env, &json).unwrap_or_default();
    if let Some(host) = host(h)
        && let Err(e) = guard(Ok(()), || host.call(&json))
    {
        throw(&mut env, format!("{e:#}"));
    }
}

/// 渲染线程每一轮调用：有变化时返回 [`host::video::Tick`] 的 JSON，否则 null
#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostTick(
    mut env: JNIEnv,
    _: JClass,
    h: jlong,
) -> jstring {
    let Some(host) = host(h) else {
        return std::ptr::null_mut();
    };
    let t = guard(None, || host.video_hub().tick());
    new_string(&mut env, t.and_then(|t| serde_json::to_string(&t).ok()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostWaitWindow(
    _: JNIEnv,
    _: JClass,
    h: jlong,
    timeout_ms: jint,
) -> jboolean {
    let Some(host) = host(h) else {
        return JNI_TRUE;
    };
    if guard(true, || host.video_hub().wait_window(timeout(timeout_ms))) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

fn direct_slice<'a>(env: &JNIEnv, buf: &JByteBuffer, offset: jint, len: jint) -> Option<&'a [u8]> {
    let ptr = env.get_direct_buffer_address(buf).ok()?;
    let cap = env.get_direct_buffer_capacity(buf).ok()?;
    let (offset, len) = (usize::try_from(offset).ok()?, usize::try_from(len).ok()?);
    if offset.checked_add(len)? > cap {
        return None;
    }
    // SAFETY: 直接缓冲区在这次 JNI 调用期间有效，范围已经检查过
    Some(unsafe { std::slice::from_raw_parts(ptr.add(offset), len) })
}

/// 编码器的一个输出。`flags`：bit0 = 关键帧，bit1 = 参数集（codec config）；`codec`：0 H.264 / 1 HEVC / 2 AV1
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostVideo(
    env: JNIEnv,
    _: JClass,
    h: jlong,
    buf: JByteBuffer,
    offset: jint,
    len: jint,
    flags: jint,
    capture_us: jlong,
    codec: jint,
    width: jint,
    height: jint,
) {
    let Some(host) = host(h) else { return };
    let Some(data) = direct_slice(&env, &buf, offset, len) else {
        tracing::warn!("hostVideo: bad buffer");
        return;
    };
    let codec = match codec {
        1 => Codec::H265,
        2 => Codec::AV1,
        _ => Codec::H264,
    };
    guard((), || {
        host.video_hub().push(
            data,
            flags & 1 != 0,
            flags & 2 != 0,
            capture_us.max(0) as u64,
            codec,
            width.clamp(0, u16::MAX as i32) as u16,
            height.clamp(0, u16::MAX as i32) as u16,
        )
    });
}

/// 一个 Opus 包
#[unsafe(no_mangle)]
pub extern "system" fn Java_app_airhand_android_core_Native_hostAudio(
    env: JNIEnv,
    _: JClass,
    h: jlong,
    buf: JByteBuffer,
    offset: jint,
    len: jint,
    capture_us: jlong,
) {
    let Some(host) = host(h) else { return };
    let Some(data) = direct_slice(&env, &buf, offset, len) else {
        return;
    };
    guard((), || host.audio_hub().push(data, capture_us.max(0) as u64));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&b), "0001abff");
        assert_eq!(unhex::<4>("0001abff"), Some(b));
        assert_eq!(unhex::<4>("0001ABFF"), Some(b));
        assert_eq!(unhex::<4>("0001ab"), None);
        assert_eq!(unhex::<2>("zzzz"), None);
    }
}
