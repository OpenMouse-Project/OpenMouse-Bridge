//! The banner's sound. By default it is two soft bell tones, synthesized as a
//! WAV so it needs no bundled asset; or it is the WAV file the user names in
//! `notificationSoundPath`. Either one plays `notificationRepeat` times in a
//! row. The volume is baked into the samples, which plays the same through
//! NSSound and waveOut.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const SAMPLE_RATE: u32 = 44_100;
/// Length of one two-note chime, including its fading tail.
const LENGTH_SECONDS: f32 = 0.7;
/// Time from the start of one chime to the start of the next.
const REPEAT_INTERVAL_SECONDS: f32 = 0.7;
/// The most times a sound repeats. Keep in step with the limit in `config.rs`.
const MAX_REPEATS: u32 = 10;
/// Peak amplitude at full volume, leaving headroom for the overlapping notes.
const PEAK: f32 = 0.45;
/// (start in seconds, frequency in hertz): A5 then E6, a rising fifth.
const NOTES: [(f32, f32); 2] = [(0.0, 880.0), (0.11, 1318.51)];
const ATTACK_SECONDS: f32 = 0.004;
const DECAY_SECONDS: f32 = 0.16;

/// The most of a custom sound that plays per repeat; a longer file is cut off
/// with a short fade.
const CUSTOM_MAX_SECONDS: u64 = 10;
/// Silence between two repeats of a custom sound.
const CUSTOM_GAP_SECONDS: f32 = 0.3;
/// How long the fade at the cut-off point of a long custom sound lasts.
const CUSTOM_FADE_SECONDS: f32 = 0.05;

/// The built-in chime at `volume` (0–100), sounding `repeats` times in a row,
/// as a 16-bit mono WAV file.
pub fn wav(volume: u8, repeats: u32) -> Vec<u8> {
    let repeats = repeats.clamp(1, MAX_REPEATS);
    let gain = volume_gain(volume) * PEAK;
    let total_seconds = LENGTH_SECONDS + (repeats - 1) as f32 * REPEAT_INTERVAL_SECONDS;
    let count = (SAMPLE_RATE as f32 * total_seconds) as u32;
    let samples: Vec<i16> = (0..count)
        .map(|index| {
            let time = index as f32 / SAMPLE_RATE as f32;
            let value: f32 = (0..repeats)
                .flat_map(|repeat| {
                    let offset = repeat as f32 * REPEAT_INTERVAL_SECONDS;
                    NOTES
                        .iter()
                        .map(move |&(start, frequency)| tone(time - offset - start, frequency))
                })
                .sum();
            (value * gain * f32::from(i16::MAX)) as i16
        })
        .collect();
    write_wav(1, SAMPLE_RATE, &samples)
}

/// Loudness is perceived roughly logarithmically, so a squared curve makes the
/// slider feel even across its range.
fn volume_gain(volume: u8) -> f32 {
    (f32::from(volume.min(100)) / 100.0).powi(2)
}

/// Samples (interleaved when there are two channels) as a 16-bit PCM WAV file
/// with the canonical 44-byte header the players expect.
fn write_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
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

/// Where `notificationSoundPath` points: an absolute path as written, anything
/// else inside `config_dir`, the folder that holds `config.json`. Quotes pasted
/// around a path are ignored.
pub fn resolve_sound_path(value: &str, config_dir: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(value.trim().trim_matches('"'));
    match config_dir {
        Some(dir) if !path.is_absolute() => dir.join(path),
        _ => path,
    }
}

