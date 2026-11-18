//! Two phones with no link at all: one plays a post through its speaker,
//! the other hears it through its microphone.

use socialmesh::{Mobile, Options};

/// Play `pcm` (8 kHz) into a microphone recording at 48 kHz, quieter, with
/// a little hiss, delivered in 20 ms pieces as Android's AudioRecord does.
fn record(pcm: &[u8]) -> Vec<Vec<u8>> {
    let mut seed = 1u32;
    let mut hiss = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) as f32 / 65536.0 - 0.5
    };
    let played: Vec<f32> = pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
    let mut mic = Vec::new();
    for w in played.windows(2) {
        for k in 0..6 {
            let s = w[0] + (w[1] - w[0]) * k as f32 / 6.0;
            mic.push(s * 0.3 + hiss() * 0.02);
        }
    }
    mic.extend((0..48_000).map(|_| hiss() * 0.02)); // keep listening a moment after
    let bytes: Vec<u8> = mic.iter().flat_map(|s| ((s * 32767.0) as i16).to_le_bytes()).collect();
    bytes.chunks(960 * 2).map(|c| c.to_vec()).collect()
}

#[test]
fn a_post_crosses_from_speaker_to_microphone() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let opts = Options { beacon: false, lan: false };
    let a = Mobile::start(da.path(), opts).unwrap();
    let b = Mobile::start(db.path(), opts).unwrap();

    assert!(a.sound_of_newest(true).is_err(), "nothing to play yet");
    let id = a.publish("heard, not seen").unwrap();
    // A site published afterwards is newer, but sound carries posts only.
    let site = vec![("index.html".to_string(), b"<h1>hi</h1>".to_vec())];
    a.publish_site(socialmesh::SiteTarget::New, "Newer site", "index.html", site, false).unwrap();
    let pcm = a.sound_of_newest(true).unwrap();

    let mut lines = Vec::new();
    for piece in record(&pcm) {
        lines.extend(b.hear(&piece, 48_000));
    }
    assert!(lines.iter().any(|l| l.starts_with("New post by sound (1 new")), "{lines:?}");
    assert!(b.feed_json(10).unwrap().contains(&id));

    // Hearing it again changes nothing.
    b.stop_hearing();
    let again: Vec<String> = record(&pcm).iter().flat_map(|p| b.hear(p, 48_000)).collect();
    assert!(again.iter().any(|l| l == "Heard a post by sound (already here)"), "{again:?}");
}

/// Replays a real recording (SOCIALMESH_WAV, 16-bit mono) through `hear`
/// and prints, per second, whether the modem would think someone's playing.
#[test]
#[ignore]
fn busy_on_a_real_recording() {
    let Ok(path) = std::env::var("SOCIALMESH_WAV") else { return };
    let wav = std::fs::read(path).unwrap();
    let rate = u32::from_le_bytes(wav[24..28].try_into().unwrap());
    let pcm = &wav[44..];
    let dir = tempfile::tempdir().unwrap();
    let m = socialmesh::Mobile::start(dir.path(), socialmesh::Options { beacon: false, lan: false }).unwrap();
    let chunk = (rate / 4) as usize * 2;
    let mut line = String::new();
    for (i, c) in pcm.chunks(chunk).enumerate() {
        m.hear(c, rate);
        line.push(if m.sound_busy() { '#' } else { '.' });
        if i % 40 == 39 {
            println!("{:4}s {line}", (i + 1) / 4);
            line.clear();
        }
    }
}
