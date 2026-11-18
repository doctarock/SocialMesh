//! Posts by sound, live: the speaker plays public posts as tones and the
//! microphone picks them up (see `mesh_transport_audio`). One way, no
//! pairing, nothing to connect: anyone in earshot with the app hears them.
//!
//! Only posts that are already public are ever played. Sound reaches
//! everyone nearby and can be traced to the speaker, so a post still on its
//! private route would give away this device as its source.

use crate::Mobile;
use anyhow::{bail, Result};
use mesh_datagram::{Receiver, Sender};
use mesh_transport_audio::{modulate, resample, Demodulator, Profile};


/// Symbol bytes per sound frame, as the CLI: small frames lose less to a
/// burst of noise.
const SYMBOL: u16 = 64;
/// Extra frames, in percent, so a cough or a door slam costs nothing.
const EXTRA_PERCENT: usize = 50;
/// Silence around the tones: some phones clip the start of playback.
const PAD_SECONDS: f32 = 0.3;
/// How many of the newest public posts the sound modem takes turns playing.
pub const RECENT: usize = 5;

/// Microphone state: both modem profiles are listened for at once.
pub(crate) struct Ears {
    robust: Demodulator,
    fast: Demodulator,
    rx: Receiver,
    heard_any: bool,
    /// Seconds of sound heard since the last frame that checked out.
    since_frame: Option<f32>,
    carrier: Carrier,
}

/// Is someone playing? A transmission counts as soon as it starts, even one
/// too faint or garbled to decode: there's sound at the modem's tones, and
/// much of what's heard is at those tones. Needs no history, so it works
/// the moment the microphone opens, even mid-transmission.
///
/// Measured on phones a metre or two apart: a quiet room has tone power
/// ~5e-8 with ~1% of its energy at the tones; transmissions, faint ones
/// included, 1e-4 to 1e-2 with 25–50% at the tones.
pub(crate) struct Carrier {
    /// Seconds of sound heard since the last that sounded like the modem
    /// (counted in audio, not wall time); None: none yet.
    since_loud: Option<f32>,
}

/// Tone power below this is effectively silence (amplitude ~0.002).
const CARRIER_MIN: f32 = 1e-6;
/// Share of the sound's energy at the tones above which it's the modem.
const CARRIER_SHARE: f32 = 0.15;

impl Carrier {
    fn push(&mut self, samples: &[f32], rate: u32) {
        if samples.len() < 64 {
            return;
        }
        let p = tone_power(samples, rate);
        let energy = samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32;
        let secs = samples.len() as f32 / rate as f32;
        self.since_loud = if p > CARRIER_MIN && p > energy * CARRIER_SHARE {
            Some(0.0)
        } else {
            self.since_loud.map(|t| t + secs)
        };
    }

    /// Modem-like sound within the last `secs` of what was heard.
    pub(crate) fn recent(&self, secs: f32) -> bool {
        self.since_loud.is_some_and(|t| t < secs)
    }
}

