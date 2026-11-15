//! Microphone smoke test: `cargo run -p friday-tools --example mic_test`

use std::time::{Duration, Instant};

use friday_tools::tools::voice::{self, VoiceEvent};

fn main() {
    let mut mic = match voice::start() {
        Ok(mic) => mic,
        Err(error) => {
            eprintln!("não foi possível abrir o microfone: {error}");
            std::process::exit(1);
        }
    };

    println!("microfone aberto — fale algo nos próximos 6 segundos…");

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut peak = 0.0f32;
    loop {
        match mic.recv_timeout(Duration::from_millis(500)) {
            Ok(VoiceEvent::Level(level)) => {
                peak = peak.max(level);
                print!("\rnível {:.3}  pico {:.3}", level, peak);
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            Ok(VoiceEvent::SpeechStart) => println!("\nfala detectada"),
            Ok(VoiceEvent::SpeechEnd {
                captured,
                sample_rate,
            }) => {
                let path = std::env::temp_dir().join("friday-mic-test.wav");
                voice::write_wav(&path, &captured, sample_rate).ok();
                println!(
                    "\nfim da fala: {} amostras @ {} Hz → {}",
                    captured.len(),
                    sample_rate,
                    path.display()
                );
                mic.stop();
                return;
            }
            Ok(VoiceEvent::BargeIn) => println!("\nbarge-in"),
            Ok(VoiceEvent::Error(error)) => {
                eprintln!("\nerro: {error}");
                mic.stop();
                std::process::exit(1);
            }
            Err(_) => {
                if Instant::now() > deadline {
                    println!("\nsem fala detectada (pico {peak:.3})");
                    mic.stop();
                    return;
                }
            }
        }
    }
}
