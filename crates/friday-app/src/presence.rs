//! Presence / personality state for the Friday HUD.
//!
//! Pure logic: no GPUI types, no file/network IO, no wall clock. Every bit of
//! motion is driven by the `dt_ms` passed to [`Presence::update`], so the same
//! sequence of inputs always yields the same [`Visual`] (deterministic /
//! seek-safe). The caller is responsible for rendering the returned plain
//! data and for speaking the strings returned by [`Presence::backchannel`].
//!
//! Visual rules (by user request): one fixed red accent, forever. No state or
//! mood changes the colour, and nothing blinks, flashes or snaps: every visual
//! amplitude is exponentially eased toward its target.

use std::f32::consts::TAU;

/// The one and only HUD accent colour. Never varied by state or mood.
pub const ACCENT: (f32, f32, f32) = (0.90, 0.10, 0.10);

/// Coarse interaction state of the orb / HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceState {
    Idle,
    Listening,
    HearingUser,
    Thinking,
    Speaking,
    Confirming,
    Alert,
}

/// Broad mood, used for eased visual amplitude and voice prosody only — never
/// for colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Calm,
    Focused,
    Wry,
    Urgent,
}

/// Plain, GPUI-free snapshot for one frame of the HUD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Visual {
    /// Overall orb scale (roughly 0.8 .. 1.5).
    pub orb_scale: f32,
    /// Outer ring radius in normalised HUD units.
    pub ring_radius: f32,
    /// Inner ring radius in the same units.
    pub ring_inner_radius: f32,
    /// Glow opacity (amplitude), 0..1.
    pub glow_alpha: f32,
    /// Eye opening, 0..1, continuously eased (never a blink).
    pub eye_open: f32,
    /// Horizontal eye direction, -1..1.
    pub eye_dx: f32,
    /// Vertical eye direction, -1..1.
    pub eye_dy: f32,
    /// Fixed red accent as (r, g, b) in 0..1. Always equal to [`ACCENT`].
    pub accent: (f32, f32, f32),
    /// Short status label in Portuguese.
    pub status: &'static str,
}

const BACKCHANNELS: [&str; 4] = ["Um momento.", "Entendo.", "Continue.", "Certo."];

/// Smoothed level at/above which a frame counts as voice.
const VOICE_LEVEL: f32 = 0.14;
/// Voice frames needed before we say the user is speaking.
const VOICE_ONSET_MS: u32 = 240;
/// Silence needed to forget a voice run.
const VOICE_RELEASE_MS: u32 = 180;
/// Time constants for the level smoother (fast attack, slow release).
const ATTACK_TAU_MS: f32 = 70.0;
const RELEASE_TAU_MS: f32 = 210.0;
/// Time constant for every visual amplitude: no abrupt jumps, ever.
const VISUAL_TAU_MS: f32 = 260.0;

/// Deterministic presence/personality state machine.
///
/// Public fields are the ones the app may read or set directly; the timing
/// internals stay private so the smoothing/cooldown rules cannot be broken.
#[derive(Debug, Clone)]
pub struct Presence {
    pub state: PresenceState,
    pub mood: Mood,
    /// Smoothed input level, 0..1.
    pub level: f32,
    /// Normalised mode string (lowercase, de-accented).
    pub mode: String,
    /// Simulated elapsed time in milliseconds. Advances only via `update`.
    pub tick: u64,

    phase: f32,
    voice_ms: u32,
    silence_ms: u32,
    backchannel_cooldown_ms: u32,
    last_backchannel: Option<usize>,

    // Eased visual amplitudes. `visual()` only reads these, so it is pure.
    orb_smooth: f32,
    ring_smooth: f32,
    glow_smooth: f32,
    eye_smooth: f32,
}

impl Default for Presence {
    fn default() -> Self {
        Self::new()
    }
}

impl Presence {
    /// Creates an idle, calm presence in "normal" mode.
    pub fn new() -> Self {
        let (orb, ring, glow, eye) = visual_targets(PresenceState::Idle, Mood::Calm, 0.0, 0.5);
        Self {
            state: PresenceState::Idle,
            mood: Mood::Calm,
            level: 0.0,
            mode: "normal".to_string(),
            tick: 0,
            phase: 0.0,
            voice_ms: 0,
            silence_ms: 0,
            backchannel_cooldown_ms: 0,
            last_backchannel: None,
            orb_smooth: orb,
            ring_smooth: ring,
            glow_smooth: glow,
            eye_smooth: eye,
        }
    }

