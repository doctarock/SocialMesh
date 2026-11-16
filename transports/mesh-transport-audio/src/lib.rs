//! Sound modem: datagram frames as audible tones.
//!
//! 16-tone frequency-shift keying (16-FSK): each symbol is one of 16 tones and
//! carries 4 bits. Tones sit between 1 000 and 2 500 Hz, inside the voice
//! band, so the signal also survives phone calls, walkie-talkies, radio and
//! cheap speakers. Detection is non-coherent: it compares the energy of the
//! tones, so it doesn't care about volume or phase.
//!
//! On air, each frame is:
//!
//! ```text
//! preamble (16 symbols, every tone once) | length (2 symbols) | data (2 symbols per byte) | silence
//! ```
//!
//! The receiver finds preambles by how much of each window's energy sits on
//! the expected tone, then reads the symbols after it. Frames are independent:
//! a missed or damaged one costs only itself, and the datagram layer's
//! fountain code makes up for it.
//!
//! This is a deliberately simple, well-understood modem. It is not fast:
//! 200 bit/s with the robust profile, 400 bit/s with the fast one. That is
//! enough for text posts, not for photos.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Modem settings. Both ends must use the same profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    pub sample_rate: u32,
    /// Frequency of tone 0, in Hz.
    pub base_hz: f32,
    /// Gap between tones, in Hz. A multiple of `sample_rate / symbol_samples`
    /// keeps the tones orthogonal.
    pub spacing_hz: f32,
    /// Samples per symbol.
    pub symbol_samples: usize,
}

impl Profile {
    /// 20 ms symbols, 200 bit/s. Tolerates room echo of a few milliseconds.
    pub const ROBUST: Profile = Profile { sample_rate: 8000, base_hz: 1000.0, spacing_hz: 100.0, symbol_samples: 160 };
    /// 10 ms symbols, 400 bit/s. For cables, calls and quiet rooms.
    pub const FAST: Profile = Profile { sample_rate: 8000, base_hz: 1000.0, spacing_hz: 100.0, symbol_samples: 80 };

    fn tone_hz(&self, tone: u8) -> f32 {
        self.base_hz + self.spacing_hz * tone as f32
    }

    /// Bits per second of frame data (excluding preamble and gaps).
    pub fn bit_rate(&self) -> f32 {
        4.0 * self.sample_rate as f32 / self.symbol_samples as f32
    }

    /// Seconds of audio for one frame of `len` bytes.
    pub fn frame_seconds(&self, len: usize) -> f32 {
        ((PREAMBLE.len() + 2 + 2 * len + GAP_SYMBOLS) * self.symbol_samples) as f32 / self.sample_rate as f32
    }
}

/// Every tone once, in an order with low self-similarity, so the preamble
/// matches itself sharply and nothing else well.
const PREAMBLE: [u8; 16] = [0, 9, 3, 14, 6, 1, 12, 7, 15, 4, 10, 2, 13, 8, 5, 11];
const GAP_SYMBOLS: usize = 2;
const AMPLITUDE: f32 = 0.5;
/// Average share of in-band energy on the expected tone for a preamble to
/// count (noise scores about 1/16 = 0.06, a clean preamble near 1).
const DETECT: f32 = 0.35;

// ---------------------------------------------------------------- transmit

/// Turn frames (each at most 255 bytes) into audio samples in [-1, 1].
pub fn modulate(frames: &[Vec<u8>], p: &Profile) -> Result<Vec<f32>> {
    let mut out = Vec::new();
    for f in frames {
        if f.is_empty() || f.len() > 255 {
            bail!("frame of {} bytes; the modem carries 1–255", f.len());
        }
        let mut symbols: Vec<u8> = PREAMBLE.to_vec();
        for b in std::iter::once(f.len() as u8).chain(f.iter().copied()) {
            symbols.push(b >> 4);
            symbols.push(b & 0x0f);
        }
        out.extend(tones(&symbols, p));
        out.extend(std::iter::repeat(0.0).take(GAP_SYMBOLS * p.symbol_samples));
    }
    Ok(out)
}

