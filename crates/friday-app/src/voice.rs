//! Runtime de voz da Friday: microfone, transcrição, fala e palavra de ativação.
//!
//! A captura e o VAD vivem em `friday_tools::tools::voice`; este módulo só
//! traduz os eventos para a UI, grava o WAV do enunciado e orquestra STT/TTS.
//!
//! - [`Mic`] — escuta contínua com barge-in, entrega [`MicEvent`]s sem bloquear.
//! - [`wake_once`] — uma passada do helper Swift de reconhecimento on-device.
//! - [`transcribe_wav`] — WAV gravado pelo [`Mic`] → texto, via ChatGPT.
//! - [`speak`] — texto → voz (ChatGPT read-aloud, com `/usr/bin/say` de reserva).
//! - [`stop_speaking`] — mata o `afplay`/`say` em curso.
//!
//! Caveats:
//! - macOS TCC exige `NSMicrophoneUsageDescription` no Info.plist do app; sem
//!   isso a captura falha (viramos [`MicEvent::Error`], nunca panic).
//! - Cada chamada de [`transcribe_wav`] cria um runtime tokio próprio e uma
//!   thread; ~1–3 ms de custo, mas não aninha runtimes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use friday_core::auth::CodexAuth;
use friday_core::wav::{encode_pcm16_wav, parse_wav};
use friday_core::tts;
use friday_tools::tools::voice::{start as start_capture, write_wav, VoiceEvent, VoiceHandle};

// ---------------------------------------------------------------------------
// API pública
// ---------------------------------------------------------------------------

/// Fase atual do subsistema de voz, para a UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoicePhase {
    Idle,
    Listening,
    Transcribing,
    Speaking,
}

/// Eventos de captura já traduzidos para a UI.
#[derive(Debug, Clone)]
pub enum MicEvent {
    /// Cruzou o limiar de fala.
    SpeechStarted,
    /// Nível RMS (a cada 2º frame, para não inundar a UI).
    Level(f32),
    /// Enunciado encerrado e gravado em disco.
    Utterance { path: PathBuf },
    /// O usuário falou por cima da Friday.
    BargeIn,
    /// Falha de captura.
    Error(String),
}

/// Microfone contínuo: envolve `friday_tools::tools::voice` e grava os WAVs.
pub struct Mic {
    handle: Option<VoiceHandle>,
    events: Option<Receiver<VoiceEvent>>,
    level_tick: u32,
}

impl Mic {
    /// Abre o microfone padrão. Falha de permissão/dispositivo vira `Err`.
    pub fn start() -> Result<Mic, String> {
        let mut handle = start_capture()?;
        let events = handle.recv_events();
        Ok(Mic {
            handle: Some(handle),
            events: Some(events),
            level_tick: 0,
        })
    }

