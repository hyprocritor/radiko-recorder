//! Record HLS segments individually so failed downloads can be retried without
//! losing newly arriving audio. URLs are held in memory only; the durable index
//! contains media times and local filenames, and orders recovered segments once.
use crate::{
    api::{PermanentError, redact},
    hls::{self, Segment},
    media::{self, MediaEvent, Outcome, StopReason, StreamSource},
    model::{JobStatus, RecordingJob, RecoveryState, StreamIssue},
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::{StreamExt, stream};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    file: String,
    bytes: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Index {
    segments: Vec<Saved>,
    issues: Vec<StreamIssue>,
}

struct Pending {
    segment: Segment,
    attempts: usize,
    next_try: Instant,
}

#[derive(Debug)]
struct HttpError(StatusCode);
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}
impl std::error::Error for HttpError {}

async fn fetch(client: &Client, url: &Url, limit: usize) -> Result<Vec<u8>> {
    let mut response = client.get(url.clone()).send().await.map_err(|e| {
        anyhow::anyhow!(
            "网络请求失败: {}",
            if e.is_timeout() {
                "超时"
            } else {
                "连接中断"
            }
        )
    })?;
    if !response.status().is_success() {
        bail!(HttpError(response.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取网络响应中断")? {
        if bytes.len() + chunk.len() > limit {
            bail!("直播响应超过大小限制");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn equivalent(saved: &Saved, segment: &Segment) -> bool {
    (saved.start - segment.start).num_milliseconds().abs() <= 20
        && (saved.end - segment.end).num_milliseconds().abs() <= 20
}

fn covered(segments: &[Saved], start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    let mut cursor = start;
    for part in segments {
        if part.start > cursor + ChronoDuration::milliseconds(20) {
            break;
        }
        cursor = cursor.max(part.end);
        if cursor + ChronoDuration::milliseconds(20) >= end {
            return true;
        }
    }
    false
}

fn holes(
    segments: &[Saved],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut cursor = start;
    let mut gaps = Vec::new();
    for part in segments {
        if part.end <= cursor || part.start >= end {
            continue;
        }
        if part.start > cursor + ChronoDuration::milliseconds(20) {
            gaps.push((cursor, part.start.min(end)));
        }
        cursor = cursor.max(part.end);
    }
    if cursor + ChronoDuration::milliseconds(20) < end {
        gaps.push((cursor, end));
    }
    gaps
}

fn refresh_issues(index: &mut Index, job: &RecordingJob, observed_end: DateTime<Utc>) {
    index.segments.sort_by_key(|s| s.start);
    for issue in &mut index.issues {
        if issue.exact
            && issue.state != RecoveryState::Recovered
            && let Some(end) = issue.end
            && covered(
                &index.segments,
                issue.start.max(job.effective_start()),
                end.min(job.effective_end()),
            )
        {
            issue.state = RecoveryState::Recovered;
            tracing::info!(job_id = %job.id, start = %issue.start, end = %end, "缺口分片已补回");
        }
    }
    for (start, end) in holes(
        &index.segments,
        job.effective_start(),
        observed_end.min(job.effective_end()),
    ) {
        if !index.issues.iter().any(|i| {
            i.exact
                && i.state != RecoveryState::Recovered
                && i.start <= start
                && i.end.is_some_and(|e| e >= end)
        }) {
            index.issues.push(StreamIssue::new(
                start,
                Some(end),
                true,
                "该时间段音频尚未取得，正在检查缓存分片和新清单",
            ));
        }
    }
}

fn publish(
    index: &Index,
    job: &RecordingJob,
    tx: &mpsc::UnboundedSender<MediaEvent>,
    outcome: &mut Outcome,
) {
    outcome.issues = index.issues.clone();
    outcome.has_gap = index
        .issues
        .iter()
        .any(|i| i.state != RecoveryState::Recovered);
    outcome.bytes = index.segments.iter().map(|s| s.bytes).sum();
    outcome.seconds = index
        .segments
        .iter()
        .map(|s| (s.end - s.start).num_microseconds().unwrap_or_default() as f64 / 1_000_000.0)
        .sum();
    let _ = tx.send(MediaEvent::Issues {
        id: Some(job.id),
        issues: outcome.issues.clone(),
    });
    let _ = tx.send(MediaEvent::Progress {
        id: job.id,
        bytes: outcome.bytes,
        seconds: outcome.seconds,
    });
}

async fn persist(index: &Index, dir: &Path) -> Result<()> {
    let index = index.clone();
    let path = dir.join("segments.json");
    tokio::task::spawn_blocking(move || crate::store::atomic_json(&path, &index))
        .await
        .context("保存分片索引的任务异常")??;
    Ok(())
}

async fn save_segment(dir: &Path, segment: &Segment, bytes: &[u8]) -> Result<Saved> {
    let extension = hls::audio_extension(bytes)?;
    let file = format!(
        "hls_{}_{}.{}",
        segment.start.timestamp_micros(),
        segment.end.timestamp_micros(),
        extension
    );
    let path = dir.join(&file);
    if path.exists() {
        let existing = tokio::fs::read(&path).await.context("读取已保存分片失败")?;
        if hls::audio_extension(&existing).is_ok() {
            return Ok(Saved {
                start: segment.start,
                end: segment.end,
                file,
                bytes: existing.len() as u64,
            });
        }
        // Preserve a damaged existing file for diagnosis before replacing it.
        tokio::fs::rename(
            &path,
            dir.join(format!(
                "damaged_{}_{}",
                uuid::Uuid::new_v4().simple(),
                file
            )),
        )
        .await
        .context("保留损坏分片失败")?;
    }
    // A deterministic name can only represent this timestamp range. A partial
    // HTTP response never reaches the final filename or durable index.
    let temp = dir.join(format!("{}.download", uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&temp, bytes)
        .await
        .context("写入录音分片失败")?;
    let handle = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&temp)
        .await?;
    handle.sync_all().await?;
    drop(handle);
    tokio::fs::rename(&temp, &path)
        .await
        .context("保存录音分片失败")?;
    Ok(Saved {
        start: segment.start,
        end: segment.end,
        file,
        bytes: bytes.len() as u64,
    })
}

async fn load(job: &RecordingJob) -> Result<Index> {
    let path = job.parts_dir().join("segments.json");
    let mut index = match tokio::fs::read(path).await {
        Ok(bytes) => {
            serde_json::from_slice::<Index>(&bytes).context("分片索引损坏，已保留原文件")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Index {
            issues: job.issues.clone(),
            ..Index::default()
        },
        Err(e) => return Err(e.into()),
    };
    for part in &index.segments {
        if !part.file.starts_with("hls_")
            || !part
                .file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_- .".contains(&b))
            || part.end <= part.start
        {
            bail!("分片索引包含无效记录");
        }
    }
    // Metadata is committed after audio. Recover an atomically downloaded file
    // even if the process stopped between the file rename and index save.
    let mut entries = tokio::fs::read_dir(job.parts_dir()).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if index.segments.iter().any(|s| s.file == name) {
            continue;
        }
        let Some(stem) = name
            .strip_prefix("hls_")
            .and_then(|s| s.strip_suffix(".aac").or_else(|| s.strip_suffix(".ts")))
        else {
            continue;
        };
        let Some((start, end)) = stem.split_once('_') else {
            continue;
        };
        let (Some(start), Some(end)) = (
            start.parse().ok().and_then(DateTime::from_timestamp_micros),
            end.parse().ok().and_then(DateTime::from_timestamp_micros),
        ) else {
            continue;
        };
        let bytes = tokio::fs::read(entry.path()).await?;
        if end > start && hls::audio_extension(&bytes).is_ok() {
            index.segments.push(Saved {
                start,
                end,
                file: name,
                bytes: bytes.len() as u64,
            });
        }
    }
    let mut existing = Vec::new();
    for part in index.segments {
        if tokio::fs::metadata(job.parts_dir().join(&part.file))
            .await
            .is_ok_and(|m| m.len() == part.bytes && m.len() > 0)
        {
            existing.push(part);
        }
    }
    index.segments = existing;
    index.segments.sort_by_key(|s| s.start);
    Ok(index)
}

/// Returns false only for streams without a supported absolute segment timeline.
pub(crate) async fn run(
    job: &RecordingJob,
    binary: &Path,
    source: &StreamSource,
    tx: &mpsc::UnboundedSender<MediaEvent>,
    stop: &mut watch::Receiver<Option<StopReason>>,
    outcome: &mut Outcome,
) -> Result<bool> {
    tokio::fs::create_dir_all(job.parts_dir())
        .await
        .context("无法创建录音目录")?;
    // Existing FFmpeg recordings have no exact per-segment timeline; keep their recovery path.
    let legacy = media::existing_parts(&job.parts_dir())
        .await?
        .iter()
        .any(|p| {
            !p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("hls_")
        });
    if legacy {
        return Ok(false);
    }
    let mut index = load(job).await?;
    let client = Client::builder()
        .user_agent(concat!("radiko-recorder/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(4))
        .build()?;
    let mut url: Option<Url> = None;
    let mut pending = BTreeMap::<i64, Pending>::new();
    let mut observed_end = index
        .segments
        .last()
        .map(|s| s.end)
        .unwrap_or(job.effective_start());
    let mut network_issue = None;
    let mut stalled_issue = false;
    let mut failures = 0;
    let mut denied = 0;
    let mut last_advance = Instant::now();
    let mut fatal = None;
    media::status(tx, job.id, JobStatus::Preparing, "准备直播分片及恢复索引");
    while !media::remaining(job).is_zero() && stop.borrow().is_none() {
        let fetched: Result<Option<hls::Playlist>> = tokio::select! {
            result = async {
                if url.is_none() { url = Some(Url::parse(&source.resolve().await?)?); }
                let media_url = url.as_ref().context("缺少直播地址")?;
                let body = fetch(&client, media_url, 1024 * 1024).await?;
                hls::parse(std::str::from_utf8(&body).context("直播清单编码无效")?, media_url)
            } => result,
            _ = stop.changed() => break,
            _ = tokio::time::sleep(media::remaining(job)) => break,
        };
        let wait;
        match fetched {
            Ok(None) => {
                if index.segments.is_empty() {
                    tracing::info!(job_id = %job.id, "使用 FFmpeg 兼容录制；清单不支持按节目时间补片");
                    return Ok(false);
                }
                fatal = Some(anyhow::anyhow!("直播格式发生变化，已保留已下载分片"));
                break;
            }
            Ok(Some(playlist)) => {
                failures = 0;
                denied = 0;
                let latest = playlist
                    .segments
                    .last()
                    .unwrap()
                    .end
                    .min(job.effective_end());
                if (!stalled_issue || latest > observed_end)
                    && let Some(id) = network_issue.take()
                    && let Some(issue) = index.issues.iter_mut().find(|i| i.id == id)
                {
                    issue.end = Some(Utc::now());
                    issue.state = RecoveryState::Recovered;
                    stalled_issue = false;
                }
                wait = Duration::from_secs_f64((playlist.target_seconds / 2.0).clamp(0.5, 5.0));
                if latest > observed_end {
                    observed_end = latest;
                    last_advance = Instant::now();
                }
                for segment in playlist.segments {
                    if segment.end <= job.effective_start()
                        || segment.start >= job.effective_end()
                        || index.segments.iter().any(|s| equivalent(s, &segment))
                    {
                        continue;
                    }
                    let key = pending
                        .iter()
                        .find(|(_, entry)| {
                            (entry.segment.start - segment.start)
                                .num_milliseconds()
                                .abs()
                                <= 20
                                && (entry.segment.end - segment.end).num_milliseconds().abs() <= 20
                        })
                        .map(|(key, _)| *key)
                        .unwrap_or_else(|| segment.start.timestamp_micros());
                    if let Some(old) = pending.get_mut(&key) {
                        if old.segment.url != segment.url {
                            old.next_try = Instant::now();
                        }
                        old.segment.url = segment.url;
                    } else {
                        pending.insert(
                            key,
                            Pending {
                                segment,
                                attempts: 0,
                                next_try: Instant::now(),
                            },
                        );
                    }
                }
                if (playlist.ended || last_advance.elapsed() > Duration::from_secs(30))
                    && network_issue.is_none()
                {
                    url = None;
                    source.invalidate().await;
                    let mut issue = StreamIssue::new(
                        Utc::now()
                            - ChronoDuration::from_std(last_advance.elapsed()).unwrap_or_default(),
                        None,
                        false,
                        "直播清单停止更新，正在重新建立会话",
                    );
                    issue.attempts = 1;
                    network_issue = Some(issue.id);
                    stalled_issue = true;
                    index.issues.push(issue);
                    last_advance = Instant::now();
                }
            }
            Err(error) => {
                let reason = redact(&format!("直播清单读取失败：{error:#}"));
                tracing::warn!(job_id = %job.id, error = %reason, "正在恢复直播会话");
                if error.downcast_ref::<PermanentError>().is_some() {
                    fatal = Some(error);
                    break;
                }
                let code = error.downcast_ref::<HttpError>().map(|e| e.0);
                if matches!(code, Some(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)) {
                    denied += 1;
                } else {
                    denied = 0;
                }
                if denied >= 2 {
                    fatal = Some(anyhow::anyhow!("重新认证后直播仍拒绝访问"));
                    break;
                }
                if matches!(
                    code,
                    Some(
                        StatusCode::UNAUTHORIZED
                            | StatusCode::FORBIDDEN
                            | StatusCode::NOT_FOUND
                            | StatusCode::GONE
                    )
                ) {
                    url = None;
                    source.invalidate().await;
                }
                if let Some(id) = network_issue
                    && let Some(issue) = index.issues.iter_mut().find(|i| i.id == id)
                {
                    issue.reason = reason;
                    issue.attempts += 1;
                } else {
                    let mut issue = StreamIssue::new(Utc::now(), None, false, reason);
                    issue.attempts = 1;
                    network_issue = Some(issue.id);
                    index.issues.push(issue);
                }
                wait = media::backoff(failures);
                failures += 1;
            }
        }

        // Reserve slots for fresh audio even if older segments are failing.
        let eligible: Vec<_> = pending
            .iter()
            .filter(|(_, p)| p.next_try <= Instant::now())
            .map(|(k, p)| (*k, p.segment.clone()))
            .collect();
        let mut selected: Vec<_> = eligible.iter().rev().take(4).cloned().collect();
        for entry in eligible.iter().take(4) {
            if !selected.iter().any(|e| e.0 == entry.0) {
                selected.push(entry.clone());
            }
        }
        let downloads = stream::iter(selected.into_iter().map(|(key, segment)| {
            let client = &client;
            async move {
                let result = fetch(client, &segment.url, 16 * 1024 * 1024)
                    .await
                    .and_then(|bytes| {
                        hls::audio_extension(&bytes)?;
                        Ok(bytes)
                    });
                (key, segment, result)
            }
        }))
        .buffer_unordered(4);
        tokio::pin!(downloads);
        loop {
            let next = tokio::select! { value = downloads.next() => value, _ = stop.changed() => break, _ = tokio::time::sleep(media::remaining(job)) => break };
            let Some((key, segment, result)) = next else {
                break;
            };
            match result {
                Ok(bytes) => {
                    if index
                        .segments
                        .iter()
                        .any(|saved| equivalent(saved, &segment))
                    {
                        pending.remove(&key);
                        continue;
                    }
                    if index.segments.iter().any(|saved| {
                        saved.start < segment.end - ChronoDuration::milliseconds(20)
                            && saved.end > segment.start + ChronoDuration::milliseconds(20)
                    }) {
                        if !index
                            .issues
                            .iter()
                            .any(|i| i.start == segment.start && i.end == Some(segment.end))
                        {
                            let mut issue = StreamIssue::new(
                                segment.start,
                                Some(segment.end),
                                true,
                                "新会话分片边界与已有音频重叠；保留已有分片，避免重复拼接，重叠部分暂未补回",
                            );
                            issue.state = RecoveryState::Unavailable;
                            index.issues.push(issue);
                        }
                        pending.remove(&key);
                        continue;
                    }
                    let saved = save_segment(&job.parts_dir(), &segment, &bytes).await?;
                    index.segments.push(saved);
                    pending.remove(&key);
                }
                Err(error) => {
                    let entry = pending.get_mut(&key).unwrap();
                    entry.attempts += 1;
                    entry.next_try = Instant::now() + media::backoff(entry.attempts - 1);
                    let reason = redact(&format!(
                        "分片下载失败：{error:#}；将重试原地址并检查新会话"
                    ));
                    if let Some(issue) = index.issues.iter_mut().find(|i| {
                        i.exact
                            && i.start == segment.start
                            && i.end == Some(segment.end)
                            && i.state != RecoveryState::Recovered
                    }) {
                        issue.reason = reason;
                        issue.attempts = entry.attempts as u32;
                    } else {
                        let mut issue =
                            StreamIssue::new(segment.start, Some(segment.end), true, reason);
                        issue.attempts = entry.attempts as u32;
                        index.issues.push(issue);
                    }
                    // Refresh signed/session URLs while keeping the original failed URI in memory.
                    if error.downcast_ref::<HttpError>().is_some() {
                        url = None;
                    }
                }
            }
        }
        // Retain at most 256 failed URLs. Their time ranges remain in the durable report.
        while pending.len() > 256 {
            pending.pop_first();
        }
        refresh_issues(&mut index, job, observed_end);
        publish(&index, job, tx, outcome);
        persist(&index, &job.parts_dir()).await?;
        media::status(
            tx,
            job.id,
            JobStatus::Recording,
            if outcome.has_gap {
                "正在录制；正在补取缺口 · e 查看时间段"
            } else {
                "正在录制；已取得的分片连续 · e 查看恢复记录"
            },
        );
        if media::delay_or_stop(wait.min(media::remaining(job)), stop).await {
            break;
        }
    }
    refresh_issues(&mut index, job, observed_end);
    if let Some(error) = &fatal {
        let mut issue = StreamIssue::new(
            Utc::now(),
            Some(Utc::now()),
            false,
            redact(&format!("{error:#}")),
        );
        issue.state = RecoveryState::Unavailable;
        index.issues.push(issue);
    }
    for issue in &mut index.issues {
        if issue.state == RecoveryState::Retrying {
            issue.state = if issue.exact {
                RecoveryState::Unavailable
            } else {
                RecoveryState::Suspected
            };
            if issue.end.is_none() {
                issue.end = Some(Utc::now().min(job.effective_end()));
            }
        }
    }
    persist(&index, &job.parts_dir()).await?;
    publish(&index, job, tx, outcome);
    if !index.segments.is_empty() {
        media::status(
            tx,
            job.id,
            JobStatus::Finalizing,
            "按分片时间顺序封装 M4A（包含已补回分片）",
        );
        let parts: Vec<PathBuf> = index
            .segments
            .iter()
            .map(|s| job.parts_dir().join(&s.file))
            .collect();
        outcome.output = Some(media::finalize(binary, job, &parts).await?);
    }
    outcome.status = match *stop.borrow() {
        Some(StopReason::Cancel) => JobStatus::Cancelled,
        Some(StopReason::Shutdown) => JobStatus::Interrupted,
        None if index.segments.is_empty() => JobStatus::Failed,
        None if outcome.has_gap || fatal.is_some() => JobStatus::Partial,
        None => JobStatus::Complete,
    };
    outcome.detail = match fatal {
        Some(error) => redact(&format!("{error:#}；已录内容保留")),
        None => match outcome.status {
            JobStatus::Complete => "录制完成；已取得的分片连续，重试结果见 e".into(),
            JobStatus::Partial => "音频已保存；仍有未补回或待确认的时间段，按 e 查看".into(),
            JobStatus::Interrupted => "退出时中断；分片索引已保存，重启可继续恢复".into(),
            JobStatus::Cancelled => "用户取消；已录内容及恢复记录已保存".into(),
            _ => "未取得音频；按 e 查看失败时间及原因".into(),
        },
    };
    Ok(true)
}
