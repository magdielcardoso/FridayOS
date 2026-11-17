//! Friday's ears: microphone capture, VAD, speech playback and speaker checks.
//!
//! Everything here is deliberately small and local: cpal for capture, a naive
//! RMS voice-activity detector tuned to the MeuZe study thresholds, `afplay`
//! and `say` for playback, and one tiny Swift helper (`swift/speech.swift`)
//! for on-device recognition used by the wake-word loop.
//!
//! Caveats that the app must handle:
//! - macOS TCC needs `NSMicrophoneUsageDescription` (and
//!   `NSSpeechRecognitionUsageDescription` for the Swift helper) in the host
//!   app's Info.plist. Without it capture fails or the process is killed.
//! - Devices usually do not run at 16 kHz. We resample naive-linearly to
//!   16 kHz mono, which is enough for STT but not studio quality.
//! - The speaker profile is a rough spectral heuristic, **not** biometric
//!   security. Do not use it to authorize sensitive actions.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Data, SampleFormat, StreamConfig};
use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Noise-relative gates replace hard-coded levels so the VAD works whether
/// the input volume is 20 or 100. These are only fallbacks/caps.
pub const SPEECH_THRESHOLD: f32 = 0.012;
/// RMS above this while Friday is speaking triggers barge-in.
pub const BARGE_THRESHOLD: f32 = 0.10;
/// Set when audible playback actually starts. The capture loop consumes it to
/// restart the echo grace period there — not at first token, when synthesis
/// still needs ~1 s and the grace would already be half spent.
static ECHO_RESET: AtomicBool = AtomicBool::new(false);

/// Restarts the echo grace period. Call it when the first audible sample is
/// about to play, so Friday's own voice through the speakers is not mistaken
/// for a barge-in that kills the start of her sentence.
pub fn mark_audible_playback() {
    ECHO_RESET.store(true, Ordering::Relaxed);
}
/// Silence that ends a short command (~0.75 s).
pub const SHORT_HANGOVER_FRAMES: u32 = 3;
/// Silence that ends a long thought (~1.5 s), where natural pauses are common.
pub const LONG_HANGOVER_FRAMES: u32 = 6;
/// Utterances longer than this get the long hangover (~2 s of speech).
pub const LONG_UTTERANCE_FRAMES: usize = 8;
/// Kept for compatibility with callers that referenced the old constant.
pub const SILENCE_FRAMES_TO_END: u32 = SHORT_HANGOVER_FRAMES;
/// Hard cap on an utterance (~8 s). Longer clips make the on-device
/// recognizer silently return no text and were almost always speaker echo.
pub const MAX_FRAMES: usize = 32;
/// Samples per VAD frame.
pub const FRAME_SIZE: usize = 4096;
/// Rate we hand to the STT layer.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// How long `voice_listen_once` will record from one utterance.
const MAX_UTTERANCE_MS: u64 = 20_000;

// ---------------------------------------------------------------------------
// Public event API
// ---------------------------------------------------------------------------

/// Events produced by the background capture thread.
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// Per-frame RMS in 0.0..~1.0. Useful for meters and interruption UX.
    Level(f32),
    /// Speech crossed `SPEECH_THRESHOLD`.
    SpeechStart,
    /// Utterance finished (silence or frame cap). `captured` is 16 kHz mono.
    SpeechEnd {
        captured: Vec<f32>,
        sample_rate: u32,
    },
    /// User started talking while Friday was speaking (see `set_speaking`).
    BargeIn,
    /// Capture failed; the thread stops after this.
    Error(String),
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// A minimal mono capture engine for the default input device.
pub struct VoiceEngine {
    device: cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    device_sample_rate: u32,
}

impl VoiceEngine {
    /// Opens the default input device. Never panics; cpal errors become `Err`.
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "nenhum microfone de entrada encontrado".to_string())?;
        let supported = device
            .default_input_config()
            .map_err(|e| format!("configuração de entrada indisponível: {e}"))?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.config();
        if config.channels == 0 {
            return Err("o microfone padrão reportou zero canais".to_string());
        }
        if config.sample_rate.0 == 0 {
            return Err("o microfone padrão reportou taxa de amostragem zero".to_string());
        }
        let device_sample_rate = config.sample_rate.0;
        Ok(Self {
            device,
            config,
            sample_format,
            device_sample_rate,
        })
    }

    pub fn device_name(&self) -> String {
        self.device
            .name()
            .unwrap_or_else(|_| "microfone padrão".to_string())
    }

    pub fn sample_rate(&self) -> u32 {
        self.device_sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.config.channels
    }

    /// Spawns the capture thread. Returns a handle the app must keep (drop or
    /// `stop()` ends the thread).
    pub fn spawn(
        self,
        tx: Sender<VoiceEvent>,
        running: Arc<AtomicBool>,
        speaking: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>, String> {
        thread::Builder::new()
            .name("friday-voice".to_string())
            .spawn(move || self.capture_loop(tx, running, speaking))
            .map_err(|e| format!("não foi possível iniciar a thread de voz: {e}"))
    }

    fn capture_loop(
        self,
        tx: Sender<VoiceEvent>,
        running: Arc<AtomicBool>,
        speaking: Arc<AtomicBool>,
    ) {
        let device_rate = self.device_sample_rate;
        let channels = self.config.channels as usize;
        let sample_format = self.sample_format;
        let mut resampler = LinearResampler::new(device_rate, TARGET_SAMPLE_RATE);
        let mut agc = Agc::new();
        let mut vad = VadState::new();

        let callback_tx = tx.clone();
        let error_tx = tx.clone();
        let callback_running = running.clone();
        let callback_speaking = speaking.clone();
        let error_sent = Arc::new(AtomicBool::new(false));
        let error_sent_cb = error_sent.clone();

        let stream = self.device.build_input_stream_raw(
            &self.config,
            sample_format,
            move |data: &Data, _info| {
                if !callback_running.load(Ordering::Relaxed) {
                    return;
                }
                let mono = to_mono(data, sample_format, channels);
                if mono.is_empty() {
                    return;
                }
                let capacity = (mono.len() as u64 * TARGET_SAMPLE_RATE as u64
                    / device_rate.max(1) as u64
                    + 8) as usize;
                let mut resampled = Vec::with_capacity(capacity);
                resampler.process(&mono, &mut resampled);
                agc.apply(&mut resampled);
                vad.push(
                    &resampled,
                    callback_speaking.load(Ordering::Relaxed),
                    &callback_tx,
                );
            },
            move |err| {
                if !error_sent_cb.swap(true, Ordering::Relaxed) {
                    let _ = error_tx.send(VoiceEvent::Error(format!("stream de áudio: {err}")));
                }
            },
            None,
        );

        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                let _ = tx.send(VoiceEvent::Error(format!(
                    "não foi possível abrir o microfone: {e}"
                )));
                return;
            }
        };

        if let Err(e) = stream.play() {
            let _ = tx.send(VoiceEvent::Error(format!(
                "não foi possível iniciar a captura: {e}"
            )));
            return;
        }

        while running.load(Ordering::Relaxed) {
            thread::park_timeout(Duration::from_millis(25));
        }
        drop(stream);
    }
}