    /// Poll não bloqueante; chame a cada frame da UI.
    pub fn poll(&mut self) -> Vec<MicEvent> {
        let mut out = Vec::new();
        let mut disconnected = false;

        if let Some(rx) = self.events.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(VoiceEvent::Level(level)) => {
                        self.level_tick = self.level_tick.wrapping_add(1);
                        if self.level_tick % 2 == 0 {
                            out.push(MicEvent::Level(level));
                        }
                    }
                    Ok(VoiceEvent::SpeechStart) => out.push(MicEvent::SpeechStarted),
                    Ok(VoiceEvent::SpeechEnd {
                        captured,
                        sample_rate,
                    }) => {
                        if captured.is_empty() {
                            continue;
                        }
                        let path = utterance_path();
                        match write_wav(&path, &captured, sample_rate.max(1)) {
                            Ok(()) => out.push(MicEvent::Utterance { path }),
                            Err(error) => out.push(MicEvent::Error(error)),
                        }
                    }
                    Ok(VoiceEvent::BargeIn) => out.push(MicEvent::BargeIn),
                    Ok(VoiceEvent::Error(error)) => out.push(MicEvent::Error(error)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if disconnected {
            self.events = None;
        }
        out
    }

    /// Enquanto a Friday fala, os frames do mic viram detecção de barge-in.
    pub fn set_speaking(&self, speaking: bool) {
        if let Some(handle) = self.handle.as_ref() {
            handle.set_speaking(speaking);
        }
    }

    /// Encerra a captura e junta a thread.
    pub fn stop(&mut self) {
        self.events = None;
        if let Some(handle) = self.handle.take() {
            handle.stop();
        }
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Roda o helper Swift de fala uma vez (~`seconds`) e diz se a frase apareceu.
///
/// Comparação sem caixa e sem acentos. Bloqueante.
pub fn wake_once(phrase: &str, seconds: u32) -> Result<bool, String> {
    let transcript = listen_once(seconds)?;
    if phrase.trim().is_empty() {
        return Ok(!transcript.trim().is_empty());
    }
    Ok(normalize_text(&transcript).contains(&normalize_text(phrase)))
}

/// Transcreve um WAV gravado pelo [`Mic`] (cria o próprio runtime tokio). Bloqueante.
/// Transcribes a WAV with SFSpeechRecognizer running fully on-device.
fn siri_transcribe_file(path: &Path) -> Result<String, String> {
    let helper = friday_tools::swift::helper("speech").map_err(|error| error.to_string())?;
    let output = Command::new(helper)
        .arg("transcribe")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("reconhecimento local: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "reconhecimento local: {}",
            stderr.trim().lines().last().unwrap_or("falhou")
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().rev() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(text) = value.get("text").and_then(|text| text.as_str()) {
                if !text.trim().is_empty() {
                    return Ok(text.trim().to_string());
                }
            }
        }
    }
    Err("reconhecimento local sem texto".into())
}

pub fn transcribe_wav(path: &Path) -> Result<String, String> {
    // Transcrição apenas no Mac (SFSpeechRecognizer on-device).
    let bytes = std::fs::read(path)
        .map_err(|e| format!("não foi possível ler {}: {e}", path.display()))?;
    let wav = normalize_for_stt(&bytes)?;
    let local_path = temp_dir().join(format!(
        "stt-input-{}-{}.wav",
        std::process::id(),
        stamp()
    ));
    let _ = std::fs::create_dir_all(temp_dir());
    let _ = std::fs::write(&local_path, &wav);
    let result = siri_transcribe_file(&local_path);
    let _ = std::fs::remove_file(&local_path);
    result
}

/// Fala o texto em voz alta (bloqueante), com cancelamento cooperativo.
/// Usa exclusivamente o read-aloud do ChatGPT. Nunca usa `say`, AV Foundation
/// ou Piper. Retorna cedo (`Ok(())`) quando `cancel` é acionado.
pub fn speak(text: &str, cancel: &Arc<AtomicBool>) -> Result<(), String> {
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return Ok(());
    }

    let cancel = Arc::clone(cancel);

    // Se já estamos dentro de um runtime tokio, `block_on` entraria em pânico.
    // Então o trabalho inteiro roda numa thread dedicada.
    if tokio::runtime::Handle::try_current().is_ok() {
        let worker = thread::Builder::new()
            .name("friday-tts".to_string())
            .spawn(move || speak_sentences(&sentences, &cancel))
            .map_err(|e| format!("não foi possível iniciar a voz: {e}"))?;
        return worker
            .join()
            .map_err(|_| "a thread de voz falhou".to_string())?;
    }

    speak_sentences(&sentences, &cancel)
}

/// Mata qualquer `afplay` (ou `say`) em curso deste processo.
pub fn stop_speaking() {
    if let Ok(mut slot) = playback().lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    if let Ok(mut slot) = siri_playback().lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Reprodução (global) + fio de fala
// ---------------------------------------------------------------------------

static PLAYBACK: Mutex<Option<Child>> = Mutex::new(None);
/// Native system/Siri voice helper (`speech speak`).
static SIRI_PLAYBACK: Mutex<Option<Child>> = Mutex::new(None);
/// Subtle rate/pitch multipliers supplied by the presence module.
static PROSODY_RATE: AtomicU32 = AtomicU32::new(1_065_353_216); // 1.0f32 bits
static PROSODY_PITCH: AtomicU32 = AtomicU32::new(1_065_353_216);

fn siri_playback() -> &'static Mutex<Option<Child>> {
    &SIRI_PLAYBACK
}

/// Updates the subtle prosody used by the native voice. Values are multipliers
/// around 1.0; the helper clamps them to a natural-sounding band.
pub fn set_prosody(rate: f32, pitch: f32) {
    PROSODY_RATE.store(rate.clamp(0.75, 1.35).to_bits(), Ordering::Relaxed);
    PROSODY_PITCH.store(pitch.clamp(0.85, 1.18).to_bits(), Ordering::Relaxed);
}

/// Short spoken backchannel ("entendo", "um momento") without blocking the UI.
pub fn speak_quick_async(text: impl Into<String>) {
    let text = text.into();
    std::thread::spawn(move || {
        let cancel = AtomicBool::new(false);
        let _ = play_text(&text, &cancel);
    });
}

/// Speaks text with the ChatGPT voice — the only voice Friday uses.
/// Local STT stays on-device; synthesis is never replaced by `say`/Piper/AV.
fn play_text(text: &str, cancel: &AtomicBool) -> Result<(), String> {
    stop_speaking();
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    friday_tools::tools::voice::mark_audible_playback();

    if let Some(path) = cache_lookup(text) {
        let result = play_wav(&path, cancel);
        set_speech_level(0.0);
        return result;
    }
    if tts_blocked() {
        vlog("voz do Codex em cooldown; permanecendo em silêncio");
        return Ok(());
    }

    let rt = runtime()?;
    let auth = CodexAuth::discover();
    let started = std::time::Instant::now();
    match synthesize_file(&rt, &auth, text) {
        Ok(path) => {
            vlog(format!(
                "codex: sintetizado {}ms · {}",
                started.elapsed().as_millis(),
                &text.chars().take(40).collect::<String>()
            ));
            cache_store(text, &path);
            let result = play_wav(&path, cancel);
            let _ = std::fs::remove_file(&path);
            set_speech_level(0.0);
            result
        }
        Err(error) => {
            if error.contains("limite") {
                block_tts(120);
            }
            Err(error)
        }
    }
}

/// Current TTS amplitude (0..1), updated while afplay runs so the orb can
/// pulse with the actual voice instead of a fixed sine.
static SPEECH_LEVEL: AtomicU32 = AtomicU32::new(0);

pub fn speech_level() -> f32 {
    f32::from_bits(SPEECH_LEVEL.load(Ordering::Relaxed))
}

fn set_speech_level(value: f32) {
    SPEECH_LEVEL.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

/// After a 429, stop hammering ChatGPT TTS for a minute and use the local voice.
static TTS_COOLDOWN: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn tts_blocked() -> bool {
    TTS_COOLDOWN
        .lock()
        .ok()
        .and_then(|slot| *slot)
        .map(|until| std::time::Instant::now() < until)
        .unwrap_or(false)
}

fn block_tts(seconds: u64) {
    if let Ok(mut slot) = TTS_COOLDOWN.lock() {
        *slot = Some(std::time::Instant::now() + std::time::Duration::from_secs(seconds));
    }
}

/// Photos of already synthesized sentences, so repeats (greetings, confirmations)
/// cost nothing.
static TTS_CACHE: Mutex<Option<std::collections::HashMap<String, PathBuf>>> = Mutex::new(None);

fn cache_key(text: &str) -> String {
    let voice = std::env::var("FRIDAY_VOICE")
        .ok()
        .or_else(friday_core::tts::configured_voice)
        .unwrap_or_else(|| friday_core::tts::DEFAULT_VOICE.to_string());
    format!("{voice}\u{1}{text}")
}

fn cache_lookup(text: &str) -> Option<PathBuf> {
    let mut guard = TTS_CACHE.lock().ok()?;
    let cache = guard.get_or_insert_with(std::collections::HashMap::new);
    let key = cache_key(text);
    let path = cache.get(&key)?.clone();
    if path.is_file() {
        Some(path)
    } else {
        cache.remove(&key);
        None
    }
}

/// Permanent home for cached clips. Temp files are deleted after playback, so
/// the cache must own its own copy or every entry died on first use.
fn cache_dir() -> PathBuf {
    voice_dir().join("cache")
}

fn cache_file_path(key: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    cache_dir().join(format!("{:016x}.wav", hasher.finish()))
}

fn cache_store(text: &str, path: &Path) {
    let key = cache_key(text);
    let target = cache_file_path(&key);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::copy(path, &target).is_err() {
        return;
    }
    if let Ok(mut guard) = TTS_CACHE.lock() {
        let cache = guard.get_or_insert_with(std::collections::HashMap::new);
        if cache.len() >= 48 {
            // Drop only the in-memory index; files are pruned on startup.
            cache.clear();
        }
        cache.insert(key, target);
    }
}

/// Keeps the cache bounded: newest 64 clips survive.
fn prune_cache() {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("wav") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= 64 {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    for (_, path) in files.iter().take(files.len() - 64) {
        let _ = std::fs::remove_file(path);
    }
}

/// Keeps the voice cache bounded: drops utterances older than a week and any
/// beyond the newest 200 (30 s captures made this grow by ~100 MB).
pub fn prune_utterances() {
    prune_cache();
    let Ok(entries) = std::fs::read_dir(voice_dir()) else {
        return;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(7 * 24 * 3600);
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("utterance-") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        if modified < cutoff {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        files.push((modified, path));
    }
    if files.len() <= 200 {
        return;
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.iter().skip(200) {
        let _ = std::fs::remove_file(path);
    }
}

fn playback() -> &'static Mutex<Option<Child>> {
    &PLAYBACK
}

fn speak_sentences(sentences: &[String], cancel: &Arc<AtomicBool>) -> Result<(), String> {
    for sentence in sentences {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        play_text(sentence.trim(), cancel)?;
    }
    Ok(())
}

/// Reproduz um WAV com envelope na HUD e cancelamento cooperativo.
fn play_wav(path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(pcm) = friday_core::wav::parse_wav(&bytes) {
            let mono = pcm.mono_f32();
            let window = (pcm.sample_rate as usize / 20).max(1);
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            std::thread::spawn(move || {
                for chunk in mono.chunks(window) {
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let rms = (chunk.iter().map(|sample| sample * sample).sum::<f32>()
                        / chunk.len().max(1) as f32)
                        .sqrt();
                    set_speech_level((rms * 6.0).clamp(0.0, 1.0));
                    thread::sleep(Duration::from_millis(50));
                }
                set_speech_level(0.0);
            });
            let mut command = Command::new("/usr/bin/afplay");
            command.arg(path);
            let result = run_playback(&mut command, cancel);
            stop.store(true, Ordering::Relaxed);
            set_speech_level(0.0);
            return result;
        }
    }
    Err("WAV inválido".to_string())
}

/// Inicia o comando, registra no global e espera checando `cancel`.
fn run_playback(command: &mut Command, cancel: &AtomicBool) -> Result<(), String> {
    stop_speaking();

    let child = command
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("falha ao iniciar a reprodução: {e}"))?;

    {
        let mut slot = playback()
            .lock()
            .map_err(|_| "estado de áudio indisponível".to_string())?;
        *slot = Some(child);
    }

    loop {
        if cancel.load(Ordering::Relaxed) {
            stop_speaking();
            return Ok(());
        }

        let mut slot = match playback().lock() {
            Ok(slot) => slot,
            Err(_) => return Ok(()),
        };

        match slot.as_mut() {
            None => return Ok(()), // `stop_speaking` externo já encerrou
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    *slot = None;
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(format!("reprodução terminou com {status}"))
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    *slot = None;
                    return Err(format!("reprodução: {e}"));
                }
            },
        }

        drop(slot);
        thread::sleep(Duration::from_millis(40));
    }
}

