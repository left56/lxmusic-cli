//! 音频魔数、音质阶梯、试听片段判断。码率下限对齐觅音 `shared/quality.ts`。

pub const QUALITY_LADDER: [&str; 5] = ["flac24bit", "flac", "320k", "192k", "128k"];

const MIN_KBPS_ANY: f64 = 80.0;

pub fn min_kbps(quality: &str) -> Option<f64> {
    Some(match quality {
        "flac24bit" => 1400.0,
        "flac" => 700.0,
        "320k" => 256.0,
        "192k" => 160.0,
        "128k" => 96.0,
        _ => return None,
    })
}

/// `preferred` 为 `highest` 或空时走完整阶梯，否则从该档开始往下。
/// `available` 非空时只保留音源声明过的档；过滤后为空则仍用原阶梯。
pub fn qualities_to_try(preferred: &str, available: Option<&[String]>) -> Vec<String> {
    let preferred = preferred.trim();
    let mut ladder: Vec<String> = if preferred.is_empty() || preferred == "highest" {
        QUALITY_LADDER.iter().map(|s| (*s).to_string()).collect()
    } else if let Some(pos) = QUALITY_LADDER.iter().position(|q| *q == preferred) {
        QUALITY_LADDER[pos..].iter().map(|s| (*s).to_string()).collect()
    } else {
        vec![preferred.to_string()]
    };
    if let Some(avail) = available {
        if !avail.is_empty() {
            let filtered: Vec<String> = ladder
                .iter()
                .filter(|q| avail.iter().any(|a| a == *q))
                .cloned()
                .collect();
            if !filtered.is_empty() {
                ladder = filtered;
            }
        }
    }
    ladder
}

pub fn estimate_kbps(bytes: u64, duration_sec: f64) -> Option<f64> {
    if bytes == 0 || duration_sec <= 0.0 {
        return None;
    }
    Some((bytes as f64) * 8.0 / duration_sec / 1000.0)
}

/// 低于任何整曲档位的码率，视为试听片段。时长未知时不拦截。
pub fn is_preview_clip(bytes: u64, duration_sec: Option<f64>) -> bool {
    let Some(dur) = duration_sec else {
        return false;
    };
    match estimate_kbps(bytes, dur) {
        Some(kbps) => kbps < MIN_KBPS_ANY,
        None => false,
    }
}

pub fn quality_plausible(bytes: u64, duration_sec: Option<f64>, quality: &str) -> bool {
    let Some(min) = min_kbps(quality) else {
        return true;
    };
    let Some(dur) = duration_sec else {
        return true;
    };
    match estimate_kbps(bytes, dur) {
        Some(kbps) => kbps >= min,
        None => true,
    }
}

pub fn quality_from_bitrate(kbps: f64) -> &'static str {
    if kbps >= 1400.0 {
        "flac24bit"
    } else if kbps >= 700.0 {
        "flac"
    } else if kbps >= 256.0 {
        "320k"
    } else if kbps >= 160.0 {
        "192k"
    } else {
        "128k"
    }
}

pub fn sniff_audio_ext(buf: &[u8]) -> Option<&'static str> {
    if buf.len() < 4 {
        return None;
    }
    if buf.starts_with(b"fLaC") {
        return Some("flac");
    }
    if buf.starts_with(b"ID3") {
        return Some("mp3");
    }
    if buf[0] == 0xff && buf[1] & 0xe0 == 0xe0 {
        return Some("mp3");
    }
    if buf.starts_with(b"OggS") {
        return Some("ogg");
    }
    if buf.len() >= 12 && buf.starts_with(b"RIFF") && &buf[8..12] == b"WAVE" {
        return Some("wav");
    }
    if buf.len() >= 8 && &buf[4..8] == b"ftyp" {
        return Some("m4a");
    }
    if buf.starts_with(b"MAC") {
        return Some("ape");
    }
    None
}

pub fn lossless_claim_mismatch(claimed: &str, sniffed: Option<&str>) -> bool {
    matches!(claimed, "flac" | "flac24bit") && sniffed == Some("mp3")
}

pub fn ext_for_quality(quality: &str) -> &'static str {
    match quality {
        "flac" | "flac24bit" => "flac",
        _ => "mp3",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_highest_and_specific() {
        assert_eq!(
            qualities_to_try("highest", None),
            vec!["flac24bit", "flac", "320k", "192k", "128k"]
        );
        assert_eq!(
            qualities_to_try("320k", None),
            vec!["320k", "192k", "128k"]
        );
        let avail = vec!["128k".into(), "flac".into()];
        assert_eq!(
            qualities_to_try("highest", Some(&avail)),
            vec!["flac", "128k"]
        );
    }

    #[test]
    fn sniff_and_preview() {
        assert_eq!(sniff_audio_ext(b"fLaC............"), Some("flac"));
        assert_eq!(sniff_audio_ext(b"ID3..........."), Some("mp3"));
        assert_eq!(sniff_audio_ext(&[0xff, 0xfb, 0x90, 0x00]), Some("mp3"));
        assert_eq!(sniff_audio_ext(b"OggS"), Some("ogg"));
        assert_eq!(sniff_audio_ext(b"RIFF1234WAVE"), Some("wav"));
        assert_eq!(sniff_audio_ext(b"\0\0\0\x18ftypM4A "), Some("m4a"));
        assert_eq!(sniff_audio_ext(b"MAC "), Some("ape"));
        assert_eq!(sniff_audio_ext(b"not audio"), None);

        // 938KB / 223s ≈ 34kbps
        assert!(is_preview_clip(938 * 1024, Some(223.0)));
        // 7MB / 223s 远高于 80kbps
        assert!(!is_preview_clip(7 * 1024 * 1024, Some(223.0)));
        assert!(!is_preview_clip(1000, None));
        assert!(lossless_claim_mismatch("flac24bit", Some("mp3")));
        assert!(!lossless_claim_mismatch("320k", Some("mp3")));
        assert!(!quality_plausible(8731 * 1024, Some(223.0), "flac24bit"));
        assert!(quality_plausible(25 * 1024 * 1024, Some(223.0), "flac"));
    }
}
