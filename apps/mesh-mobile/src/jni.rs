//! JNI entry points for `org.socialmesh.android.Native` (see
//! android/app/src/main/java/org/socialmesh/android/Native.kt).
//!
//! A node is a `long` handle. Errors become `java.lang.RuntimeException`.
//! Calls on handle 0 or after `stop` do nothing (or return null).

use crate::{Mobile, Options};
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use std::path::PathBuf;
use std::time::Duration;

fn mobile<'a>(handle: jlong) -> Option<&'a Mobile> {
    // SAFETY: handles come from `start` (Box::into_raw) and are freed only by
    // `free`, which the Kotlin side calls exactly once, after every other call.
    (handle != 0).then(|| unsafe { &*(handle as *const Mobile) })
}

fn throw(env: &mut JNIEnv, msg: impl std::fmt::Display) {
    let _ = env.throw_new("java/lang/RuntimeException", msg.to_string());
}

fn string(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_start(
    mut env: JNIEnv,
    _: JClass,
    home: JString,
    beacon: jboolean,
    lan: jboolean,
) -> jlong {
    let home: String = match env.get_string(&home) {
        Ok(s) => s.into(),
        Err(e) => {
            throw(&mut env, e);
            return 0;
        }
    };
    let opts = Options { beacon: beacon != JNI_FALSE, lan: lan != JNI_FALSE };
    match Mobile::start(&PathBuf::from(home), opts) {
        Ok(m) => Box::into_raw(Box::new(m)) as jlong,
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_stop(_: JNIEnv, _: JClass, handle: jlong) {
    if let Some(m) = mobile(handle) {
        m.stop();
    }
}

/// Release the node. Call once, after `stop` and after the Bluetooth
/// driver's threads have finished.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_free(_: JNIEnv, _: JClass, handle: jlong) {
    if handle != 0 {
        // SAFETY: see `mobile`.
        drop(unsafe { Box::from_raw(handle as *mut Mobile) });
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_nextRadioCommand(
    env: JNIEnv,
    _: JClass,
    handle: jlong,
    timeout_ms: jint,
) -> jbyteArray {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.next_radio_command(Duration::from_millis(timeout_ms.max(0) as u64)) {
        Some(b) => env.byte_array_from_slice(&b).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_radioEvent(
    env: JNIEnv,
    _: JClass,
    handle: jlong,
    event: JByteArray,
) -> jboolean {
    let Some(m) = mobile(handle) else { return JNI_FALSE };
    match env.convert_byte_array(&event) {
        Ok(b) if m.radio_event(&b) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_publish(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    text: JString,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let text: String = match env.get_string(&text) {
        Ok(s) => s.into(),
        Err(e) => {
            throw(&mut env, e);
            return std::ptr::null_mut();
        }
    };
    match m.publish(&text) {
        Ok(id) => string(&mut env, &id),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_feedJson(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    limit: jint,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.feed_json(limit.max(0) as usize) {
        Ok(json) => string(&mut env, &json),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_nextEvent(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    timeout_ms: jint,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.next_event(Duration::from_millis(timeout_ms.max(0) as u64)) {
        Some(line) => string(&mut env, &line),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_links(mut env: JNIEnv, _: JClass, handle: jlong) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let links = m.links();
    string(&mut env, &links)
}

/// "posting-id link-key", both short.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_ids(mut env: JNIEnv, _: JClass, handle: jlong) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let (me, link) = m.ids();
    string(&mut env, &format!("{me} {link}"))
}

/// The newest public post as 16-bit little-endian PCM, mono, 8000 Hz.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_soundOfNewest(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    fast: jboolean,
) -> jbyteArray {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.sound_of_newest(fast != JNI_FALSE) {
        Ok(pcm) => env.byte_array_from_slice(&pcm).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Microphone audio (16-bit little-endian PCM, mono, `rate` Hz). Returns
/// lines worth showing, newline-separated, or null.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_hear(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    pcm: JByteArray,
    rate: jint,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let Ok(pcm) = env.convert_byte_array(&pcm) else { return std::ptr::null_mut() };
    let lines = m.hear(&pcm, rate.max(1) as u32);
    if lines.is_empty() {
        std::ptr::null_mut()
    } else {
        string(&mut env, &lines.join("\n"))
    }
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_stopHearing(_: JNIEnv, _: JClass, handle: jlong) {
    if let Some(m) = mobile(handle) {
        m.stop_hearing();
    }
}

fn jstr(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(Into::into)
}

/// Sites held here, as JSON (see `Mobile::sites_json`).
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_sitesJson(mut env: JNIEnv, _: JClass, handle: jlong) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.sites_json() {
        Ok(json) => string(&mut env, &json),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Queue a site's files; returns its index path.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_siteWant(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    address: JString,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let Some(address) = jstr(&mut env, &address) else { return std::ptr::null_mut() };
    match m.site_want(&address) {
        Ok(index) => string(&mut env, &index),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Files present and in total, as `have << 32 | all`, or -1.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_siteProgress(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    address: JString,
) -> jlong {
    let Some(m) = mobile(handle) else { return -1 };
    let Some(address) = jstr(&mut env, &address) else { return -1 };
    match m.site_progress(&address) {
        Ok((have, all)) => ((have as i64) << 32) | all as i64,
        Err(_) => -1,
    }
}

/// One file for the viewer, packed as
/// `u16 status | str media_type | u16 header_count | header_count × (str name | str value) | body`
/// with each `str` a u16 length then UTF-8. Null if the site isn't held.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_siteFile(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    address: JString,
    path: JString,
) -> jbyteArray {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let (Some(address), Some(path)) = (jstr(&mut env, &address), jstr(&mut env, &path)) else {
        return std::ptr::null_mut();
    };
    let Ok(f) = m.site_file(&address, &path) else { return std::ptr::null_mut() };
    let mut out = Vec::with_capacity(f.data.len() + 512);
    let put_str = |out: &mut Vec<u8>, s: &str| {
        let b = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
        out.extend_from_slice(&(b.len() as u16).to_be_bytes());
        out.extend_from_slice(b);
    };
    out.extend_from_slice(&f.status.to_be_bytes());
    put_str(&mut out, &f.media_type);
    out.extend_from_slice(&(f.headers.len() as u16).to_be_bytes());
    for (k, v) in &f.headers {
        put_str(&mut out, k);
        put_str(&mut out, v);
    }
    out.extend_from_slice(&f.data);
    env.byte_array_from_slice(&out).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut())
}