// ---------------------------------------------------------------------------
// Runtime tokio
// ---------------------------------------------------------------------------

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("falha ao iniciar o runtime assíncrono: {e}"))
}

// ---------------------------------------------------------------------------
// Caminhos
// ---------------------------------------------------------------------------

static UTTERANCE_SEQ: AtomicU64 = AtomicU64::new(0);

fn voice_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Caches/FridayOS")
        .join("voice")
}

fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn utterance_path() -> PathBuf {
    let seq = UTTERANCE_SEQ.fetch_add(1, Ordering::Relaxed);
    voice_dir().join(format!(
        "utterance-{}-{}-{seq}.wav",
        std::process::id(),
        stamp()
    ))
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("friday-voice")
}

fn temp_audio_path() -> PathBuf {
    let seq = UTTERANCE_SEQ.fetch_add(1, Ordering::Relaxed);
    temp_dir().join(format!("speech-{}-{}-{seq}.wav", std::process::id(), stamp()))
}

// ---------------------------------------------------------------------------
// Áudio para STT
// ---------------------------------------------------------------------------

/// Garante PCM16 mono 16 kHz (o formato que o backend espera).
fn normalize_for_stt(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let pcm = parse_wav(bytes).map_err(|e| format!("WAV inválido: {e}"))?;
    if pcm.samples.is_empty() {
        return Err("o áudio está vazio; fale por alguns segundos e tente de novo".to_string());
    }

    let mono = pcm.mono_f32();
    let rate = pcm.sample_rate.max(1);
    let samples = if rate == 16_000 {
        mono
    } else {
        resample_linear(&mono, rate, 16_000)
    };

    let pcm16: Vec<i16> = samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16)
        .collect();
    if pcm16.is_empty() {
        return Err("o áudio ficou vazio após a conversão".to_string());
    }

    Ok(encode_pcm16_wav(&pcm16, 16_000))
}

