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

/// Microphone state: both modem profiles are listened for at once.
pub(crate) struct Ears {
    robust: Demodulator,
    fast: Demodulator,
    rx: Receiver,
    heard_any: bool,
}

impl Default for Ears {
    fn default() -> Ears {
        Ears {
            robust: Demodulator::new(Profile::ROBUST),
            fast: Demodulator::new(Profile::FAST),
            rx: Receiver::default(),
            heard_any: false,
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
        let profile = if fast { Profile::FAST } else { Profile::ROBUST };
        let obj = self.node.with_store(|s| -> Result<_> {
            // The newest public *post*: sites and other kinds need their files,
            // which sound is far too slow to carry.
            for id in s.shareable_ids(true)?.iter().rev() {
                let Some(o) = s.get(id)? else { continue };
                // Embargoed (still on the private route) or otherwise unshareable: never broadcast.
                if o.object().kind == mesh_object::KIND_POST && s.shareable(id, true)? {
                    return Ok(o);
                }
            }
            bail!("no public post to play yet")
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
        let mut frames = ears.robust.push(&samples);
        frames.extend(ears.fast.push(&samples));
        let mut lines = Vec::new();
        for f in frames {
            let rejected = ears.rx.rejected;
            let payload = ears.rx.push(&f);
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

    /// Forget partly heard sound (the microphone was turned off).
    pub fn stop_hearing(&self) {
        *self.ears.lock().unwrap() = Ears::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_roundtrip() {
        let x = [0.0, 0.5, -0.5, 0.999];
        let back = from_pcm16(&pcm16(&x));
        for (a, b) in x.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3);
        }
    }
}