/// The WAV file at `path` at `volume` (0–100), sounding `repeats` times in a
/// row, as a 16-bit WAV file. At most the first ten seconds of the file play.
/// The error says in plain words why the file cannot be used.
pub fn custom_wav(path: &Path, volume: u8, repeats: u32) -> Result<Vec<u8>, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("{} cannot be opened: {error}", path.display()))?;
    let (encoding, start, data_len) = read_headers(&mut file)?;
    let frame_bytes = encoding.channels * encoding.bits / 8;
    let limit = CUSTOM_MAX_SECONDS * u64::from(encoding.sample_rate) * frame_bytes as u64;
    let truncated = data_len > limit;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    file.take(data_len.min(limit))
        .read_to_end(&mut raw)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    raw.truncate(raw.len() / frame_bytes * frame_bytes);

    let mut samples = decode(&raw, &encoding);
    if samples.is_empty() {
        return Err("the WAV file contains no audio".to_owned());
    }
    let gain = volume_gain(volume);
    for sample in &mut samples {
        *sample = (f32::from(*sample) * gain) as i16;
    }
    if truncated {
        fade_out(&mut samples, encoding.channels, encoding.sample_rate);
    }

    let repeats = repeats.clamp(1, MAX_REPEATS) as usize;
    let gap = (CUSTOM_GAP_SECONDS * encoding.sample_rate as f32) as usize * encoding.channels;
    let mut repeated = Vec::with_capacity((samples.len() + gap) * repeats);
    for repeat in 0..repeats {
        if repeat > 0 {
            repeated.resize(repeated.len() + gap, 0);
        }
        repeated.extend_from_slice(&samples);
    }
    Ok(write_wav(
        encoding.channels as u16,
        encoding.sample_rate,
        &repeated,
    ))
}

/// How the audio of a WAV file is stored.
#[derive(Clone, Copy)]
struct Encoding {
    float: bool,
    channels: usize,
    sample_rate: u32,
    bits: usize,
}

/// What a WAV file holds and where its audio sits: the encoding, then the byte
/// offset and the length of the `data` chunk. Chunks it does not need (a
/// `LIST` with the song title, say) are skipped.
fn read_headers(file: &mut (impl Read + Seek)) -> Result<(Encoding, u64, u64), String> {
    let length = file
        .seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut riff = [0u8; 12];
    if file.read_exact(&mut riff).is_err() || &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
        return Err("it is not a WAV file (only .wav files work, not .mp3)".to_owned());
    }

    let mut encoding = None;
    let mut position = 12u64;
    loop {
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return Err("the WAV file has no audio data".to_owned());
        }
        let size = u64::from(u32::from_le_bytes([
            header[4], header[5], header[6], header[7],
        ]));
        let body = position + 8;
        match &header[..4] {
            b"fmt " => {
                let take = size.min(40) as usize;
                let mut format = vec![0u8; take];
                if take < 16 || file.read_exact(&mut format).is_err() {
                    return Err("the format information of the WAV file is damaged".to_owned());
                }
                encoding = Some(parse_encoding(&format)?);
            }
            b"data" => {
                let Some(encoding) = encoding else {
                    return Err("the WAV file has no format information".to_owned());
                };
                return Ok((encoding, body, size.min(length.saturating_sub(body))));
            }
            _ => {}
        }
        // Chunks are padded to an even size.
        position = body + size + (size & 1);
        file.seek(SeekFrom::Start(position))
            .map_err(|error| error.to_string())?;
    }
}

/// The encoding in the body of a `fmt ` chunk (at least 16 bytes), if Bridge can
/// play it: 8, 16, 24 and 32-bit integers or 32-bit floats, mono or stereo.
fn parse_encoding(format: &[u8]) -> Result<Encoding, String> {
    let word = |at: usize| u16::from_le_bytes([format[at], format[at + 1]]);
    let mut tag = word(0);
    // WAVE_FORMAT_EXTENSIBLE keeps the real format in the first bytes of its
    // sub-format identifier.
    if tag == 0xFFFE && format.len() >= 26 {
        tag = word(24);
    }
    let channels = usize::from(word(2));
    let sample_rate = u32::from_le_bytes([format[4], format[5], format[6], format[7]]);
    let bits = usize::from(word(14));
    let float = match tag {
        1 => false,
        3 => true,
        other => {
            return Err(format!(
                "the WAV file uses an unsupported encoding ({other}); save it as a normal PCM .wav"
            ));
        }
    };
    let supported = if float {
        bits == 32
    } else {
        matches!(bits, 8 | 16 | 24 | 32)
    };
    if !supported {
        return Err(format!(
            "{bits}-bit audio is not supported; save it as a 16-bit .wav"
        ));
    }
    if !(1..=2).contains(&channels) {
        return Err(format!(
            "{channels} audio channels are not supported; use a mono or stereo .wav"
        ));
    }
    if !(4_000..=192_000).contains(&sample_rate) {
        return Err(format!(
            "a sample rate of {sample_rate} Hz is not supported"
        ));
    }
    Ok(Encoding {
        float,
        channels,
        sample_rate,
        bits,
    })
}