fn resample_linear(input: &[f32], src: u32, dst: u32) -> Vec<f32> {
    if input.is_empty() || src == 0 || dst == 0 {
        return Vec::new();
    }
    if src == dst {
        return input.to_vec();
    }

    let ratio = src as f64 / dst as f64;
    let out_len = ((input.len() as f64) / ratio).round().max(1.0) as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = input[idx.min(last)];
        let b = input[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}

// ---------------------------------------------------------------------------
// Helper Swift de fala (palavra de ativação)
// ---------------------------------------------------------------------------

fn listen_once(seconds: u32) -> Result<String, String> {
    let binary = friday_tools::swift::helper("speech").map_err(|e| e.to_string())?;
    let seconds = seconds.clamp(1, 120);

    let mut child = Command::new(&binary)
        .arg("listen")
        .arg(seconds.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("não foi possível iniciar o helper de fala: {e}"))?;

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

    let deadline = Instant::now() + Duration::from_secs(seconds as u64 + 15);
    let mut timed_out = false;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("helper de fala: {e}"));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(60));
    };

    let stdout = stdout_reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    let mut transcript = String::new();
    let mut helper_error: Option<String> = None;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(text) = json_string_field(line, "text") {
            if !text.trim().is_empty() {
                transcript = text;
            }
        }
        if let Some(error) = json_string_field(line, "error") {
            helper_error = Some(error);
        }
    }

    if let Some(error) = helper_error {
        return Err(format!("reconhecimento de fala: {error}"));
    }

    if transcript.trim().is_empty() {
        if timed_out {
            return Err("o helper de fala excedeu o tempo limite".to_string());
        }
        let success = status.map(|s| s.success()).unwrap_or(false);
        if !success {
            let detail = stderr.trim();
            return Err(if detail.is_empty() {
                "o helper de fala falhou sem detalhes".to_string()
            } else {
                format!("o helper de fala falhou: {detail}")
            });
        }
    }

    Ok(transcript)
}