/// Starts capture through AVAudioEngine with voice processing (AEC) enabled.
/// Friday's playback goes through the same engine, so her own voice is
/// subtracted from the microphone before the VAD sees it.
pub fn start_aec() -> Result<VoiceHandle, String> {
    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let speaking = Arc::new(AtomicBool::new(false));
    let running_thread = Arc::clone(&running);
    let speaking_thread = Arc::clone(&speaking);
    let raw = std::env::temp_dir().join(format!(
        "friday-aec-{}-{}.raw",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    crate::voice_engine::start_stream(&raw)?;
    let join = thread::Builder::new()
        .name("friday-voice-aec".into())
        .spawn(move || {
            let mut vad = VadState::new();
            let mut agc = Agc::new();
            let mut offset = 0u64;
            while running_thread.load(Ordering::Relaxed) {
                let Ok(meta) = std::fs::metadata(&raw) else { break };
                let len = meta.len();
                if len > offset {
                    if let Ok(mut file) = std::fs::File::open(&raw) {
                        use std::io::{Read, Seek, SeekFrom};
                        if file.seek(SeekFrom::Start(offset)).is_ok() {
                            let mut bytes = Vec::new();
                            if file.read_to_end(&mut bytes).is_ok() {
                                offset = len;
                                let usable = (bytes.len() / 2) * 2;
                                let samples: Vec<f32> = bytes[..usable]
                                    .chunks_exact(2)
                                    .map(|pair| {
                                        i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32_768.0
                                    })
                                    .collect();
                                let mut resampled = samples;
                                agc.apply(&mut resampled);
                                vad.push(
                                    &resampled,
                                    speaking_thread.load(Ordering::Relaxed),
                                    &tx,
                                );
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = crate::voice_engine::stop_stream();
            let _ = std::fs::remove_file(&raw);
        })
        .map_err(|error| format!("aec voice thread: {error}"))?;
    Ok(VoiceHandle {
        events: Some(rx),
        running,
        speaking,
        join: Some(join),
    })
}

/// Starts capture on the default input device (fallback without AEC).
pub fn start() -> Result<VoiceHandle, String> {
    let engine = VoiceEngine::new()?;
    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let speaking = Arc::new(AtomicBool::new(false));
    let join = engine.spawn(tx, running.clone(), speaking.clone())?;
    Ok(VoiceHandle {
        events: Some(rx),
        running,
        speaking,
        join: Some(join),
    })
}

/// Handle to the running capture thread.
pub struct VoiceHandle {
    events: Option<Receiver<VoiceEvent>>,
    running: Arc<AtomicBool>,
    speaking: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl VoiceHandle {
    /// Takes the event receiver. Call this once; the app can then loop over
    /// `recv()` / `recv_timeout()` while retaining the handle for `stop()`.
    pub fn recv_events(&mut self) -> Receiver<VoiceEvent> {
        match self.events.take() {
            Some(rx) => rx,
            None => {
                // Second call: return a disconnected receiver instead of panicking.
                let (_tx, rx) = mpsc::channel();
                rx
            }
        }
    }

    /// Waits for the next event while the receiver is still owned by the handle.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<VoiceEvent, RecvTimeoutError> {
        match self.events.as_ref() {
            Some(rx) => rx.recv_timeout(timeout),
            None => Err(RecvTimeoutError::Disconnected),
        }
    }

    /// Non-blocking poll while the receiver is still owned by the handle.
    pub fn try_recv(&mut self) -> Result<VoiceEvent, TryRecvError> {
        match self.events.as_ref() {
            Some(rx) => rx.try_recv(),
            None => Err(TryRecvError::Disconnected),
        }
    }

    /// Tell the VAD that Friday is currently speaking, enabling `BargeIn`.
    pub fn set_speaking(&self, speaking: bool) {
        self.speaking.store(speaking, Ordering::Relaxed);
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking.load(Ordering::Relaxed)
    }

    /// Stops capture and joins the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for VoiceHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Drops leading and trailing frames below `threshold`, keeping a 120 ms
/// margin on each side so consonants are not clipped. Trimming before STT is
/// what turns a 30 s noisy capture into a usable clip.
fn trim_edges(samples: &[f32], _gate: f32) -> Vec<f32> {
    let frame = FRAME_SIZE / 4;
    if samples.len() <= frame * 2 {
        return samples.to_vec();
    }
    // Relative to the clip's own peak: a quiet speaker must not be trimmed to
    // nothing, and a loud one still loses the room tone around the words.
    let peak = samples
        .iter()
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()));
    let threshold = (peak * 0.06).max(0.004);
    let energy = |chunk: &[f32]| {
        (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len().max(1) as f32).sqrt()
    };
    let mut first = 0usize;
    let mut last = samples.len();
    for (index, chunk) in samples.chunks(frame).enumerate() {
        if energy(chunk) >= threshold {
            first = index * frame;
            break;
        }
    }
    for (index, chunk) in samples.chunks(frame).enumerate().rev() {
        if energy(chunk) >= threshold {
            last = ((index + 1) * frame).min(samples.len());
            break;
        }
    }
    let margin = (TARGET_SAMPLE_RATE as usize * 120 / 1_000).min(first);
    let start = first.saturating_sub(margin);
    let end = (last + margin).min(samples.len());
    if end <= start {
        return samples.to_vec();
    }
    samples[start..end].to_vec()
}

/// Automatic gain control. The mac mini's input volume can be anywhere from
/// 20 to 100; without this, quiet speech (~0.06 RMS) was below the STT's usable
/// range and came back as an empty transcript.
struct Agc {
    gain: f32,
    peak: f32,
    floor: f32,
}

impl Agc {
    fn new() -> Self {
        Self {
            gain: 1.0,
            peak: 0.05,
            floor: 0.01,
        }
    }

    fn apply(&mut self, samples: &mut [f32]) {
        if samples.is_empty() {
            return;
        }
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        // Slowly decaying peak (voice level) and noise floor (silence level).
        self.peak = rms.max(self.peak * 0.99);
        if rms < self.floor {
            self.floor = rms;
        } else {
            self.floor += (rms - self.floor) * 0.005;
        }
        // No noise-driven gain reduction: user asked for the raw, boosted mic.
        // AEC already removes playback echo; the VAD uses a low absolute gate.
        let target = (0.45 / self.peak.max(0.001)).clamp(1.0, 16.0);
        self.gain += (target - self.gain) * 0.18;
        if (self.gain - 1.0).abs() > 0.01 {
            for sample in samples.iter_mut() {
                *sample = (*sample * self.gain).clamp(-1.0, 1.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VAD + resampling
// ---------------------------------------------------------------------------

struct VadState {
    frame: Vec<f32>,
    captured: Vec<f32>,
    in_speech: bool,
    silence_frames: u32,
    frames_captured: usize,
    barge_sent: bool,
    /// Running ambient noise estimate, so a noisy room does not look like speech.
    noise: f32,
    /// Frames used to learn the room before we accept any speech.
    calibrated: u32,
    /// Frames elapsed since Friday started speaking (echo guard).
    speaking_frames: u32,
    /// Set once a barge-in opened a capture while Friday was speaking.
    barge_capturing: bool,
    /// Consecutive voiced frames, so a single click does not open an utterance.
    voiced_run: u32,
    /// Consecutive loud frames while Friday speaks, so one echo transient
    /// cannot kill her sentence; a real interruption sustains.
    barge_run: u32,
    /// Loudest mic level observed while her own audio plays. Without AEC this
    /// is the best estimate of her echo, and an interruption must beat it.
    echo_ref: f32,
    /// Frames kept before speech starts, so the first syllable is not clipped.
    pre_roll: std::collections::VecDeque<Vec<f32>>,
    /// Fixed hangover override (`FRIDAY_VAD_HANGOVER_MS`) for tuning without a rebuild.
    hangover_override: Option<u32>,
}

impl VadState {
    fn new() -> Self {
        Self {
            frame: Vec::with_capacity(FRAME_SIZE),
            captured: Vec::new(),
            in_speech: false,
            silence_frames: 0,
            frames_captured: 0,
            barge_sent: false,
            noise: 0.02,
            calibrated: 0,
            speaking_frames: 0,
            barge_capturing: false,
            voiced_run: 0,
            barge_run: 0,
            echo_ref: 0.0,
            pre_roll: std::collections::VecDeque::with_capacity(3),
            hangover_override: std::env::var("FRIDAY_VAD_HANGOVER_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|ms| ((ms as f32 / 256.0).ceil() as u32).clamp(1, 10)),
        }
    }

    fn push(&mut self, samples: &[f32], speaking: bool, tx: &Sender<VoiceEvent>) {
        self.frame.extend_from_slice(samples);
        while self.frame.len() >= FRAME_SIZE {
            let frame: Vec<f32> = self.frame.drain(..FRAME_SIZE).collect();
            self.process(&frame, speaking, tx);
        }
    }

    fn process(&mut self, frame: &[f32], speaking: bool, tx: &Sender<VoiceEvent>) {
        let rms = rms(frame);
        let _ = tx.send(VoiceEvent::Level(rms));

        // Learn the room for ~1.5 s before trusting the gate: mics differ a lot
        // and a noisy fan would otherwise look like non-stop speech.
        // Absolute, low gate. No room-noise estimate: it was raising the
        // threshold exactly when the user spoke quietly.
        let gate = SPEECH_THRESHOLD;

        // While Friday speaks, the microphone also hears her. Never treat that
        // as an utterance: only a much louder voice after a short grace period
        // counts as barge-in, so she never transcribes her own tail.
        if speaking && !self.barge_capturing {
            // Audible playback just (re)started: the echo grace restarts here,
            // where the speakers actually make sound — not at first token.
            if ECHO_RESET.swap(false, Ordering::Relaxed) {
                self.speaking_frames = 0;
                self.barge_sent = false;
                self.barge_run = 0;
                self.echo_ref = 0.0;
            }
            self.speaking_frames += 1;
            // Learn her own echo level from the mic; it decays slowly so a
            // quieter passage does not lower the bar for a false barge-in.
            self.echo_ref = (self.echo_ref * 0.995).max(rms);
            self.in_speech = false;
            self.silence_frames = 0;
            self.frames_captured = 0;
            // Her own voice through the speakers sets the reference. A real
            // interruption has to be clearly louder than the echo we measured.
            let echo_bar = (self.echo_ref * 1.3).min(0.95);
            let barge_gate = BARGE_THRESHOLD.max(echo_bar);
            if rms >= barge_gate {
                self.barge_run += 1;
            } else {
                self.barge_run = 0;
            }
            // ~2 s grace from the first audible sample, then two loud frames in
            // a row (~0.5 s) so a transient cannot cut her sentence.
            if self.speaking_frames > 8 && self.barge_run >= 2 {
                if !self.barge_sent {
                    self.barge_sent = true;
                    // Start capturing right away: the words that interrupted
                    // Friday become the guidance for the running turn.
                    self.barge_capturing = true;
                    self.in_speech = true;
                    self.silence_frames = 0;
                    self.frames_captured = self.pre_roll.len();
                    self.captured.clear();
                    for pending in self.pre_roll.drain(..) {
                        self.captured.extend_from_slice(&pending);
                    }
                    let _ = tx.send(VoiceEvent::BargeIn);
                    let _ = tx.send(VoiceEvent::SpeechStart);
                }
            } else if rms < barge_gate {
                self.barge_sent = false;
            }
            return;
        }
        if !speaking {
            self.speaking_frames = 0;
        }

        // Hysteresis: while already in a sentence, quieter trailing syllables
        // still count as speech. The sustain threshold must stay above the room
        // noise, otherwise a loud room (mic at 100%) keeps the utterance open
        // until the 30 s cap and sends noise to STT.
        let sustain = (gate * 0.70).min(0.45);
        let voiced = rms >= if self.in_speech { sustain } else { gate };

        self.pre_roll.push_back(frame.to_vec());
        while self.pre_roll.len() > 3 {
            self.pre_roll.pop_front();
        }

        if !self.in_speech {
            // Two voiced frames in a row (~0.5 s) before we believe it is speech:
            // fast enough for a natural reply, long enough to reject a click.
            if voiced {
                self.voiced_run += 1;
            } else {
                self.voiced_run = 0;
            }
            // One frame (~0.25 s) is enough for quiet speech; a click does not
            // survive `trim_edges` and the minimum length check below.
            if self.voiced_run >= 1 {
                self.in_speech = true;
                self.silence_frames = 0;
                self.frames_captured = self.pre_roll.len();
                self.captured.clear();
                for pending in self.pre_roll.drain(..) {
                    self.captured.extend_from_slice(&pending);
                }
                let _ = tx.send(VoiceEvent::SpeechStart);
            }
            return;
        }

        self.captured.extend_from_slice(frame);
        self.frames_captured += 1;
        if voiced {
            self.silence_frames = 0;
        } else {
            self.silence_frames += 1;
        }

        // Short commands close quickly; longer thoughts get room for a natural
        // pause before the endpoint. A fixed value can be forced with
        // FRIDAY_VAD_HANGOVER_MS (e.g. 1200).
        let hangover = self.hangover_override.unwrap_or_else(|| {
            if self.frames_captured >= LONG_UTTERANCE_FRAMES {
                LONG_HANGOVER_FRAMES
            } else {
                SHORT_HANGOVER_FRAMES
            }
        });
        if self.silence_frames >= hangover || self.frames_captured >= MAX_FRAMES {
            let captured = std::mem::take(&mut self.captured);
            let frames = self.frames_captured;
            self.in_speech = false;
            self.silence_frames = 0;
            self.frames_captured = 0;
            self.voiced_run = 0;
            self.barge_sent = false;
            self.barge_capturing = false;
            // Utterances shorter than ~0.75 s are clicks, not speech.
            if frames >= 3 {
                let trimmed = trim_edges(&captured, sustain.max(gate));
                // Never ship an empty clip: if trimming ate everything, keep the
                // original capture and let STT decide.
                let captured = if trimmed.len() >= FRAME_SIZE / 2 {
                    trimmed
                } else {
                    captured
                };
                if captured.len() >= FRAME_SIZE / 2 {
                    let _ = tx.send(VoiceEvent::SpeechEnd {
                        captured,
                        sample_rate: TARGET_SAMPLE_RATE,
                    });
                }
            }
        }
    }
}

fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    (sum / frame.len() as f32).sqrt()
}

/// Naive linear resampler, good enough to feed an STT model.
struct LinearResampler {
    ratio: f64,
    phase: f64,
    prev: f32,
    started: bool,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        let dst = dst_rate.max(1);
        let ratio = (src_rate.max(1) as f64 / dst as f64).max(0.01);
        Self {
            ratio,
            phase: 0.0,
            prev: 0.0,
            started: false,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.ratio - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        for &cur in input {
            if !self.started {
                self.prev = cur;
                self.started = true;
            }
            while self.phase < 1.0 {
                let value = self.prev + (cur - self.prev) * self.phase as f32;
                out.push(value);
                self.phase += self.ratio;
            }
            self.phase -= 1.0;
            self.prev = cur;
        }
    }
}

/// Downmixes interleaved device data to mono f32.
fn to_mono(data: &Data, format: SampleFormat, channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    let mut interleaved: Vec<f32> = Vec::new();

    macro_rules! convert {
        ($ty:ty, $conv:expr) => {
            match data.as_slice::<$ty>() {
                Some(samples) => {
                    interleaved.reserve(samples.len());
                    for &sample in samples {
                        interleaved.push($conv(sample));
                    }
                }
                None => return Vec::new(),
            }
        };
    }

    match format {
        SampleFormat::F32 => convert!(f32, |v: f32| v),
        SampleFormat::F64 => convert!(f64, |v: f64| v as f32),
        SampleFormat::I8 => convert!(i8, |v: i8| v as f32 / 128.0),
        SampleFormat::I16 => convert!(i16, |v: i16| v as f32 / 32_768.0),
        SampleFormat::I32 => convert!(i32, |v: i32| v as f32 / 2_147_483_648.0),
        SampleFormat::I64 => convert!(i64, |v: i64| v as f32 / 9_223_372_036_854_775_808.0),
        SampleFormat::U8 => convert!(u8, |v: u8| (v as f32 - 128.0) / 128.0),
        SampleFormat::U16 => convert!(u16, |v: u16| (v as f32 - 32_768.0) / 32_768.0),
        SampleFormat::U32 => convert!(u32, |v: u32| {
            (v as f64 - 2_147_483_648.0) as f32 / 2_147_483_648.0
        }),
        SampleFormat::U64 => convert!(u64, |v: u64| {
            (v as f64 - 9_223_372_036_854_775_808.0) as f32 / 9_223_372_036_854_775_808.0
        }),
        _ => return Vec::new(),
    }

    if interleaved.is_empty() {
        return Vec::new();
    }
    if channels == 1 {
        return interleaved;
    }

    let frames = interleaved.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for frame in interleaved.chunks_exact(channels) {
        let sum: f32 = frame.iter().copied().sum();
        mono.push(sum / channels as f32);
    }
    mono
}

// ---------------------------------------------------------------------------
// WAV helpers (no hound)
// ---------------------------------------------------------------------------

fn voice_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Caches/FridayOS")
        .join("voice")
}

fn wav_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    voice_dir().join(format!("utterance-{stamp}.wav"))
}

/// Writes 16-bit PCM mono. Clamps samples to [-1, 1].
pub fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("não foi possível criar {}: {e}", parent.display()))?;
    }
    let data_len = samples.len().saturating_mul(2);
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&sample_rate.saturating_mul(2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, out)
        .map_err(|e| format!("não foi possível escrever {}: {e}", path.display()))
}

/// Reads PCM (8/16/24/32-bit) or float32 WAVs, downmixing to mono.
fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32), String> {
    // Refuse huge files: a model-supplied path used to be read whole and
    // expanded to f32, which could exhaust memory.
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    let size = std::fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|e| format!("não foi possível abrir {}: {e}", path.display()))?;
    if size > MAX_BYTES {
        return Err(format!(
            "{} é grande demais ({size} bytes; limite 64 MB)",
            path.display()
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("não foi possível ler {}: {e}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{} não é um WAV válido", path.display()));
    }

    let mut pos = 12usize;
    let mut format: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut data_range: Option<(usize, usize)> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = pos + 8;

        if id == b"fmt " {
            if size < 16 || body + 16 > bytes.len() {
                return Err("bloco fmt inválido".to_string());
            }
            let tag = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            let rate = u32::from_le_bytes([
                bytes[body + 4],
                bytes[body + 5],
                bytes[body + 6],
                bytes[body + 7],
            ]);
            let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            format = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            let len = size.min(bytes.len().saturating_sub(body));
            data_range = Some((body, len));
        }

        let step = size.saturating_add(size & 1);
        pos = match body.checked_add(step) {
            Some(next) if next > pos => next,
            _ => break,
        };
    }

    let (tag, channels, rate, bits) =
        format.ok_or_else(|| "bloco fmt ausente no WAV".to_string())?;
    let (start, len) = data_range.ok_or_else(|| "bloco data ausente no WAV".to_string())?;
    if rate == 0 || channels == 0 {
        return Err("WAV com taxa ou canais inválidos".to_string());
    }
    let data = &bytes[start..start + len];
    let mut interleaved: Vec<f32> = Vec::new();

    match (tag, bits) {
        (1, 8) => {
            interleaved.reserve(data.len());
            for &value in data {
                interleaved.push((value as f32 - 128.0) / 128.0);
            }
        }
        (1, 16) => {
            interleaved.reserve(data.len() / 2);
            for chunk in data.chunks_exact(2) {
                let value = i16::from_le_bytes([chunk[0], chunk[1]]);
                interleaved.push(value as f32 / 32_768.0);
            }
        }
        (1, 24) => {
            interleaved.reserve(data.len() / 3);
            for chunk in data.chunks_exact(3) {
                let raw = ((chunk[2] as i32) << 16) | ((chunk[1] as i32) << 8) | (chunk[0] as i32);
                let signed = if raw & 0x0080_0000 != 0 {
                    raw | !0x00FF_FFFF
                } else {
                    raw
                };
                interleaved.push(signed as f32 / 8_388_608.0);
            }
        }
        (1, 32) => {
            interleaved.reserve(data.len() / 4);
            for chunk in data.chunks_exact(4) {
                let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                interleaved.push(value as f32 / 2_147_483_648.0);
            }
        }
        (3, 32) => {
            interleaved.reserve(data.len() / 4);
            for chunk in data.chunks_exact(4) {
                let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                interleaved.push(value);
            }
        }
        _ => {
            return Err(format!(
                "formato WAV não suportado (tag {tag}, {bits} bits); use PCM 16-bit"
            ))
        }
    }

    if interleaved.is_empty() {
        return Err("WAV sem amostras".to_string());
    }

    let channels = channels as usize;
    let mono = if channels == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
            .collect()
    };
    Ok((mono, rate))
}

// ---------------------------------------------------------------------------
// Speaker profile (crude heuristic, not biometrics)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct SpeakerProfile {
    mean_rms: f64,
    zcr: f64,
    centroid_hz: f64,
}

impl SpeakerProfile {
    fn compute(samples: &[f32], rate: u32) -> Self {
        if samples.len() < 256 || rate == 0 {
            return Self {
                mean_rms: 0.0,
                zcr: 0.0,
                centroid_hz: 0.0,
            };
        }

        let mut crossings = 0usize;
        for pair in samples.windows(2) {
            if (pair[0] >= 0.0) != (pair[1] >= 0.0) {
                crossings += 1;
            }
        }
        let zcr = crossings as f64 / (samples.len() - 1) as f64;

        const N: usize = 512;
        const BINS: usize = 64;
        let window: Vec<f32> = (0..N)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / (N - 1) as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();

        // cos/sin tables for bins 1..=BINS. BINS * N pairs, computed once.
        let mut cos_tab = vec![0f32; BINS * N];
        let mut sin_tab = vec![0f32; BINS * N];
        for k in 1..=BINS {
            for n in 0..N {
                let angle = std::f32::consts::TAU * k as f32 * n as f32 / N as f32;
                cos_tab[(k - 1) * N + n] = angle.cos();
                sin_tab[(k - 1) * N + n] = angle.sin();
            }
        }

        let mut rms_sum = 0f64;
        let mut centroid_sum = 0f64;
        let mut frames = 0usize;

        for frame in samples.chunks_exact(N) {
            rms_sum += rms(frame) as f64;
            let mut weighted = 0f64;
            let mut magnitudes = 0f64;
            for k in 1..=BINS {
                let mut re = 0f32;
                let mut im = 0f32;
                let offset = (k - 1) * N;
                for n in 0..N {
                    let value = frame[n] * window[n];
                    re += value * cos_tab[offset + n];
                    im -= value * sin_tab[offset + n];
                }
                let magnitude = ((re * re + im * im) as f64).sqrt();
                let frequency = k as f64 * rate as f64 / N as f64;
                weighted += magnitude * frequency;
                magnitudes += magnitude;
            }
            if magnitudes > 1e-9 {
                centroid_sum += weighted / magnitudes;
            }
            frames += 1;
        }

        if frames == 0 {
            return Self {
                mean_rms: 0.0,
                zcr,
                centroid_hz: 0.0,
            };
        }

        Self {
            mean_rms: rms_sum / frames as f64,
            zcr,
            centroid_hz: centroid_sum / frames as f64,
        }
    }

    fn to_json(self, sample_rate: u32, created: u64) -> Value {
        json!({
            "version": 1,
            "created": created,
            "sample_rate": sample_rate,
            "mean_rms": self.mean_rms,
            "zcr": self.zcr,
            "spectral_centroid_hz": self.centroid_hz,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            mean_rms: value.get("mean_rms")?.as_f64()?,
            zcr: value.get("zcr")?.as_f64()?,
            centroid_hz: value.get("spectral_centroid_hz")?.as_f64()?,
        })
    }

    fn similarity(&self, other: &Self) -> f64 {
        fn rel(a: f64, b: f64) -> f64 {
            let sum = a.abs() + b.abs();
            if sum < 1e-9 {
                1.0
            } else {
                (1.0 - (a - b).abs() / (sum + 1e-9)).clamp(0.0, 1.0)
            }
        }
        let score = 0.2 * rel(self.mean_rms, other.mean_rms)
            + 0.3 * rel(self.zcr, other.zcr)
            + 0.5 * rel(self.centroid_hz, other.centroid_hz);
        score.clamp(0.0, 1.0)
    }
}

fn speaker_path() -> PathBuf {
    voice_dir().join("speaker.json")
}

fn load_speaker_profile(path: &Path) -> Option<SpeakerProfile> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    SpeakerProfile::from_json(&value)
}

const HEURISTIC_NOTE: &str =
    "Perfil espectral grosseiro (RMS, taxa de cruzamentos por zero, centroide espectral). \
     É uma heurística de conveniência, não segurança biométrica.";

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let mut path = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        path.push(rest);
        return path;
    }
    PathBuf::from(raw)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::invalid(format!("`{key}` é obrigatório")))
}