    /// Overrides the coarse state (e.g. when the agent starts answering).
    /// `update` only auto-drives `Idle`/`Listening`/`HearingUser`.
    pub fn set_state(&mut self, state: PresenceState) {
        self.state = state;
    }

    /// Overrides the mood.
    pub fn set_mood(&mut self, mood: Mood) {
        self.mood = mood;
    }

    /// Convenience: derive the mood from a piece of text.
    pub fn set_mood_from_text(&mut self, text: &str) {
        self.mood = mood_for_text(text);
    }

    /// Sets/normalises the mode string.
    pub fn set_mode(&mut self, mode: &str) {
        self.mode = normalize_mode(mode);
    }

    pub fn state(&self) -> PresenceState {
        self.state
    }

    pub fn mood(&self) -> Mood {
        self.mood
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Advances the simulation by `dt_ms` milliseconds.
    ///
    /// - smooths `level` (fast attack, slow release, always finite/0..1),
    /// - detects sustained voice and advances the state,
    /// - advances the breathing phase,
    /// - eases every visual amplitude toward its target (no jumps),
    /// - counts down the backchannel cooldown.
    pub fn update(&mut self, dt_ms: u32, level: f32, transcript_activity: bool, mode: &str) {
        let dt = dt_ms as f32;
        let raw = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };

        self.tick = self.tick.saturating_add(dt_ms as u64);
        self.backchannel_cooldown_ms = self.backchannel_cooldown_ms.saturating_sub(dt_ms);

        if self.mode != mode {
            self.mode = normalize_mode(mode);
        }

        let tau = if raw > self.level {
            ATTACK_TAU_MS
        } else {
            RELEASE_TAU_MS
        };
        self.level = clamp01(self.level + (raw - self.level) * (1.0 - (-dt / tau).exp()));

        // A frame counts as voice if the smoothed level is loud OR the STT
        // reports activity; the run must persist for VOICE_ONSET_MS.
        let voiced = self.level >= VOICE_LEVEL || transcript_activity;
        if voiced {
            self.voice_ms = self.voice_ms.saturating_add(dt_ms);
            self.silence_ms = 0;
        } else {
            self.silence_ms = self.silence_ms.saturating_add(dt_ms);
            if self.silence_ms >= VOICE_RELEASE_MS {
                self.voice_ms = 0;
            }
        }
        let hearing = self.voice_ms >= VOICE_ONSET_MS;
        match self.state {
            PresenceState::Idle | PresenceState::Listening => {
                if hearing {
                    self.state = PresenceState::HearingUser;
                }
            }
            PresenceState::HearingUser => {
                if !hearing {
                    self.state = PresenceState::Listening;
                }
            }
            // Thinking / Speaking / Confirming / Alert are owned by the app.
            _ => {}
        }

        // Breathing phase, wrapped to one cycle. Cap the per-call step so a
        // huge dt (app suspended) cannot lose float precision or jump wildly.
        let breath_hz = match (self.mood, self.state) {
            (Mood::Urgent, _) | (_, PresenceState::Alert) => 0.55,
            (_, PresenceState::Speaking | PresenceState::Thinking) => 0.36,
            _ => 0.24,
        };
        let phase_dt = dt.min(1000.0) / 1000.0;
        self.phase = (self.phase + TAU * breath_hz * phase_dt) % TAU;

        // Ease all amplitudes toward the new targets. A state/mood change
        // shifts the target, never the current value.
        let breathe = self.phase.sin() * 0.5 + 0.5; // 0..1
        let (orb, ring, glow, eye) = visual_targets(self.state, self.mood, self.level, breathe);
        let ease = 1.0 - (-dt / VISUAL_TAU_MS).exp();
        self.orb_smooth += (orb - self.orb_smooth) * ease;
        self.ring_smooth += (ring - self.ring_smooth) * ease;
        self.glow_smooth += (glow - self.glow_smooth) * ease;
        self.eye_smooth += (eye - self.eye_smooth) * ease;
    }

    /// Computes the frame's visual snapshot. Pure: no mutation, no IO.
    ///
    /// The accent is always [`ACCENT`]; no colour or alpha effect varies.
    pub fn visual(&self) -> Visual {
        let ring_radius = clamp_range(self.ring_smooth, 0.30, 0.70);
        let status = match self.state {
            PresenceState::Idle => "Em repouso",
            PresenceState::Listening => "Ouvindo",
            PresenceState::HearingUser => "Ouvindo você",
            PresenceState::Thinking => "Pensando",
            PresenceState::Speaking => "Falando",
            PresenceState::Confirming => "Confirmando",
            PresenceState::Alert => "Atenção",
        };

        Visual {
            orb_scale: clamp_range(self.orb_smooth, 0.8, 1.5),
            ring_radius,
            ring_inner_radius: ring_radius * 0.62,
            glow_alpha: clamp01(self.glow_smooth),
            eye_open: clamp01(self.eye_smooth),
            // Slow, continuous drift only — no per-tick alternation.
            eye_dx: clamp_range(
                (self.phase * 0.6).cos() * 0.10 + (self.level - 0.5) * 0.08,
                -1.0,
                1.0,
            ),
            eye_dy: clamp_range((self.phase * 0.45).sin() * 0.06, -1.0, 1.0),
            accent: ACCENT,
            status,
        }
    }