/// Extrai um campo string de uma linha JSON sem depender de serde_json.
fn json_string_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let colon = rest.find(':')? + 1;
    let rest = rest[colon..].trim_start();
    let rest = rest.strip_prefix('"')?;

    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                '/' => out.push('/'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
    }

    None // string não terminada
}

// ---------------------------------------------------------------------------
// Texto: limpeza, frases e normalização
// ---------------------------------------------------------------------------

/// Limite de caracteres por frase enviada ao TTS.
const MAX_SENTENCE: usize = 220;

/// Remove ruído de markdown e blocos de código antes de falar.
fn clean_for_speech(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let words: Vec<&str> = trimmed
            .split_whitespace()
            .filter(|word| {
                let bare = word.trim_start_matches('(');
                !bare.starts_with("http://") && !bare.starts_with("https://")
            })
            .collect();
        if words.is_empty() {
            continue;
        }

        let joined = words.join(" ");
        for c in joined.chars() {
            match c {
                '`' | '*' | '_' | '#' | '>' | '|' | '[' | ']' | '(' | ')' | '~' => {}
                other => out.push(other),
            }
        }
        out.push(' ');
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Divide em frases de até [`MAX_SENTENCE`] chars, respeitando `.!?…`.
fn split_sentences(text: &str) -> Vec<String> {
    let cleaned = clean_for_speech(text);
    let chars: Vec<char> = cleaned.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<char> = Vec::new();

    for i in 0..chars.len() {
        let c = chars[i];
        cur.push(c);

        let boundary = matches!(c, '.' | '!' | '?' | '…')
            && chars.get(i + 1).map_or(true, |next| next.is_whitespace());
        if boundary && cur.len() >= 8 {
            flush_sentence(&mut cur, &mut out);
            continue;
        }

        if cur.len() >= MAX_SENTENCE {
            let mut split_at = None;
            for idx in (40..cur.len()).rev() {
                if matches!(cur[idx], ',' | ';' | ':') {
                    split_at = Some(idx + 1);
                    break;
                }
            }
            if split_at.is_none() {
                for idx in (40..cur.len()).rev() {
                    if cur[idx].is_whitespace() {
                        split_at = Some(idx + 1);
                        break;
                    }
                }
            }
            let idx = split_at.unwrap_or(MAX_SENTENCE);
            let head: String = cur.drain(..idx).collect();
            let head = head.trim().to_string();
            if !head.is_empty() {
                out.push(head);
            }
        }
    }

    flush_sentence(&mut cur, &mut out);
    out
}

fn flush_sentence(cur: &mut Vec<char>, out: &mut Vec<String>) {
    let sentence = cur.iter().collect::<String>();
    let sentence = sentence.trim().to_string();
    if !sentence.is_empty() {
        out.push(sentence);
    }
    cur.clear();
}

/// Minúsculas + sem acentos + espaços colapsados, para comparar frases.
fn normalize_text(text: &str) -> String {
    text.chars()
        .flat_map(|c| c.to_lowercase())
        .map(fold_char)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_short_sentences() {
        let sentences = split_sentences("Bom dia, senhor. Tudo em ordem! Algum problema?");
        assert_eq!(
            sentences,
            vec!["Bom dia, senhor.", "Tudo em ordem!", "Algum problema?"]
        );
    }

    #[test]
    fn splits_long_chunk_at_boundary() {
        let long = "palavra ".repeat(80);
        let sentences = split_sentences(&long);
        assert!(sentences.len() > 1);
        for sentence in &sentences {
            assert!(sentence.chars().count() <= MAX_SENTENCE);
        }
    }

    #[test]
    fn strips_code_fences_and_markdown() {
        let cleaned = clean_for_speech("```rust\nlet x = 1;\n```\n**Olá** _mundo_");
        assert_eq!(cleaned, "Olá mundo");
    }

    #[test]
    fn normalizes_phrase_comparison() {
        assert_eq!(normalize_text("  FRIÁDA,  você? "), "friada, voce?");
        assert!(normalize_text("Ei, Friday!").contains(&normalize_text("friday")));
    }

    #[test]
    fn parses_json_text_field() {
        assert_eq!(
            json_string_field(r#"{"text":"bom dia","final":false}"#, "text").as_deref(),
            Some("bom dia")
        );
        assert_eq!(
            json_string_field(r#"{"error":"sem mic"}"#, "error").as_deref(),
            Some("sem mic")
        );
    }

    #[test]
    fn resamples_length() {
        let input: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = resample_linear(&input, 48_000, 16_000);
        assert!((out.len() as i64 - 16_000).abs() <= 1);
    }
}

/// Streaming variant used by the live call loop: sentences arrive while the
/// model is still writing, and each one is synthesized *while the previous one
/// plays*. That removes the dead air between sentences that a naive
/// synthesize-then-play loop leaves behind.
/// Same log file as the UI, so a silent Friday is easy to diagnose.
fn vlog(message: impl AsRef<str>) {
    use std::io::Write;
    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = std::path::PathBuf::from(home).join("Library/Logs/FridayOS.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{secs}] tts: {}", message.as_ref());
    }
}

pub fn speak_stream(
    sentences: std::sync::mpsc::Receiver<String>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let cancel = Arc::clone(cancel);
    if tokio::runtime::Handle::try_current().is_ok() {
        let worker = thread::Builder::new()
            .name("friday-tts-stream".to_string())
            .spawn(move || stream_sentences(sentences, &cancel))
            .map_err(|e| format!("não foi possível iniciar a voz: {e}"))?;
        return worker
            .join()
            .map_err(|_| "a thread de voz falhou".to_string())?;
    }
    stream_sentences(sentences, &cancel)
}

fn stream_sentences(
    sentences: std::sync::mpsc::Receiver<String>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    for sentence in sentences {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let text = sentence.trim();
        if text.is_empty() {
            continue;
        }
        vlog(format!("voz codex: {}", &text.chars().take(60).collect::<String>()));
        play_text(text, cancel)?;
    }
    set_speech_level(0.0);
    Ok(())
}

fn usage_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/tts-usage.json")
}

/// Persisted per-hour budget: ChatGPT read-aloud is limited by requests per
/// hour, and a 429 is slow to recover from. Better to stay under it.
fn check_budget() -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = usage_path();
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({ "requests": [], "blocked_until": 0 }));

    let blocked_until = value.get("blocked_until").and_then(|v| v.as_u64()).unwrap_or(0);
    if blocked_until > now {
        let wait = (blocked_until - now + 59) / 60;
        return Err(format!(
            "limite por hora da voz ChatGPT (conta Plus) atingido — volta em ~{wait} min"
        ));
    }

    let mut requests: Vec<u64> = value
        .get("requests")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default();
    requests.retain(|ts| now.saturating_sub(*ts) < 3600);
    let budget = std::env::var("FRIDAY_TTS_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(400);
    if requests.len() >= budget {
        return Err(format!(
            "orçamento local de voz por hora esgotado ({budget} pedidos); tenta de novo em alguns minutos"
        ));
    }
    requests.push(now);
    value["requests"] = serde_json::json!(requests);
    value["blocked_until"] = serde_json::json!(0);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, value.to_string());
    Ok(())
}

fn mark_blocked(seconds: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = usage_path();
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({ "requests": [], "blocked_until": 0 }));
    value["blocked_until"] = serde_json::json!(now + seconds);
    value["requests"] = serde_json::json!([]);
    let _ = std::fs::write(&path, value.to_string());
}

fn synthesize_file(
    rt: &tokio::runtime::Runtime,
    auth: &CodexAuth,
    text: &str,
) -> Result<PathBuf, String> {
    check_budget()?;
    let pcm = rt.block_on(tts::synthesize(auth, text)).map_err(|e| {
        if e.to_string().contains("limite") {
            mark_blocked(900);
        }
        e.to_string()
    })?;
    let samples: Vec<i16> = pcm
        .mono_f32()
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16)
        .collect();
    if samples.is_empty() {
        return Err("áudio vazio".into());
    }
    let bytes = encode_pcm16_wav(&samples, pcm.sample_rate.max(8_000));
    let path = temp_audio_path();
    std::fs::write(&path, &bytes).map_err(|e| format!("não foi possível gravar o áudio: {e}"))?;
    Ok(path)
}