/// Runs a child process with a hard timeout, collecting stdout/stderr.
fn run_with_timeout(
    binary: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<(bool, String, String), String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("helper speech: {e}"))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_reader = stdout_pipe.map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut buffer);
            buffer
        })
    });
    let stderr_reader = stderr_pipe.map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut buffer);
            buffer
        })
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                return Err(format!("helper speech: {e}"));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(40));
    };

    let stdout = stdout_reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    if timed_out {
        return Err(format!(
            "helper speech excedeu {}s e foi encerrado",
            timeout.as_secs()
        ));
    }

    Ok((status.map(|s| s.success()).unwrap_or(false), stdout, stderr))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn make_spec(name: &'static str, summary: &'static str, risk: Risk, args: Value) -> ToolSpec {
    ToolSpec {
        name,
        summary,
        risk,
        args,
    }
}

// --- voice_devices ---------------------------------------------------------

pub struct VoiceDevices {
    spec: ToolSpec,
}

impl VoiceDevices {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_devices",
                "Lista microfones e saídas de áudio disponíveis, com o dispositivo padrão.",
                Risk::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoiceDevices {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: &Value, _ctx: &ToolCtx) -> ToolResult {
        let host = cpal::default_host();

        let default_input = host
            .default_input_device()
            .and_then(|device| device.name().ok());
        let default_output = host
            .default_output_device()
            .and_then(|device| device.name().ok());

        let mut inputs = Vec::new();
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                let Ok(name) = device.name() else { continue };
                let config = device.default_input_config().ok();
                inputs.push(json!({
                    "name": name,
                    "default": default_input.as_deref() == Some(name.as_str()),
                    "sample_rate": config.as_ref().map(|c| c.sample_rate().0),
                    "channels": config.as_ref().map(|c| c.channels()),
                    "sample_format": config.as_ref().map(|c| format!("{:?}", c.sample_format())),
                }));
            }
        }

        let mut outputs = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let Ok(name) = device.name() else { continue };
                let config = device.default_output_config().ok();
                outputs.push(json!({
                    "name": name,
                    "default": default_output.as_deref() == Some(name.as_str()),
                    "sample_rate": config.as_ref().map(|c| c.sample_rate().0),
                    "channels": config.as_ref().map(|c| c.channels()),
                }));
            }
        }

        Ok(json!({
            "inputs": inputs,
            "outputs": outputs,
            "default_input": default_input,
            "default_output": default_output,
            "nota": "Taxas diferentes de 16 kHz são reamostradas de forma linear na captura.",
        }))
    }
}