/// Continuous-phase tones (no clicks between symbols), faded in and out.
fn tones(symbols: &[u8], p: &Profile) -> Vec<f32> {
    let n = symbols.len() * p.symbol_samples;
    let fade = (p.sample_rate as usize / 500).min(n / 2); // 2 ms
    let mut out = Vec::with_capacity(n);
    let mut phase = 0f32;
    for &s in symbols {
        let step = std::f32::consts::TAU * p.tone_hz(s) / p.sample_rate as f32;
        for _ in 0..p.symbol_samples {
            out.push(AMPLITUDE * phase.sin());
            phase = (phase + step) % std::f32::consts::TAU;
        }
    }
    for i in 0..fade {
        let g = i as f32 / fade as f32;
        out[i] *= g;
        out[n - 1 - i] *= g;
    }
    out
}

// ---------------------------------------------------------------- receive

/// Energy of `freq` in `x` (Goertzel), scaled so a full-window tone of
/// amplitude A gives about A² / 2 per sample (comparable to signal power).
fn tone_power(x: &[f32], freq: f32, rate: u32) -> f32 {
    let w = std::f32::consts::TAU * freq / rate as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let mag2 = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * mag2 / (x.len() as f32 * x.len() as f32)
}

/// Most likely tone in one symbol window. The first quarter of the window is
/// skipped: that's where the previous symbol's echo lands.
fn read_symbol(window: &[f32], p: &Profile) -> u8 {
    let skip = window.len() / 4;
    let w = &window[skip..];
    (0..16u8)
        .map(|t| (t, tone_power(w, p.tone_hz(t), p.sample_rate)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(t, _)| t)
        .unwrap_or(0)
}

/// Find every frame in `samples` and return its bytes. Damaged frames come
/// back too (the datagram checksum rejects them); missing ones don't.
pub fn demodulate(samples: &[f32], p: &Profile) -> Vec<Vec<u8>> {
    scan(samples, p).0
}

/// Live decoding: feed audio as it arrives (from a microphone) and get each
/// frame once, as soon as all of it has been heard. Holds at most one
/// frame's worth of audio between calls.
pub struct Demodulator {
    profile: Profile,
    buf: Vec<f32>,
}

impl Demodulator {
    pub fn new(profile: Profile) -> Demodulator {
        Demodulator { profile, buf: Vec::new() }
    }

    /// Add samples at the profile's rate; returns the frames they completed.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(samples);
        let (frames, done) = scan(&self.buf, &self.profile);
        self.buf.drain(..done);
        frames
    }
}

/// Frames in `samples`, and how far the samples are finished with: a frame
/// can't start before that point and still be unread. Everything after it
/// (a frame cut off by the end, or the tail too short to search) is needed
/// again once more audio arrives.
fn scan(samples: &[f32], p: &Profile) -> (Vec<Vec<u8>>, usize) {
    let n = p.symbol_samples;
    let pre_len = PREAMBLE.len() * n;
    if samples.len() < pre_len {
        return (Vec::new(), 0);
    }
    // All 16 tone powers on a grid of `step`-spaced, symbol-long windows,
    // computed once. A preamble at grid point g puts PREAMBLE[i] in window
    // g + i*SUB. Score = how much of the in-band energy sits on the expected
    // tone, averaged over the preamble: about 1/16 for noise of any loudness,
    // near 1 for a clean preamble.
    const SUB: usize = 8;
    let step = (n / SUB).max(1);
    let windows = (samples.len() - n) / step + 1;
    let grid: Vec<[f32; 16]> = (0..windows)
        .map(|g| {
            let w = &samples[g * step..g * step + n];
            std::array::from_fn(|t| tone_power(w, p.tone_hz(t as u8), p.sample_rate))
        })
        .collect();
    let ratio = |powers: &[f32; 16], tone: u8| -> f32 {
        let all: f32 = powers.iter().sum();
        if all < 1e-9 { 0.0 } else { powers[tone as usize] / all }
    };
    let grid_score = |g: usize| -> f32 {
        PREAMBLE.iter().enumerate().map(|(i, &t)| ratio(&grid[g + i * SUB], t)).sum::<f32>() / PREAMBLE.len() as f32
    };
    // Same score at an exact sample offset, for fine alignment.
    let exact_score = |start: usize| -> f32 {
        PREAMBLE
            .iter()
            .enumerate()
            .map(|(i, &t)| {
                let w = &samples[start + i * n..start + (i + 1) * n];
                let powers: [f32; 16] = std::array::from_fn(|j| tone_power(w, p.tone_hz(j as u8), p.sample_rate));
                ratio(&powers, t)
            })
            .sum::<f32>()
            / PREAMBLE.len() as f32
    };
    let last_g = windows.saturating_sub((PREAMBLE.len() - 1) * SUB + 1);

    let mut frames = Vec::new();
    let mut g = 0;
    while g < last_g {
        if grid_score(g) < DETECT {
            g += 1;
            continue;
        }
        // The score peaks at the true start: best grid point within a symbol,
        // then the best sample around it.
        let best_g = (g..(g + SUB).min(last_g)).max_by(|a, b| grid_score(*a).total_cmp(&grid_score(*b))).unwrap_or(g);
        let centre = best_g * step;
        let (lo, hi) = (centre.saturating_sub(step), (centre + step).min(samples.len() - pre_len));
        let start = (lo..=hi).max_by(|a, b| exact_score(*a).total_cmp(&exact_score(*b))).unwrap_or(centre);
        let mut at = start + pre_len;
        let byte = |at: &mut usize| -> Option<u8> {
            if *at + 2 * n > samples.len() {
                return None;
            }
            let hi = read_symbol(&samples[*at..*at + n], p);
            let lo = read_symbol(&samples[*at + n..*at + 2 * n], p);
            *at += 2 * n;
            Some(hi << 4 | lo)
        };
        let Some(len) = byte(&mut at) else { return (frames, start) };
        let mut f = Vec::with_capacity(len as usize);
        for _ in 0..len {
            match byte(&mut at) {
                Some(b) => f.push(b),
                // Cut off by the end of the samples: keep it for next time.
                None => return (frames, start),
            }
        }
        let resume = if f.len() == len as usize && len > 0 {
            frames.push(f);
            at
        } else {
            // Garbled length or cut off: resume just after this preamble.
            start + pre_len
        };
        g = resume.div_ceil(step);
    }
    (frames, (g.min(last_g) * step).min(samples.len()))
}