    /// Occasional short acknowledgments, only while hearing the user.
    ///
    /// Cooldown is a deterministic 2–4 s derived from `tick`, and the same
    /// phrase is never returned twice in a row. The app may speak the result.
    pub fn backchannel(&mut self) -> Option<&'static str> {
        if self.state != PresenceState::HearingUser || self.backchannel_cooldown_ms > 0 {
            return None;
        }
        let index = match self.last_backchannel {
            Some(previous) => (previous + 1) % BACKCHANNELS.len(),
            None => (self.tick as usize / 1700) % BACKCHANNELS.len(),
        };
        self.last_backchannel = Some(index);
        let jitter = (self.tick / 137 + index as u64 * 491) % 2001; // 0..=2000
        self.backchannel_cooldown_ms = 2000 + jitter as u32;
        Some(BACKCHANNELS[index])
    }

    /// `(rate, pitch)` multipliers for the native voice, clamped to sane ranges.
    pub fn prosody(&self) -> (f32, f32) {
        let (mood_rate, mood_pitch): (f32, f32) = match self.mood {
            Mood::Calm => (0.94, 0.92),
            Mood::Focused => (1.00, 0.96),
            Mood::Wry => (1.05, 1.06),
            Mood::Urgent => (1.18, 1.12),
        };
        let (state_rate, state_pitch): (f32, f32) = match self.state {
            PresenceState::Idle => (1.00, 1.00),
            PresenceState::Listening => (0.99, 1.00),
            PresenceState::HearingUser => (1.00, 1.00),
            PresenceState::Thinking => (0.96, 0.98),
            PresenceState::Speaking => (0.98, 1.00),
            PresenceState::Confirming => (1.00, 1.02),
            PresenceState::Alert => (1.08, 1.06),
        };
        (
            (mood_rate * state_rate).clamp(0.75, 1.35),
            (mood_pitch * state_pitch).clamp(0.80, 1.30),
        )
    }

    /// Short HUD label for the current mode.
    pub fn mode_label(&self) -> &'static str {
        match self.mode.as_str() {
            "reuniao" | "reunion" | "meeting" => "Reunião",
            "foco" | "focus" => "Foco",
            "dirigindo" | "driving" => "Dirigindo",
            "jogo" | "jogos" | "game" | "gaming" => "Jogo",
            _ => "Normal",
        }
    }
}