// --- voice_listen_once -----------------------------------------------------

pub struct VoiceListenOnce {
    spec: ToolSpec,
}

impl VoiceListenOnce {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_listen_once",
                "Escuta uma única fala com VAD (até ~20 s), grava um WAV mono 16 kHz em \
                 ~/Library/Caches/FridayOS/voice e devolve o caminho para a camada de STT.",
                Risk::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1000,
                            "maximum": 60000,
                            "description": "Tempo máximo de espera pelo início da fala."
                        }
                    },
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoiceListenOnce {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "acao": "capturar uma fala e gravar WAV em ~/Library/Caches/FridayOS/voice",
                "timeout_ms": args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(15_000),
            }));
        }

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(15_000)
            .clamp(1_000, 60_000);

        let mut handle = start().map_err(ToolError::failed)?;
        let events = handle.recv_events();
        let wait_deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                handle.stop();
                return Err(ToolError::failed(
                    "nenhuma fala detectada dentro do tempo limite",
                ));
            }
            match events.recv_timeout(remaining) {
                Ok(VoiceEvent::SpeechStart) => break,
                Ok(VoiceEvent::Error(error)) => {
                    handle.stop();
                    return Err(ToolError::failed(error));
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    handle.stop();
                    return Err(ToolError::failed(
                        "nenhuma fala detectada dentro do tempo limite",
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    handle.stop();
                    return Err(ToolError::failed(
                        "thread de captura encerrou inesperadamente",
                    ));
                }
            }
        }

        let speech_started = Instant::now();
        let capture_deadline = speech_started + Duration::from_secs(26);

        loop {
            let remaining = capture_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                handle.stop();
                return Err(ToolError::failed(
                    "a fala não terminou dentro do tempo limite de captura",
                ));
            }
            match events.recv_timeout(remaining) {
                Ok(VoiceEvent::SpeechEnd {
                    mut captured,
                    sample_rate,
                }) => {
                    handle.stop();
                    let rate = sample_rate.max(1);
                    let max_samples = (rate as u64 * MAX_UTTERANCE_MS / 1000) as usize;
                    if captured.len() > max_samples {
                        captured.truncate(max_samples);
                    }
                    if captured.is_empty() {
                        return Err(ToolError::failed("fala capturada vazia"));
                    }
                    let path = wav_path();
                    write_wav(&path, &captured, rate).map_err(ToolError::failed)?;
                    let duration_ms = captured.len() as u64 * 1000 / rate as u64;
                    return Ok(json!({
                        "path": path.display().to_string(),
                        "duration_ms": duration_ms,
                        "sample_rate": rate,
                        "samples": captured.len(),
                        "nota": "WAV PCM 16-bit mono; pronto para STT.",
                    }));
                }
                Ok(VoiceEvent::Error(error)) => {
                    handle.stop();
                    return Err(ToolError::failed(error));
                }
                Ok(_) => {}
                Err(_) => {
                    handle.stop();
                    return Err(ToolError::failed(
                        "a fala não terminou dentro do tempo limite de captura",
                    ));
                }
            }
        }
    }
}