// ---------------------------------------------------------------- files

/// Write samples as a 16-bit mono WAV at the profile's rate.
pub fn write_wav(path: &Path, samples: &[f32], p: &Profile) -> Result<()> {
    let spec = hound::WavSpec { channels: 1, sample_rate: p.sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).with_context(|| format!("creating {}", path.display()))?;
    for s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

/// Read a WAV (any rate, mono or the first channel), resampled to the
/// profile's rate. Recordings from phones are usually 44.1 or 48 kHz.
pub fn read_wav(path: &Path, p: &Profile) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 / scale)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = raw.chunks(ch).map(|c| c[0]).collect();
    Ok(resample(&mono, spec.sample_rate, p.sample_rate))
}

/// Resample by averaging over each output sample's span (a crude low-pass
/// when going down) and linear interpolation (going up).
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = (x.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let a = i as f64 * ratio;
            if ratio > 1.0 {
                let (s, e) = (a as usize, ((a + ratio) as usize).min(x.len()));
                let span = &x[s..e.max(s + 1).min(x.len())];
                span.iter().sum::<f32>() / span.len() as f32
            } else {
                let j = a as usize;
                let frac = (a - j as f64) as f32;
                let next = x.get(j + 1).copied().unwrap_or(x[j]);
                x[j] * (1.0 - frac) + next * frac
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn frames(n: usize, len: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut r = StdRng::seed_from_u64(seed);
        (0..n).map(|_| (0..len).map(|_| r.gen()).collect()).collect()
    }

    /// White noise at `snr_db` below the signal's level, plus a gain change
    /// and some silence before it, as a recording would have.
    fn channel(signal: &[f32], snr_db: f32, gain: f32, seed: u64) -> Vec<f32> {
        let mut r = StdRng::seed_from_u64(seed);
        let power = AMPLITUDE * AMPLITUDE / 2.0;
        let sigma = (power / 10f32.powf(snr_db / 10.0)).sqrt();
        let mut noise = || {
            // Box-Muller.
            let (u1, u2): (f32, f32) = (r.gen_range(1e-6..1.0), r.gen());
            sigma * (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
        };
        let lead = 1234;
        let mut out: Vec<f32> = (0..lead).map(|_| noise()).collect();
        for s in signal {
            out.push(s * gain + noise());
        }
        out
    }

    #[test]
    fn clean_roundtrip() {
        let f = frames(3, 84, 1);
        let audio = modulate(&f, &Profile::FAST).unwrap();
        assert_eq!(demodulate(&audio, &Profile::FAST), f);
    }

    #[test]
    fn survives_noise_gain_and_offset() {
        let f = frames(4, 84, 2);
        let audio = modulate(&f, &Profile::ROBUST).unwrap();
        let rx = channel(&audio, 6.0, 0.3, 3);
        let got = demodulate(&rx, &Profile::ROBUST);
        let good = f.iter().filter(|x| got.contains(x)).count();
        assert!(good >= 3, "only {good}/4 frames intact at 6 dB SNR");
    }

    #[test]
    fn survives_a_48khz_recording() {
        let f = frames(2, 40, 4);
        let audio = modulate(&f, &Profile::FAST).unwrap();
        let rec = resample(&audio, 8000, 48000); // as a phone would record it
        let back = resample(&rec, 48000, 8000);
        assert_eq!(demodulate(&back, &Profile::FAST), f);
    }

    #[test]
    fn a_dropout_costs_only_the_frames_it_hits() {
        let f = frames(5, 60, 5);
        let mut audio = modulate(&f, &Profile::FAST).unwrap();
        // Someone coughs over / the line drops for the middle of frame 3.
        let per = audio.len() / 5;
        for s in &mut audio[2 * per + per / 3..2 * per + per / 2] {
            *s = 0.0;
        }
        let got = demodulate(&audio, &Profile::FAST);
        for (i, x) in f.iter().enumerate() {
            if i != 2 {
                assert!(got.contains(x), "frame {i} lost");
            }
        }
    }

    #[test]
    fn silence_and_noise_yield_nothing() {
        assert!(demodulate(&vec![0.0; 16000], &Profile::FAST).is_empty());
        let noise = channel(&vec![0.0; 16000], -10.0, 1.0, 6);
        assert!(demodulate(&noise, &Profile::FAST).is_empty());
    }

    /// Audio in odd-sized pieces, as a microphone delivers it: every frame
    /// comes out exactly once, and little audio is held between pieces.
    #[test]
    fn live_decoding_in_pieces() {
        let f = frames(6, 84, 10);
        let audio = channel(&modulate(&f, &Profile::ROBUST).unwrap(), 12.0, 0.4, 11);
        let mut d = Demodulator::new(Profile::ROBUST);
        let mut r = StdRng::seed_from_u64(12);
        let (mut got, mut at) = (Vec::new(), 0);
        while at < audio.len() {
            let n = r.gen_range(1..6000).min(audio.len() - at);
            got.extend(d.push(&audio[at..at + n]));
            at += n;
            let longest = Profile::ROBUST.frame_seconds(255) * 8000.0;
            assert!(d.buf.len() as f32 <= longest + 6000.0, "holding {} samples", d.buf.len());
        }
        got.extend(d.push(&vec![0.0; 8000])); // trailing silence
        assert_eq!(got, f);
    }

    #[test]
    fn wav_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mesh-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let f = frames(2, 30, 7);
        write_wav(&path, &modulate(&f, &Profile::FAST).unwrap(), &Profile::FAST).unwrap();
        assert_eq!(demodulate(&read_wav(&path, &Profile::FAST).unwrap(), &Profile::FAST), f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole one-way path: payload → fountain frames → sound → noisy,
    /// interrupted recording → frames → payload.
    #[test]
    fn payload_over_a_bad_audio_channel() {
        let mut r = StdRng::seed_from_u64(8);
        let payload: Vec<u8> = (0..600).map(|_| r.gen()).collect();
        let mut tx = mesh_datagram::Sender::new(&payload, 64).unwrap();
        let k = tx.source_frames();
        let sent = tx.frames(k + k / 2 + 2);
        let mut audio = modulate(&sent, &Profile::FAST).unwrap();
        let per = audio.len() / sent.len();
        for s in &mut audio[per..3 * per] {
            *s = 0.0; // two frames lost entirely
        }
        let rx = channel(&audio, 10.0, 0.5, 9);
        let mut rcv = mesh_datagram::Receiver::default();
        let out = demodulate(&rx, &Profile::FAST).iter().find_map(|f| rcv.push(f));
        assert_eq!(out.unwrap(), payload);
    }
}