/// Power at the 16 modem tones (Goertzel), summed over the tones and
/// averaged over symbol-long slices (the tone changes every symbol, so
/// longer windows would smear it away). Samples in [-1, 1].
fn tone_power(samples: &[f32], rate: u32) -> f32 {
    let slice = (rate as usize / 50).max(16); // 20 ms, one ROBUST symbol
    let mut total = 0.0;
    let mut slices = 0;
    for chunk in samples.chunks_exact(slice) {
        let n = chunk.len() as f32;
        for t in 0..16 {
            let f = Profile::ROBUST.base_hz + Profile::ROBUST.spacing_hz * t as f32;
            let k = 2.0 * (std::f32::consts::TAU * f / rate as f32).cos();
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in chunk {
                let s0 = x + k * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            total += (s1 * s1 + s2 * s2 - k * s1 * s2) / (n * n);
        }
        slices += 1;
    }
    if slices == 0 {
        0.0
    } else {
        total / slices as f32
    }
}

impl Default for Ears {
    fn default() -> Ears {
        Ears {
            robust: Demodulator::new(Profile::ROBUST),
            fast: Demodulator::new(Profile::FAST),
            rx: Receiver::default(),
            heard_any: false,
            since_frame: None,
            carrier: Carrier { since_loud: None },
        }
    }
}

/// Samples in [-1, 1] as 16-bit little-endian PCM.
fn pcm16(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect()
}

fn from_pcm16(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect()
}

impl Mobile {
    /// The sample rate of [`Mobile::sound_of_newest`].
    pub const SOUND_RATE: u32 = 8000;

    /// The newest public post as sound: 16-bit little-endian mono PCM at
    /// [`Mobile::SOUND_RATE`], ready to play. `fast` doubles the speed for
    /// quiet rooms and cables. Errors if nothing is public yet.
    pub fn sound_of_newest(&self, fast: bool) -> Result<Vec<u8>> {
        self.sound_of_recent(0, fast)
    }

    /// Like [`Mobile::sound_of_newest`], for the `nth` newest of the
    /// [`RECENT`] newest public posts (counting round, so any `nth` works):
    /// the sound modem takes turns through them.
    pub fn sound_of_recent(&self, nth: usize, fast: bool) -> Result<Vec<u8>> {
        let profile = if fast { Profile::FAST } else { Profile::ROBUST };
        let obj = self.node.with_store(|s| -> Result<_> {
            // Public *posts*: sites and other kinds need their files, which
            // sound is far too slow to carry.
            let mut posts = Vec::new();
            for id in s.shareable_ids(true)?.iter().rev() {
                let Some(o) = s.get(id)? else { continue };
                // Embargoed (still on the private route) or otherwise unshareable: never broadcast.
                if o.object().kind == mesh_object::KIND_POST && s.shareable(id, true)? {
                    posts.push(o);
                    if posts.len() == RECENT {
                        break;
                    }
                }
            }
            if posts.is_empty() {
                bail!("no public post to play yet")
            }
            let n = nth % posts.len();
            Ok(posts.swap_remove(n))
        })?;
        let payload = mesh_transport_file::bundle(&[obj]);
        let mut tx = Sender::new(&payload, SYMBOL)?;
        let k = tx.source_frames();
        let frames = tx.frames(k + k * EXTRA_PERCENT / 100 + 2);
        let tones = modulate(&frames, &profile)?;
        let pad = vec![0.0; (PAD_SECONDS * profile.sample_rate as f32) as usize];
        Ok(pcm16(&[&pad[..], &tones, &pad].concat()))
    }

    /// Microphone audio (16-bit little-endian mono PCM at `rate`), in pieces
    /// as it's recorded. Posts heard in full are verified and kept, and the
    /// node passes them on like any other. Returns lines worth showing.
    pub fn hear(&self, pcm: &[u8], rate: u32) -> Vec<String> {
        let mut samples = from_pcm16(pcm);
        if rate != Self::SOUND_RATE {
            samples = resample(&samples, rate, Self::SOUND_RATE);
        }
        let mut ears = self.ears.lock().unwrap();
        ears.carrier.push(&samples, Self::SOUND_RATE);
        let secs = samples.len() as f32 / Self::SOUND_RATE as f32;
        ears.since_frame = ears.since_frame.map(|t| t + secs);
        let mut frames = ears.robust.push(&samples);
        frames.extend(ears.fast.push(&samples));
        let mut lines = Vec::new();
        for f in frames {
            let rejected = ears.rx.rejected;
            let payload = ears.rx.push(&f);
            if ears.rx.rejected == rejected {
                ears.since_frame = Some(0.0);
            }
            if !ears.heard_any && ears.rx.rejected == rejected {
                ears.heard_any = true;
                lines.push("Hearing a post by sound…".to_string());
            }
            let Some(payload) = payload else { continue };
            ears.heard_any = false;
            match self.node.with_store(|s| mesh_transport_file::import(s, &mut payload.as_slice())) {
                Ok(r) if r.new > 0 => lines.push(format!("New post by sound ({} new, {} already here)", r.new, r.duplicate)),
                Ok(r) if r.duplicate > 0 => lines.push("Heard a post by sound (already here)".to_string()),
                Ok(_) => lines.push("Heard a post by sound, but it failed verification".to_string()),
                Err(e) => lines.push(format!("Heard something unreadable by sound: {e:#}")),
            }
        }
        lines
    }

    /// Someone nearby is playing: modem tones, or a good frame, in the last
    /// three seconds of sound heard. The sound
    /// modem waits rather than talk over them.
    pub fn sound_busy(&self) -> bool {
        let ears = self.ears.lock().unwrap();
        // Time counted in sound heard, not on the clock (the
        // microphone hands it over in quarter seconds as it's recorded).
        ears.since_frame.is_some_and(|t| t < 3.0) || ears.carrier.recent(3.0)
    }

    /// Forget partly heard sound (the microphone was turned off).
    pub fn stop_hearing(&self) {
        *self.ears.lock().unwrap() = Ears::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transmission_is_noticed_from_its_first_moments_even_faint() {
        let mut c = Carrier { since_loud: None };
        // A quiet room: a little broadband noise.
        let mut seed = 1u32;
        let mut noise = |n: usize, a: f32| -> Vec<f32> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    ((seed >> 16) as f32 / 32768.0 - 1.0) * a
                })
                .collect()
        };
        for _ in 0..20 {
            c.push(&noise(2000, 0.003), 8000);
        }
        c.push(&[0.0; 2000], 8000); // a microphone warming up
        c.push(&noise(2000, 0.3), 8000); // a loud bang: loud, but not tones
        assert!(!c.recent(3.0), "noise isn't someone playing");
        // Faint tones (amplitude 0.02, as a weak speaker across a room).
        let tones = modulate(&[vec![0x5A; 40]], &Profile::ROBUST).unwrap();
        let faint: Vec<f32> = tones.iter().zip(noise(tones.len(), 0.003)).map(|(t, n)| t * 0.02 + n).collect();
        c.push(&faint[..2000], 8000);
        assert!(c.recent(3.0), "noticed in the first quarter second");
        for _ in 0..12 {
            c.push(&noise(2000, 0.003), 8000);
        }
        assert!(!c.recent(3.0), "three seconds of quiet later, the air is free");
    }

    #[test]
    fn pcm_roundtrip() {
        let x = [0.0, 0.5, -0.5, 0.999];
        let back = from_pcm16(&pcm16(&x));
        for (a, b) in x.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3);
        }
    }
}