/// Keyword heuristic for mood. No dependencies, case/accent tolerant.
pub fn mood_for_text(text: &str) -> Mood {
    let lower = text.to_lowercase();
    if ["urgente", "urgencia", "erro", "perigo", "now", "quick", "rapido"]
        .iter()
        .any(|word| contains_word(&lower, word))
    {
        Mood::Urgent
    } else if ["piada", "engracado", "irônico", "ironico", "wry"]
        .iter()
        .any(|word| contains_word(&lower, word))
    {
        Mood::Wry
    } else if ["foco", "concentra", "analisa", "analise", "analyz"]
        .iter()
        .any(|word| contains_word(&lower, word))
    {
        Mood::Focused
    } else {
        Mood::Calm
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn clamp01(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn clamp_range(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        lo
    }
}

/// Targets for the eased amplitudes: (orb scale, ring radius, glow, eye open).
/// State/mood only nudge amplitude; they never touch colour.
fn visual_targets(state: PresenceState, mood: Mood, level: f32, breathe: f32) -> (f32, f32, f32, f32) {
    let base_scale = match state {
        PresenceState::Idle => 1.0,
        PresenceState::Listening => 1.03,
        PresenceState::HearingUser => 1.08,
        PresenceState::Thinking => 1.05,
        PresenceState::Speaking => 1.10,
        PresenceState::Confirming => 1.06,
        PresenceState::Alert => 1.14,
    };
    let mood_gain = match mood {
        Mood::Calm => 0.0,
        Mood::Focused => 0.01,
        Mood::Wry => 0.005,
        Mood::Urgent => 0.03,
    };
    let state_glow = match state {
        PresenceState::Idle => 0.0,
        PresenceState::Listening => 0.04,
        PresenceState::HearingUser => 0.10,
        PresenceState::Thinking => 0.06,
        PresenceState::Speaking => 0.12,
        PresenceState::Confirming => 0.08,
        PresenceState::Alert => 0.18,
    };
    let base_eye = match state {
        PresenceState::Idle => 0.38,
        PresenceState::Listening => 0.72,
        PresenceState::HearingUser => 0.95,
        PresenceState::Thinking => 0.60,
        PresenceState::Speaking => 0.85,
        PresenceState::Confirming => 0.90,
        PresenceState::Alert => 1.0,
    };

    (
        clamp_range(base_scale + mood_gain + breathe * 0.035 + level * 0.14, 0.8, 1.5),
        clamp_range(0.40 + breathe * 0.025 + level * 0.06, 0.30, 0.70),
        clamp01(0.16 + level * 0.45 + breathe * 0.06 + state_glow + mood_gain * 0.5),
        clamp01(base_eye + (level - 0.4) * 0.15),
    )
}

/// Matches a keyword only on alphanumeric word boundaries, so "now" does not
/// match "knowledge" and "erro" does not match "erros...".
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        let left_ok = before.map_or(true, |c| !c.is_alphanumeric());
        let right_ok = after.map_or(true, |c| !c.is_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        from = if end > start { end } else { start + 1 };
    }
    false
}

/// Lowercase + de-accent common Portuguese characters for mode matching.
fn normalize_mode(mode: &str) -> String {
    mode.to_lowercase()
        .chars()
        .map(|c| match c {
            'ã' | 'á' | 'à' | 'â' => 'a',
            'é' | 'ê' => 'e',
            'í' => 'i',
            'ó' | 'ô' => 'o',
            'ú' => 'u',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(p: &mut Presence, frames: u32, dt_ms: u32, level: f32, transcript: bool) {
        for _ in 0..frames {
            p.update(dt_ms, level, transcript, "normal");
        }
    }

    #[test]
    fn backchannel_respects_cooldown_and_never_repeats() {
        let mut p = Presence::new();
        p.set_state(PresenceState::HearingUser);

        let first = p.backchannel().expect("hearing user should acknowledge");
        assert!(p.backchannel().is_none(), "no immediate repeat");

        pump(&mut p, 15, 100, 0.4, true); // 1500 ms < min cooldown
        assert!(p.backchannel().is_none(), "still cooling down");

        pump(&mut p, 30, 100, 0.4, true); // +3000 ms > max cooldown
        let second = p.backchannel().expect("cooldown expired");
        assert_ne!(first, second, "consecutive phrases must differ");

        p.set_state(PresenceState::Thinking);
        pump(&mut p, 45, 100, 0.4, true);
        assert!(p.backchannel().is_none(), "only while HearingUser");
    }

    #[test]
    fn state_transitions_follow_sustained_voice() {
        let mut p = Presence::new();
        assert_eq!(p.state(), PresenceState::Idle);

        pump(&mut p, 5, 100, 0.02, false);
        assert_eq!(p.state(), PresenceState::Idle, "quiet stays idle");

        pump(&mut p, 2, 100, 0.5, true); // 200 ms < 240 ms onset
        assert_eq!(p.state(), PresenceState::Idle, "not sustained yet");

        pump(&mut p, 1, 100, 0.5, true); // 300 ms
        assert_eq!(p.state(), PresenceState::HearingUser);

        pump(&mut p, 5, 100, 0.0, false); // silence
        assert_eq!(p.state(), PresenceState::Listening);

        p.set_state(PresenceState::Thinking);
        pump(&mut p, 10, 100, 0.9, true);
        assert_eq!(p.state(), PresenceState::Thinking, "app owns other states");
    }

    #[test]
    fn mood_heuristics() {
        assert_eq!(mood_for_text("ISSO É URGENTE!"), Mood::Urgent);
        assert_eq!(mood_for_text("erro no servidor"), Mood::Urgent);
        assert_eq!(mood_for_text("perigo"), Mood::Urgent);
        assert_eq!(mood_for_text("do it now"), Mood::Urgent);
        assert_eq!(mood_for_text("quick, please"), Mood::Urgent);
        assert_eq!(mood_for_text("que piada engraçada"), Mood::Wry);
        assert_eq!(mood_for_text("irônico"), Mood::Wry);
        assert_eq!(mood_for_text("foco total"), Mood::Focused);
        assert_eq!(mood_for_text("concentra e analisa"), Mood::Focused);
        assert_eq!(mood_for_text("bom dia, Friday"), Mood::Calm);
        assert_eq!(mood_for_text("knowledge is power"), Mood::Calm);
    }

    #[test]
    fn visual_is_bounded_and_deterministic() {
        let mut a = Presence::new();
        let mut b = Presence::new();
        for frame in 0..60 {
            let level = (frame as f32 / 60.0 * TAU).sin().abs() * 0.8;
            let transcript = frame % 7 == 0;
            a.update(16, level, transcript, "reunião");
            b.update(16, level, transcript, "reunião");
        }
        assert_eq!(a.visual(), b.visual());

        let v = a.visual();
        assert!(v.orb_scale >= 0.8 && v.orb_scale <= 1.5);
        assert!(v.ring_inner_radius < v.ring_radius);
        assert!((0.0..=1.0).contains(&v.glow_alpha));
        assert!((0.0..=1.0).contains(&v.eye_open));
        assert!((-1.0..=1.0).contains(&v.eye_dx));
        assert!((-1.0..=1.0).contains(&v.eye_dy));
        for c in [v.accent.0, v.accent.1, v.accent.2] {
            assert!((0.0..=1.0).contains(&c));
        }
        assert_eq!(a.mode_label(), "Reunião");
    }

    #[test]
    fn accent_never_changes_with_state_or_mood() {
        let mut p = Presence::new();
        let fixed = p.visual().accent;
        assert_eq!(fixed, ACCENT);
        for state in [
            PresenceState::Idle,
            PresenceState::Listening,
            PresenceState::HearingUser,
            PresenceState::Thinking,
            PresenceState::Speaking,
            PresenceState::Confirming,
            PresenceState::Alert,
        ] {
            for mood in [Mood::Calm, Mood::Focused, Mood::Wry, Mood::Urgent] {
                p.set_state(state);
                p.set_mood(mood);
                pump(&mut p, 3, 16, 0.6, true);
                assert_eq!(p.visual().accent, fixed, "accent must be constant");
            }
        }
    }

    #[test]
    fn no_blink_no_flicker_no_jumps() {
        let mut p = Presence::new();
        pump(&mut p, 90, 16, 0.35, false); // settle under constant input

        let mut previous = p.visual();
        for _ in 0..180 {
            p.update(16, 0.35, false, "normal");
            let current = p.visual();
            assert!((current.orb_scale - previous.orb_scale).abs() < 0.01);
            assert!((current.ring_radius - previous.ring_radius).abs() < 0.01);
            assert!((current.glow_alpha - previous.glow_alpha).abs() < 0.01);
            // No on/off alternation of the eyes.
            assert!((current.eye_open - previous.eye_open).abs() < 0.01);
            previous = current;
        }

        // Even a hard state/mood switch only nudges the eased target.
        p.set_state(PresenceState::Alert);
        p.set_mood(Mood::Urgent);
        p.update(16, 0.35, false, "normal");
        let after = p.visual();
        assert!((after.orb_scale - previous.orb_scale).abs() < 0.02);
        assert!((after.glow_alpha - previous.glow_alpha).abs() < 0.02);
        assert!((after.eye_open - previous.eye_open).abs() < 0.02);
    }

    #[test]
    fn prosody_clamps_and_follows_mood() {
        let mut p = Presence::new();
        p.set_mood(Mood::Calm);
        let calm = p.prosody();
        p.set_mood(Mood::Urgent);
        let urgent = p.prosody();
        assert!(urgent.0 > calm.0 && urgent.1 > calm.1);

        for mood in [Mood::Calm, Mood::Focused, Mood::Wry, Mood::Urgent] {
            for state in [
                PresenceState::Idle,
                PresenceState::Listening,
                PresenceState::HearingUser,
                PresenceState::Thinking,
                PresenceState::Speaking,
                PresenceState::Confirming,
                PresenceState::Alert,
            ] {
                p.set_mood(mood);
                p.set_state(state);
                let (rate, pitch) = p.prosody();
                assert!((0.75..=1.35).contains(&rate));
                assert!((0.80..=1.30).contains(&pitch));
            }
        }
    }

    #[test]
    fn mode_labels_and_non_finite_level() {
        let mut p = Presence::new();
        for (mode, label) in [
            ("normal", "Normal"),
            ("Reunião", "Reunião"),
            ("foco", "Foco"),
            ("dirigindo", "Dirigindo"),
            ("jogo", "Jogo"),
        ] {
            p.set_mode(mode);
            assert_eq!(p.mode_label(), label);
        }
        p.update(16, f32::NAN, false, "normal");
        assert_eq!(p.level(), 0.0, "NaN must not poison the smoother");
    }
}