// --- voice_play ------------------------------------------------------------

pub struct VoicePlay {
    spec: ToolSpec,
}

impl VoicePlay {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_play",
                "Reproduz um arquivo de áudio pelo alto-falante padrão usando afplay.",
                Risk::Reversible,
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Caminho do arquivo de áudio (WAV, MP3, M4A...)."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoicePlay {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let raw = required_str(args, "path")?;
        let path = expand_tilde(raw);

        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "comando": format!("/usr/bin/afplay {}", path.display()),
            }));
        }
        if !path.is_file() {
            return Err(ToolError::invalid(format!(
                "arquivo de áudio não encontrado: {}",
                path.display()
            )));
        }

        let mut child = std::process::Command::new("/usr/bin/afplay")
            .arg(&path)
            .spawn()
            .map_err(|e| ToolError::failed(format!("afplay: {e}")))?;
        let pid = child.id();
        // Reap in the background so we do not leave a zombie or block the caller.
        let _ = thread::Builder::new()
            .name("friday-afplay".to_string())
            .spawn(move || {
                let _ = child.wait();
            });

        Ok(json!({
            "path": path.display().to_string(),
            "playing": true,
            "pid": pid,
        }))
    }
}

// --- voice_say -------------------------------------------------------------

pub struct VoiceSay {
    spec: ToolSpec,
}

