//! The unencrypted AAC/TS media playlists used by Radiko live broadcasts.
//! Timeline identity uses program-date-time, never sequence numbers across sessions.
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use reqwest::Url;

#[derive(Clone, Debug)]
pub struct Segment {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub url: Url,
}

pub struct Playlist {
    pub segments: Vec<Segment>,
    pub target_seconds: f64,
    pub ended: bool,
}

/// None requests the FFmpeg compatibility path (no timeline, encryption, fMP4 or byte ranges).
pub fn parse(body: &str, base: &Url) -> Result<Option<Playlist>> {
    if !body.trim_start().starts_with("#EXTM3U") {
        bail!("服务器未返回 HLS 清单");
    }
    let mut target = 5.0;
    let mut ended = false;
    let mut time = None;
    let mut duration = None;
    let mut segments = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = value.parse::<f64>().context("HLS 目标时长无效")?;
            if !target.is_finite() || target <= 0.0 || target > 120.0 {
                bail!("HLS 目标时长超出范围");
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            time = Some(
                DateTime::parse_from_rfc3339(value)
                    .context("HLS 分片时间无效")?
                    .with_timezone(&Utc),
            );
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let seconds = value
                .split(',')
                .next()
                .unwrap_or_default()
                .parse::<f64>()
                .context("HLS 分片时长无效")?;
            if !seconds.is_finite() || seconds <= 0.0 || seconds > 120.0 {
                bail!("HLS 分片时长超出范围");
            }
            duration = Some(Duration::microseconds(
                (seconds * 1_000_000.0).round() as i64
            ));
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if line == "#EXT-X-DISCONTINUITY" {
            time = None;
        } else if line.starts_with("#EXT-X-MAP:")
            || line.starts_with("#EXT-X-BYTERANGE:")
            || (line.starts_with("#EXT-X-KEY:") && line != "#EXT-X-KEY:METHOD=NONE")
        {
            return Ok(None);
        } else if !line.is_empty() && !line.starts_with('#') {
            let (Some(start), Some(duration)) = (time, duration.take()) else {
                return Ok(None);
            };
            let end = start + duration;
            let url = base.join(line).context("HLS 分片地址无效")?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!("不支持的分片地址协议");
            }
            segments.push(Segment { start, end, url });
            time = Some(end);
        }
    }
    if segments.is_empty() {
        bail!("HLS 清单没有音频分片");
    }
    if segments.windows(2).any(|s| s[1].start < s[0].start) {
        bail!("HLS 分片时间倒退");
    }
    Ok(Some(Playlist {
        segments,
        target_seconds: target,
        ended,
    }))
}

/// Reject HTTP error bodies and truncated ADTS/TS responses before committing a segment.
pub fn audio_extension(bytes: &[u8]) -> Result<&'static str> {
    if bytes.len() >= 188
        && bytes.len().is_multiple_of(188)
        && bytes.chunks_exact(188).all(|packet| packet[0] == 0x47)
    {
        return Ok("ts");
    }
    let mut position = 0;
    if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let size = bytes[6..10]
            .iter()
            .fold(0usize, |size, b| (size << 7) | (b & 0x7f) as usize);
        position = 10 + size + if bytes[5] & 0x10 != 0 { 10 } else { 0 };
    }
    let mut frames = 0;
    while position + 7 <= bytes.len() {
        let header = &bytes[position..position + 7];
        if header[0] != 0xff || header[1] & 0xf6 != 0xf0 {
            break;
        }
        let size = (((header[3] & 3) as usize) << 11)
            | ((header[4] as usize) << 3)
            | ((header[5] as usize) >> 5);
        if size < 7 || position + size > bytes.len() {
            break;
        }
        position += size;
        frames += 1;
    }
    if frames > 0 && position == bytes.len() {
        Ok("aac")
    } else {
        bail!("音频分片为空、损坏或下载不完整")
    }
}