/// The raw samples as 16-bit integers, keeping the top 16 bits of deeper ones.
fn decode(raw: &[u8], encoding: &Encoding) -> Vec<i16> {
    match (encoding.float, encoding.bits) {
        (false, 8) => raw
            .iter()
            .map(|&byte| (i16::from(byte) - 128) << 8)
            .collect(),
        (false, 16) => raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&bytes| i16::from_le_bytes(bytes))
            .collect(),
        (false, 24) => raw
            .as_chunks::<3>()
            .0
            .iter()
            .map(|bytes| i16::from_le_bytes([bytes[1], bytes[2]]))
            .collect(),
        (false, 32) => raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| i16::from_le_bytes([bytes[2], bytes[3]]))
            .collect(),
        (true, 32) => raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&bytes| {
                let value = f32::from_le_bytes(bytes);
                (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Fades the last moments of `samples` out so a sound cut short does not click.
fn fade_out(samples: &mut [i16], channels: usize, sample_rate: u32) {
    let frames = samples.len() / channels;
    let fade = ((CUSTOM_FADE_SECONDS * sample_rate as f32) as usize).min(frames);
    for frame in frames - fade..frames {
        let factor = (frames - frame) as f32 / fade as f32;
        for sample in &mut samples[frame * channels..(frame + 1) * channels] {
            *sample = (f32::from(*sample) * factor) as i16;
        }
    }
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

    /// Loudest sample between `from` and `to` seconds.
    fn peak_between(wav: &[u8], from: f32, to: f32) -> i16 {
        let samples = wav[44..].as_chunks::<2>().0;
        let first = (from * SAMPLE_RATE as f32) as usize;
        let last = ((to * SAMPLE_RATE as f32) as usize).min(samples.len());
        samples[first..last]
            .iter()
            .map(|&pair| i16::from_le_bytes(pair).saturating_abs())
            .max()
            .unwrap_or(0)
    }

    fn samples_of(wav: &[u8]) -> Vec<i16> {
        wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect()
    }

    fn word(wav: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([wav[at], wav[at + 1]])
    }

    fn dword(wav: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([wav[at], wav[at + 1], wav[at + 2], wav[at + 3]])
    }

    /// A WAV file put together by hand, so these tests do not lean on the
    /// writer they check.
    fn riff(
        tag: u16,
        channels: u16,
        rate: u32,
        bits: u16,
        extra_chunk: bool,
        audio: &[u8],
    ) -> Vec<u8> {
        let mut body = b"WAVEfmt ".to_vec();
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&tag.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&rate.to_le_bytes());
        body.extend_from_slice(&(rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
        body.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        if extra_chunk {
            // An odd-sized chunk, padded to even, as players write song titles.
            body.extend_from_slice(b"LIST");
            body.extend_from_slice(&3u32.to_le_bytes());
            body.extend_from_slice(&[1, 2, 3, 0]);
        }
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(audio.len() as u32).to_le_bytes());
        body.extend_from_slice(audio);
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);
        file
    }

    fn pcm16(values: &[i16]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "openmouse-chime-test-{}-{name}.wav",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Plays `bytes` as a custom sound and cleans up after itself.
    fn custom(name: &str, bytes: &[u8], volume: u8, repeats: u32) -> Result<Vec<u8>, String> {
        let path = temp_file(name, bytes);
        let result = custom_wav(&path, volume, repeats);
        let _ = std::fs::remove_file(path);
        result
    }

    #[test]
    fn writes_a_well_formed_wav() {
        let wav = wav(60, 1);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + data_len);
    }

    #[test]
    fn volume_scales_the_chime_without_clipping() {
        assert_eq!(peak(&wav(0, 1)), 0);
        assert!(peak(&wav(30, 1)) < peak(&wav(60, 1)));
        assert!(peak(&wav(100, 1)) < i16::MAX);
    }

    #[test]
    fn the_chime_repeats_as_often_as_asked_with_a_quiet_gap_between() {
        for repeats in [1, 2, 3, 5] {
            let wav = wav(100, repeats);
            for repeat in 0..repeats {
                let start = repeat as f32 * REPEAT_INTERVAL_SECONDS;
                let loud = peak_between(&wav, start, start + 0.2);
                let tail = peak_between(
                    &wav,
                    start + REPEAT_INTERVAL_SECONDS - 0.05,
                    start + REPEAT_INTERVAL_SECONDS,
                );
                assert!(
                    loud > 4_000,
                    "chime {repeat} of {repeats} is too quiet: {loud}"
                );
                assert!(
                    tail < loud / 10,
                    "chime {repeat} of {repeats} does not fade: {tail}"
                );
            }
        }
        assert!(wav(60, 3).len() > wav(60, 2).len());
        assert!(wav(60, 2).len() > wav(60, 1).len());
    }

    #[test]
    fn the_repeat_count_stays_between_one_and_ten() {
        assert_eq!(wav(60, 0), wav(60, 1));
        assert_eq!(wav(60, 99), wav(60, 10));
        assert_ne!(wav(60, 9), wav(60, 10));
    }

    #[test]
    fn a_custom_file_plays_at_the_chosen_volume_and_repeats() {
        let audio = pcm16(&[10_000; 1_000]);
        let file = riff(1, 1, 22_050, 16, false, &audio);

        let wav = custom("volume", &file, 100, 3).unwrap();
        let gap = (CUSTOM_GAP_SECONDS * 22_050.0) as usize;
        assert_eq!(word(&wav, 22), 1, "mono");
        assert_eq!(dword(&wav, 24), 22_050, "sample rate");
        assert_eq!(wav.len(), 44 + 2 * (3 * 1_000 + 2 * gap));
        let samples = samples_of(&wav);
        assert_eq!(samples[0], 10_000);
        assert_eq!(samples[1_000], 0, "silence between repeats");
        assert_eq!(samples[1_000 + gap], 10_000, "second repeat");

        // 50% volume is a quarter of the amplitude.
        let quiet = custom("volume-half", &file, 50, 1).unwrap();
        assert_eq!(samples_of(&quiet)[0], 2_500);
        assert_eq!(peak(&custom("volume-zero", &file, 0, 1).unwrap()), 0);
    }

    #[test]
    fn a_custom_file_is_repeated_at_most_ten_times() {
        let file = riff(1, 1, 8_000, 16, false, &pcm16(&[1_000; 100]));
        assert_eq!(
            custom("repeat-99", &file, 100, 99).unwrap(),
            custom("repeat-10", &file, 100, 10).unwrap()
        );
        assert_eq!(
            custom("repeat-0", &file, 100, 0).unwrap(),
            custom("repeat-1", &file, 100, 1).unwrap()
        );
    }

    #[test]
    fn stereo_files_with_extra_chunks_keep_both_channels() {
        let audio = pcm16(&[100, -100, 200, -200]);
        let file = riff(1, 2, 8_000, 16, true, &audio);
        let wav = custom("stereo", &file, 100, 1).unwrap();
        assert_eq!(word(&wav, 22), 2, "stereo");
        assert_eq!(samples_of(&wav), [100, -100, 200, -200]);
    }

    #[test]
    fn other_sample_formats_are_converted_to_16_bit() {
        let eight = riff(1, 1, 8_000, 8, false, &[128, 255, 0]);
        assert_eq!(
            samples_of(&custom("bits8", &eight, 100, 1).unwrap()),
            [0, 127 << 8, -128 << 8]
        );
        let twenty_four = riff(1, 1, 8_000, 24, false, &[0xAA, 0x34, 0x12]);
        assert_eq!(
            samples_of(&custom("bits24", &twenty_four, 100, 1).unwrap()),
            [0x1234]
        );
        let thirty_two = riff(1, 1, 8_000, 32, false, &[0xAA, 0xBB, 0x34, 0x12]);
        assert_eq!(
            samples_of(&custom("bits32", &thirty_two, 100, 1).unwrap()),
            [0x1234]
        );
        let floats: Vec<u8> = [0.5f32, -1.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let float = riff(3, 1, 8_000, 32, false, &floats);
        assert_eq!(
            samples_of(&custom("float", &float, 100, 1).unwrap()),
            [16_383, -32_767]
        );
    }

    #[test]
    fn a_long_file_is_cut_at_ten_seconds_with_a_fade() {
        let file = riff(1, 1, 8_000, 16, false, &pcm16(&vec![20_000; 12 * 8_000]));
        let samples = samples_of(&custom("long", &file, 100, 1).unwrap());
        assert_eq!(samples.len(), 10 * 8_000);
        assert_eq!(samples[samples.len() - 1_000], 20_000);
        assert!(samples[samples.len() - 1].abs() < 100, "fades to silence");
    }

    #[test]
    fn a_file_whose_header_promises_more_than_it_holds_still_plays() {
        let mut file = riff(1, 1, 8_000, 16, false, &pcm16(&[500; 100]));
        // Claim a bigger `data` chunk than the file has, as a cut-off download would.
        let at = file.len() - 200 - 4;
        file[at..at + 4].copy_from_slice(&1_000_000u32.to_le_bytes());
        let samples = samples_of(&custom("short", &file, 100, 1).unwrap());
        assert_eq!(samples, [500; 100]);
    }

    #[test]
    fn files_that_cannot_be_played_say_why() {
        let mp3 = custom("mp3", b"ID3\x04\0\0\0\0\0\0 not a wav", 100, 1).unwrap_err();
        assert!(mp3.contains("not a WAV file"), "{mp3}");

        let adpcm = riff(2, 1, 8_000, 4, false, &[0; 4]);
        let error = custom("adpcm", &adpcm, 100, 1).unwrap_err();
        assert!(error.contains("unsupported encoding"), "{error}");

        let surround = riff(1, 6, 48_000, 16, false, &[0; 12]);
        let error = custom("surround", &surround, 100, 1).unwrap_err();
        assert!(error.contains("6 audio channels"), "{error}");

        let empty = riff(1, 1, 8_000, 16, false, &[]);
        let error = custom("empty", &empty, 100, 1).unwrap_err();
        assert!(error.contains("no audio"), "{error}");

        let missing = std::env::temp_dir().join("openmouse-chime-no-such-file.wav");
        let error = custom_wav(&missing, 100, 1).unwrap_err();
        assert!(error.contains("cannot be opened"), "{error}");
    }

    #[test]
    fn sound_paths_resolve_against_the_config_folder() {
        let dir = std::env::temp_dir().join("openmouse-config");
        let absolute = std::env::temp_dir().join("music").join("ding.wav");
        let absolute_text = absolute.to_str().unwrap();

        assert_eq!(resolve_sound_path(absolute_text, Some(&dir)), absolute);
        assert_eq!(
            resolve_sound_path("ding.wav", Some(&dir)),
            dir.join("ding.wav")
        );
        assert_eq!(
            resolve_sound_path("sounds/ding.wav", Some(&dir)),
            dir.join("sounds/ding.wav")
        );
        assert_eq!(
            resolve_sound_path("ding.wav", None),
            PathBuf::from("ding.wav")
        );
        let quoted = format!("  \"{absolute_text}\"  ");
        assert_eq!(resolve_sound_path(&quoted, Some(&dir)), absolute);
    }
}