impl VoiceSay {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_say",
                "Fala um texto pelo `say` (TTS de reserva, usado só quando o ChatGPT TTS \
                 não estiver disponível).",
                Risk::Reversible,
                json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Texto a ser falado."
                        },
                        "voice": {
                            "type": "string",
                            "description": "Nome da voz do macOS (opcional), ex.: 'Luciana'."
                        }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoiceSay {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let text = required_str(args, "text")?;
        let voice = args
            .get("voice")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "text": text,
                "voice": voice,
                "comando": match &voice {
                    Some(voice) => format!("/usr/bin/say -v {voice} <texto>"),
                    None => "/usr/bin/say <texto>".to_string(),
                },
            }));
        }

        let mut command = std::process::Command::new("/usr/bin/say");
        if let Some(voice) = &voice {
            command.arg("-v").arg(voice);
        }
        let mut child = command
            .arg(text)
            .spawn()
            .map_err(|e| ToolError::failed(format!("say: {e}")))?;
        let pid = child.id();
        let _ = thread::Builder::new()
            .name("friday-say".to_string())
            .spawn(move || {
                let _ = child.wait();
            });

        Ok(json!({
            "spoken": true,
            "pid": pid,
            "voice": voice,
        }))
    }
}

// --- voice_wake_listen -----------------------------------------------------

