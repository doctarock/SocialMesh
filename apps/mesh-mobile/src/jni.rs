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

/// This device's site keys, as JSON (see `Mobile::my_sites_json`).
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_mySitesJson(mut env: JNIEnv, _: JClass, handle: jlong) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.my_sites_json() {
        Ok(json) => string(&mut env, &json),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// `files` packed as repeated `u16 path_len | path (UTF-8) | u32 len | bytes`.
fn unpack_files(b: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let n = u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize;
        let path = String::from_utf8(b.get(i + 2..i + 2 + n)?.to_vec()).ok()?;
        i += 2 + n;
        let len = u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize;
        let data = b.get(i + 4..i + 4 + len)?.to_vec();
        i += 4 + len;
        out.push((path, data));
    }
    Some(out)
}

/// Publish a site. `key` names one of this device's site keys (a new
/// version), or is null for a new site; `ownerless` publishes with no key.
/// Throws with a message starting `METADATA ` for a file whose hidden
/// details can't be removed (retry with `keepMetadata` if the user agrees).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_socialmesh_android_Native_publishSite(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    key: JString,
    ownerless: jboolean,
    title: JString,
    index: JString,
    files: JByteArray,
    keep_metadata: jboolean,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let key = if key.is_null() { None } else { jstr(&mut env, &key) };
    let (Some(title), Some(index)) = (jstr(&mut env, &title), jstr(&mut env, &index)) else {
        throw(&mut env, "bad title or index");
        return std::ptr::null_mut();
    };
    let Some(files) = env.convert_byte_array(&files).ok().and_then(|b| unpack_files(&b)) else {
        throw(&mut env, "bad file list");
        return std::ptr::null_mut();
    };
    let target = match (&key, ownerless != JNI_FALSE) {
        (_, true) => crate::SiteTarget::Ownerless,
        (Some(k), false) => crate::SiteTarget::Update(k),
        (None, false) => crate::SiteTarget::New,
    };
    match m.publish_site(target, &title, &index, files, keep_metadata != JNI_FALSE) {
        Ok(line) => string(&mut env, &line),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// A string result, or a thrown RuntimeException.
fn string_or_throw(env: &mut JNIEnv, r: anyhow::Result<String>) -> jstring {
    match r {
        Ok(s) => string(env, &s),
        Err(e) => {
            throw(env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// `{"title","address","version"}` for an app's code.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_appInfo(mut env: JNIEnv, _: JClass, handle: jlong, address: JString) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let Some(address) = jstr(&mut env, &address) else { return std::ptr::null_mut() };
    let r = m.app_info(&address);
    string_or_throw(&mut env, r)
}

/// Entries to an app, as JSON.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_appEntries(mut env: JNIEnv, _: JClass, handle: jlong, address: JString) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let Some(address) = jstr(&mut env, &address) else { return std::ptr::null_mut() };
    let r = m.app_entries(&address);
    string_or_throw(&mut env, r)
}

/// The short ID anonymous entries to this app are signed with.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_appAnonymousId(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    address: JString,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let Some(address) = jstr(&mut env, &address) else { return std::ptr::null_mut() };
    let r = m.app_anonymous_id(&address);
    string_or_throw(&mut env, r)
}

/// Publish an entry the person agreed to. Returns its ID (hex).
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_appSubmit(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    address: JString,
    data: JString,
    anonymous: jboolean,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let (Some(address), Some(data)) = (jstr(&mut env, &address), jstr(&mut env, &data)) else {
        return std::ptr::null_mut();
    };
    let r = m.app_submit(&address, &data, anonymous != JNI_FALSE);
    string_or_throw(&mut env, r)
}

// ---- sound modem -------------------------------------------------------------

/// The `nth` of the newest public posts as sound (see `Mobile::sound_of_recent`).
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_soundOfRecent(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    nth: jint,
    fast: jboolean,
) -> jbyteArray {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    match m.sound_of_recent(nth.max(0) as usize, fast != JNI_FALSE) {
        Ok(pcm) => env.byte_array_from_slice(&pcm).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Someone nearby is playing a post right now.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_soundBusy(_: JNIEnv, _: JClass, handle: jlong) -> jboolean {
    match mobile(handle) {
        Some(m) if m.sound_busy() => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

// ---- hosts ---------------------------------------------------------------------

/// Saved and found hosts (JSON, see `Mobile::hosts_json`).
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_hostsJson(mut env: JNIEnv, _: JClass, handle: jlong) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let r = m.hosts_json();
    string_or_throw(&mut env, r)
}

/// Save and connect to a host (address or invitation). Blocks for up to
/// several seconds while it tries the address.
#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_addHost(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    input: JString,
) -> jstring {
    let Some(m) = mobile(handle) else { return std::ptr::null_mut() };
    let Some(input) = jstr(&mut env, &input) else { return std::ptr::null_mut() };
    let r = m.add_host(&input);
    string_or_throw(&mut env, r)
}

#[no_mangle]
pub extern "system" fn Java_org_socialmesh_android_Native_removeHost(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    addr: JString,
) -> jboolean {
    let Some(m) = mobile(handle) else { return JNI_FALSE };
    let Some(addr) = jstr(&mut env, &addr) else { return JNI_FALSE };
    match m.remove_host(&addr) {
        Ok(true) => JNI_TRUE,
        Ok(false) => JNI_FALSE,
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            JNI_FALSE
        }
    }
}
