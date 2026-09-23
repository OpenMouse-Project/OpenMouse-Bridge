//! The banner's chime: two soft bell tones, synthesized as a WAV so it needs
//! no bundled asset and the volume can be baked into the samples, which plays
//! the same through NSSound and PlaySound.

const SAMPLE_RATE: u32 = 44_100;
const LENGTH_SECONDS: f32 = 0.7;
/// Peak amplitude at full volume, leaving headroom for the overlapping notes.
const PEAK: f32 = 0.45;
/// (start in seconds, frequency in hertz): A5 then E6, a rising fifth.
const NOTES: [(f32, f32); 2] = [(0.0, 880.0), (0.11, 1318.51)];
const ATTACK_SECONDS: f32 = 0.004;
const DECAY_SECONDS: f32 = 0.16;

/// The chime at `volume` (0–100) as a 16-bit mono WAV file.
pub fn wav(volume: u8) -> Vec<u8> {
    // Loudness is perceived roughly logarithmically, so a squared curve makes
    // the slider feel even across its range.
    let gain = (f32::from(volume.min(100)) / 100.0).powi(2) * PEAK;
    let count = (SAMPLE_RATE as f32 * LENGTH_SECONDS) as u32;
    let samples = (0..count).map(|index| {
        let time = index as f32 / SAMPLE_RATE as f32;
        let value: f32 = NOTES
            .iter()
            .map(|&(start, frequency)| tone(time - start, frequency))
            .sum();
        (value * gain * f32::from(i16::MAX)) as i16
    });

    let data_len = count * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// One bell-like note `time` seconds after it starts: a sine with a quieter
/// octave overtone, a click-free attack and an exponential decay.
fn tone(time: f32, frequency: f32) -> f32 {
    if time < 0.0 {
        return 0.0;
    }
    let envelope = (time / ATTACK_SECONDS).min(1.0) * (-time / DECAY_SECONDS).exp();
    let phase = std::f32::consts::TAU * frequency * time;
    envelope * (phase.sin() + 0.25 * (phase * 2.0).sin()) / 1.25
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(wav: &[u8]) -> i16 {
        wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair).saturating_abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn writes_a_well_formed_wav() {
        let wav = wav(60);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + data_len);
    }

    #[test]
    fn volume_scales_the_chime_without_clipping() {
        assert_eq!(peak(&wav(0)), 0);
        assert!(peak(&wav(30)) < peak(&wav(60)));
        assert!(peak(&wav(100)) < i16::MAX);
    }
}