pub struct VoiceWakeListen {
    spec: ToolSpec,
}

impl VoiceWakeListen {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_wake_listen",
                "Escuta pelo helper Swift (reconhecimento on-device) por alguns segundos e \
                 devolve o texto reconhecido; com `phrase`, indica se a palavra de ativação apareceu.",
                Risk::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "phrase": {
                            "type": "string",
                            "description": "Palavra/frase de ativação a procurar (opcional)."
                        },
                        "seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 30,
                            "description": "Duração da escuta em segundos (padrão 8)."
                        }
                    },
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoiceWakeListen {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let seconds = args
            .get("seconds")
            .and_then(Value::as_u64)
            .unwrap_or(8)
            .clamp(1, 30);
        let phrase = args
            .get("phrase")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "acao": format!("escutar {seconds}s com o helper Swift `speech`"),
                "phrase": phrase,
            }));
        }

        let binary = crate::swift::helper("speech")?;
        let seconds_arg = seconds.to_string();
        let timeout = Duration::from_secs(seconds + 12);
        let (success, stdout, stderr) =
            run_with_timeout(&binary, &["listen", &seconds_arg], timeout)
                .map_err(ToolError::failed)?;

        let mut recognized = String::new();
        let mut finals = 0u64;
        let mut saw_json = false;

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            saw_json = true;
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                return Err(ToolError::failed(format!("speech: {error}")));
            }
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                recognized = text.to_string();
                if value.get("final").and_then(Value::as_bool) == Some(true) {
                    finals += 1;
                }
            }
        }

        if !success && recognized.is_empty() {
            let detail = stderr.trim();
            return Err(ToolError::failed(if detail.is_empty() {
                "helper speech falhou sem detalhes".to_string()
            } else {
                format!("helper speech falhou: {detail}")
            }));
        }
        if !saw_json {
            return Err(ToolError::failed("helper speech não devolveu transcrições"));
        }

        let matched = match phrase.as_deref() {
            Some(phrase) => recognized.to_lowercase().contains(&phrase.to_lowercase()),
            None => !recognized.trim().is_empty(),
        };

        Ok(json!({
            "text": recognized,
            "matched": matched,
            "phrase": phrase,
            "seconds": seconds,
            "finals": finals,
        }))
    }
}

