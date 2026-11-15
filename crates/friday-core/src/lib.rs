//! FridayOS core: no UI, no global state.
//!
//! - [`auth`] — ChatGPT/Codex subscription credentials and token refresh
//! - [`wav`] — PCM16 WAV encode/decode
//! - [`stt`] — speech to text (ChatGPT transcribe)
//! - [`tts`] — text to speech (ChatGPT read aloud)

pub mod auth;
pub mod codex;
pub mod stt;
pub mod tts;
pub mod wav;

pub use auth::{AuthError, CodexAuth, Session};
