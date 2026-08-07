//! Content-free audible feedback for microphone lifecycle transitions.

use std::sync::Arc;

use remtene_application::ports::{PortError, PortFuture, RecordingCue, RecordingCuePort};

/// Builds the platform cue adapter without exposing native sound APIs to
/// Application. Unsupported platforms fail best-effort through the Port.
#[must_use]
pub fn create_default_recording_cue() -> Arc<dyn RecordingCuePort> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(MacOsRecordingCue)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(UnsupportedRecordingCue)
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Default)]
struct MacOsRecordingCue;

#[cfg(target_os = "macos")]
impl RecordingCuePort for MacOsRecordingCue {
    fn play(&self, cue: RecordingCue) -> PortFuture<'_, Result<(), PortError>> {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("remtene-recording-cue".to_owned())
            .spawn(move || {
                let _ = sender.send(play_macos_cue(cue));
            });
        if spawned.is_err() {
            return Box::pin(async { Err(cue_error("recording_cue.thread_unavailable")) });
        }
        Box::pin(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(cue_error("recording_cue.playback_unavailable")))
        })
    }
}

#[cfg(target_os = "macos")]
fn play_macos_cue(cue: RecordingCue) -> Result<(), PortError> {
    use std::time::Duration;

    use objc2::{AnyThread, rc::autoreleasepool};
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;

    let wav = synthesize_cue(cue);
    autoreleasepool(|_| {
        let data = NSData::with_bytes(&wav);
        let sound = NSSound::initWithData(NSSound::alloc(), &data)
            .ok_or_else(|| cue_error("recording_cue.decode_failed"))?;
        if !sound.play() {
            return Err(cue_error("recording_cue.playback_unavailable"));
        }
        // Keep the NSSound object alive for the known synthesized duration.
        // `isPlaying` relies on an AppKit run loop and does not transition
        // reliably on this dedicated background thread.
        std::thread::sleep(Duration::from_millis(u64::from(
            NOTE_DURATION_MS * 2 + NOTE_GAP_MS + 25,
        )));
        let _ = sound.stop();
        Ok(())
    })
}

#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy, Debug, Default)]
struct UnsupportedRecordingCue;

#[cfg(not(target_os = "macos"))]
impl RecordingCuePort for UnsupportedRecordingCue {
    fn play(&self, _cue: RecordingCue) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Err(cue_error("recording_cue.unsupported")) })
    }
}

const CUE_SAMPLE_RATE_HZ: u32 = 44_100;
const NOTE_DURATION_MS: u32 = 50;
const NOTE_GAP_MS: u32 = 7;
const FADE_DURATION_MS: u32 = 5;
const CUE_AMPLITUDE: f32 = 0.18;

fn synthesize_cue(cue: RecordingCue) -> Vec<u8> {
    let frequencies = match cue {
        RecordingCue::Start => [659.25_f32, 880.0_f32],
        RecordingCue::Finish => [783.99_f32, 587.33_f32],
        RecordingCue::Cancel => [440.0_f32, 329.63_f32],
    };
    let note_frames = CUE_SAMPLE_RATE_HZ * NOTE_DURATION_MS / 1_000;
    let gap_frames = CUE_SAMPLE_RATE_HZ * NOTE_GAP_MS / 1_000;
    let fade_frames = (CUE_SAMPLE_RATE_HZ * FADE_DURATION_MS / 1_000).max(1);
    let total_frames = note_frames * 2 + gap_frames;
    let mut samples = Vec::with_capacity(total_frames as usize);

    for (note_index, frequency) in frequencies.into_iter().enumerate() {
        for frame in 0..note_frames {
            let attack = (frame as f32 / fade_frames as f32).min(1.0);
            let frames_left = note_frames.saturating_sub(frame + 1);
            let release = (frames_left as f32 / fade_frames as f32).min(1.0);
            let envelope = attack.min(release);
            let phase =
                std::f32::consts::TAU * frequency * frame as f32 / CUE_SAMPLE_RATE_HZ as f32;
            let sample = (phase.sin() * envelope * CUE_AMPLITUDE * f32::from(i16::MAX)) as i16;
            samples.push(sample);
        }
        if note_index == 0 {
            samples.extend(std::iter::repeat_n(0_i16, gap_frames as usize));
        }
    }

    pcm16_mono_wav(CUE_SAMPLE_RATE_HZ, &samples)
}

fn pcm16_mono_wav(sample_rate_hz: u32, samples: &[i16]) -> Vec<u8> {
    let data_bytes = u32::try_from(samples.len().saturating_mul(2)).unwrap_or(u32::MAX);
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&data_bytes.saturating_add(36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
    wav.extend_from_slice(&sample_rate_hz.saturating_mul(2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

fn cue_error(code: &str) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: "errors.recording_cue.unavailable".to_owned(),
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cue_is_a_short_two_note_pcm16_wav() {
        for cue in [
            RecordingCue::Start,
            RecordingCue::Finish,
            RecordingCue::Cancel,
        ] {
            let wav = synthesize_cue(cue);
            assert_eq!(&wav[0..4], b"RIFF");
            assert_eq!(&wav[8..12], b"WAVE");
            assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
            assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 44_100);
            assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
            let duration_ms = (wav.len() - 44) as u64 * 1_000 / 2 / 44_100;
            assert!((105..=110).contains(&duration_ms));
        }
    }

    #[test]
    fn cue_shapes_are_distinct() {
        assert_ne!(
            synthesize_cue(RecordingCue::Start),
            synthesize_cue(RecordingCue::Finish)
        );
        assert_ne!(
            synthesize_cue(RecordingCue::Finish),
            synthesize_cue(RecordingCue::Cancel)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires explicit REMTENE_RUN_LIVE_RECORDING_CUES=1 and produces audible output"]
    fn live_macos_plays_all_recording_cues() {
        assert_eq!(
            std::env::var("REMTENE_RUN_LIVE_RECORDING_CUES").as_deref(),
            Ok("1"),
            "set REMTENE_RUN_LIVE_RECORDING_CUES=1 before producing audible output"
        );
        for cue in [
            RecordingCue::Start,
            RecordingCue::Finish,
            RecordingCue::Cancel,
        ] {
            play_macos_cue(cue).expect("play recording cue");
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    }
}