// --- voice_speaker_profile -------------------------------------------------

pub struct VoiceSpeakerProfile {
    spec: ToolSpec,
}

impl VoiceSpeakerProfile {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_speaker_profile",
                "Calcula um perfil espectral grosseiro (RMS médio, ZCR, centroide espectral) \
                 de um WAV e salva/compara com ~/Library/Caches/FridayOS/voice/speaker.json. \
                 Heurística de conveniência, NÃO é segurança biométrica.",
                Risk::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "WAV PCM de referência (idealmente 16 kHz mono)."
                        },
                        "save": {
                            "type": "boolean",
                            "description": "Salvar como perfil de referência (padrão true)."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoiceSpeakerProfile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let raw = required_str(args, "path")?;
        let path = expand_tilde(raw);
        let save = args.get("save").and_then(Value::as_bool).unwrap_or(true);

        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "path": path.display().to_string(),
                "save": save,
                "baseline_path": speaker_path().display().to_string(),
            }));
        }
        if !path.is_file() {
            return Err(ToolError::invalid(format!(
                "WAV não encontrado: {}",
                path.display()
            )));
        }

        let (samples, rate) = read_wav_mono(&path).map_err(ToolError::failed)?;
        if samples.len() < 1_600 {
            return Err(ToolError::invalid(
                "WAV curto demais; grave pelo menos ~0,1 s de fala",
            ));
        }

        let profile = SpeakerProfile::compute(&samples, rate);
        let baseline_path = speaker_path();
        let existing = load_speaker_profile(&baseline_path);
        let similarity = existing.map(|baseline| profile.similarity(&baseline));

        if save {
            let payload = profile.to_json(rate, now_secs());
            if let Some(parent) = baseline_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolError::failed(format!("não foi possível criar {}: {e}", parent.display()))
                })?;
            }
            std::fs::write(
                &baseline_path,
                serde_json::to_string_pretty(&payload).unwrap_or_default(),
            )
            .map_err(|e| {
                ToolError::failed(format!(
                    "não foi possível salvar {}: {e}",
                    baseline_path.display()
                ))
            })?;
        }

        Ok(json!({
            "profile": profile.to_json(rate, now_secs()),
            "baseline_path": baseline_path.display().to_string(),
            "saved": save,
            "similarity": similarity,
            "nota": HEURISTIC_NOTE,
        }))
    }
}

// --- voice_verify ----------------------------------------------------------

pub struct VoiceVerify {
    spec: ToolSpec,
}

impl VoiceVerify {
    fn new() -> Self {
        Self {
            spec: make_spec(
                "voice_verify",
                "Compara um WAV com o perfil salvo e devolve uma similaridade de 0 a 1. \
                 Heurística grosseira, NÃO use como autenticação ou segurança biométrica.",
                Risk::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "WAV a comparar com o perfil salvo."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Tool for VoiceVerify {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let raw = required_str(args, "path")?;
        let path = expand_tilde(raw);

        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "path": path.display().to_string(),
                "baseline_path": speaker_path().display().to_string(),
            }));
        }
        if !path.is_file() {
            return Err(ToolError::invalid(format!(
                "WAV não encontrado: {}",
                path.display()
            )));
        }

        let baseline_path = speaker_path();
        let baseline = load_speaker_profile(&baseline_path).ok_or_else(|| {
            ToolError::failed(format!(
                "nenhum perfil salvo em {}; rode voice_speaker_profile primeiro",
                baseline_path.display()
            ))
        })?;

        let (samples, rate) = read_wav_mono(&path).map_err(ToolError::failed)?;
        if samples.len() < 1_600 {
            return Err(ToolError::invalid(
                "WAV curto demais; grave pelo menos ~0,1 s de fala",
            ));
        }

        let profile = SpeakerProfile::compute(&samples, rate);
        let score = profile.similarity(&baseline);
        let verdict = if score >= 0.75 {
            "provavelmente o mesmo locutor"
        } else if score >= 0.5 {
            "inconclusivo"
        } else {
            "provavelmente outro locutor"
        };

        Ok(json!({
            "score": score,
            "verdict": verdict,
            "baseline_path": baseline_path.display().to_string(),
            "profile": profile.to_json(rate, now_secs()),
            "nota": HEURISTIC_NOTE,
        }))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut Registry) {
    registry.add(VoiceDevices::new());
    registry.add(VoiceListenOnce::new());
    registry.add(VoicePlay::new());
    // `voice_say` (macOS say) is intentionally NOT registered: Friday's only
    // accepted voice is the ChatGPT read-aloud voice.
    registry.add(VoiceWakeListen::new());
    registry.add(VoiceSpeakerProfile::new());
    registry.add(VoiceVerify::new());
}
