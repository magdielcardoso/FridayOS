//! Minimal WAV helpers. STT wants PCM16 mono at 16 kHz; TTS returns PCM16 WAV
//! (after `afconvert`) at whatever rate the service picked.

#[derive(Debug, Clone)]
pub struct Pcm {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Pcm {
    pub fn mono_f32(&self) -> Vec<f32> {
        let channels = self.channels.max(1) as usize;
        self.samples
            .chunks(channels)
            .map(|frame| {
                let sum: i32 = frame.iter().map(|&s| s as i32).sum();
                sum as f32 / frame.len() as f32 / 32_768.0
            })
            .collect()
    }
}

pub fn encode_pcm16_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Parses the first `fmt ` + `data` chunks of a canonical WAV file.
pub fn parse_wav(bytes: &[u8]) -> anyhow::Result<Pcm> {
    anyhow::ensure!(bytes.len() >= 44, "WAV truncado");
    anyhow::ensure!(&bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE", "WAV inválido");

    let mut sample_rate = 0u32;
    let mut channels = 1u16;
    let mut offset = 12usize;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body = offset + 8;
        if id == b"fmt " {
            anyhow::ensure!(body + 16 <= bytes.len(), "fmt truncado");
            channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap()).max(1);
            sample_rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            let bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap());
            anyhow::ensure!(bits == 16, "áudio não é PCM16");
        } else if id == b"data" {
            anyhow::ensure!(sample_rate > 0, "WAV sem taxa de amostragem");
            let end = (body + size).min(bytes.len());
            let samples = bytes[body..end]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            return Ok(Pcm {
                samples,
                sample_rate,
                channels,
            });
        }
        offset = body + size + (size % 2);
    }
    anyhow::bail!("WAV sem bloco de áudio")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_pcm16_mono() {
        let samples: Vec<i16> = vec![0, 1, -1, 12_345, -12_345, i16::MAX, i16::MIN];
        let wav = encode_pcm16_wav(&samples, 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + samples.len() * 2);

        let parsed = parse_wav(&wav).expect("parse");
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples, samples);
    }

    #[test]
    fn converts_to_mono_f32() {
        let pcm = Pcm {
            samples: vec![16_384, 16_384, 0, 0],
            sample_rate: 16_000,
            channels: 2,
        };
        let mono = pcm.mono_f32();
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.5).abs() < 0.001);
        assert!(mono[1].abs() < f32::EPSILON);
    }

    #[test]
    fn rejects_truncated_wav() {
        assert!(parse_wav(b"RIFF").is_err());
        assert!(parse_wav(&[0u8; 60]).is_err());
    }
}
