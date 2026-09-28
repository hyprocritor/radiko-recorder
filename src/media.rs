use crate::{
    api::{PermanentError, Radiko, redact},
    model::{JobStatus, RecordingJob},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, watch},
    time::{Instant, sleep},
};
use uuid::Uuid;

#[derive(Clone)]
pub enum StreamSource {
    Radiko(Arc<Radiko>, String),
    Url(String),
}
impl StreamSource {
    async fn resolve(&self) -> Result<String> {
        match self {
            Self::Radiko(api, station) => api.live_url(station).await,
            Self::Url(url) => Ok(url.clone()),
        }
    }
    async fn invalidate(&self) {
        if let Self::Radiko(api, _) = self {
            api.invalidate_auth().await;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Cancel,
    Shutdown,
}

#[derive(Debug)]
pub struct Outcome {
    pub status: JobStatus,
    pub detail: String,
    pub output: Option<PathBuf>,
    pub has_gap: bool,
    pub bytes: u64,
    pub seconds: f64,
}

#[derive(Debug)]
pub enum MediaEvent {
    Status {
        id: Uuid,
        status: JobStatus,
        detail: String,
    },
    Progress {
        id: Uuid,
        seconds: f64,
        bytes: u64,
    },
    Finished {
        id: Uuid,
        outcome: Outcome,
    },
    Preview {
        active: bool,
        detail: String,
    },
}

fn command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command.kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    command
}

pub async fn check_ffmpeg(binary: &Path) -> Result<()> {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        command(binary)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    .context("FFmpeg 检测超时")?
    .context("找不到 FFmpeg；请安装并加入 PATH，或使用 --ffmpeg 指定路径")?;
    if !result.success() {
        bail!("FFmpeg 无法运行");
    }
    Ok(())
}

pub async fn stop_child(child: &mut Child) {
    let pid = child.id();
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(b"q\n").await;
    }
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(exit)) => tracing::info!(?pid, exit = %exit, "FFmpeg 已回收"),
        result => {
            match result {
                Ok(Err(error)) => tracing::error!(?pid, error = %error, "等待 FFmpeg 退出失败"),
                Err(_) => tracing::warn!(?pid, "FFmpeg 未在 5 秒内退出，强制终止"),
                Ok(Ok(_)) => unreachable!(),
            }
            if let Err(error) = child.kill().await {
                tracing::error!(?pid, error = %error, "终止 FFmpeg 失败");
            }
            if let Err(error) = child.wait().await {
                tracing::error!(?pid, error = %error, "回收 FFmpeg 失败");
            }
        }
    }
}

fn input_options(command: &mut Command, url: &str) {
    command.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        // Radiko returns an error body for Range requests on /medialist.
        "-seekable",
        "0",
        "-http_seekable",
        "0",
        "-user_agent",
        "radiko-recorder/0.1",
        "-rw_timeout",
        "15000000",
        "-reconnect",
        "1",
        "-reconnect_streamed",
        "1",
        "-reconnect_on_network_error",
        "1",
        "-reconnect_on_http_error",
        "5xx",
        "-reconnect_delay_max",
        "5",
        "-i",
        url,
    ]);
}

async fn stderr_tail(
    stderr: tokio::process::ChildStderr,
    context: String,
    pid: Option<u32>,
) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut tail = std::collections::VecDeque::new();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::error!(context, ?pid, error = %error, "读取 FFmpeg 错误输出失败");
                tail.push_back(format!("读取 FFmpeg 错误输出失败: {error}"));
                break;
            }
        };
        // Persist every diagnostic now, rather than losing all but the tail on a crash.
        tracing::warn!(context, ?pid, detail = %crate::api::redact_full(&line), "FFmpeg 诊断");
        if tail.len() >= 8 {
            tail.pop_front();
        }
        tail.push_back(redact(&line));
    }
    tail.into_iter().collect::<Vec<_>>().join(" ")
}

async fn delay_or_stop(duration: Duration, stop: &mut watch::Receiver<Option<StopReason>>) -> bool {
    if stop.borrow().is_some() {
        return true;
    }
    tokio::select! { _ = sleep(duration) => false, _ = stop.changed() => true }
}

fn remaining(job: &RecordingJob) -> Duration {
    (job.effective_end() - Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO)
}

async fn resolve_before_deadline(
    source: &StreamSource,
    job: &RecordingJob,
    stop: &mut watch::Receiver<Option<StopReason>>,
) -> Result<Option<String>> {
    if stop.borrow().is_some() || remaining(job).is_zero() {
        return Ok(None);
    }
    tokio::select! {
        result = source.resolve() => result.map(Some),
        _ = stop.changed() => Ok(None),
        _ = sleep(remaining(job)) => Ok(None),
    }
}

fn status(tx: &mpsc::UnboundedSender<MediaEvent>, id: Uuid, state: JobStatus, detail: &str) {
    tracing::info!(job_id = %id, status = ?state, detail, "录制状态更新");
    let _ = tx.send(MediaEvent::Status {
        id,
        status: state,
        detail: redact(detail),
    });
}

/// All subprocesses are owned by this task and reaped before Finished is emitted.
pub async fn record(
    job: RecordingJob,
    binary: PathBuf,
    source: StreamSource,
    tx: mpsc::UnboundedSender<MediaEvent>,
    mut stop: watch::Receiver<Option<StopReason>>,
) {
    tracing::info!(job_id = %job.id, station = %job.program.station_id, start = %job.effective_start(), end = %job.effective_end(), parts_dir = %job.parts_dir().display(), "录制任务启动");
    let mut outcome = Outcome {
        status: JobStatus::Failed,
        detail: String::new(),
        output: job.output.clone(),
        has_gap: job.has_gap,
        bytes: job.bytes,
        seconds: job.recorded_seconds,
    };
    let result = record_inner(&job, &binary, &source, &tx, &mut stop, &mut outcome).await;
    if let Err(error) = result {
        tracing::error!(job_id = %job.id, error = %format!("{error:#}"), "录制任务失败");
        outcome.status = if outcome.bytes > 0 {
            JobStatus::Partial
        } else {
            JobStatus::Failed
        };
        outcome.has_gap = true;
        outcome.detail = redact(&format!(
            "{error:#}；临时文件目录：{}",
            job.parts_dir().display()
        ));
    }
    if matches!(outcome.status, JobStatus::Failed | JobStatus::Partial) {
        tracing::warn!(job_id = %job.id, status = ?outcome.status, detail = %outcome.detail, "录制未完整完成");
    }
    tracing::info!(job_id = %job.id, status = ?outcome.status, detail = %outcome.detail, bytes = outcome.bytes, seconds = outcome.seconds, output = ?outcome.output, "录制任务结束");
    let _ = tx.send(MediaEvent::Finished {
        id: job.id,
        outcome,
    });
}

async fn record_inner(
    job: &RecordingJob,
    binary: &Path,
    source: &StreamSource,
    tx: &mpsc::UnboundedSender<MediaEvent>,
    stop: &mut watch::Receiver<Option<StopReason>>,
    outcome: &mut Outcome,
) -> Result<()> {
    tokio::fs::create_dir_all(job.parts_dir())
        .await
        .context("无法创建录音目录")?;
    let mut parts = existing_parts(&job.parts_dir()).await?;
    outcome.bytes = part_bytes(&parts).await;
    let mut attempt = 0;
    let mut auth_retries = 0;
    let mut failure: Option<String> = None;
    status(tx, job.id, JobStatus::Preparing, "准备认证和直播流");
    while !remaining(job).is_zero() && stop.borrow().is_none() {
        let url = match resolve_before_deadline(source, job, stop).await {
            Ok(Some(url)) => url,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(job_id = %job.id, attempt, permanent = error.downcast_ref::<PermanentError>().is_some(), error = %format!("{error:#}"), "获取直播流失败");
                let message = redact(&format!("{error:#}"));
                if error.downcast_ref::<PermanentError>().is_some() {
                    failure = Some(message);
                    break;
                }
                status(
                    tx,
                    job.id,
                    JobStatus::Preparing,
                    &format!("{message}；稍后重试"),
                );
                if Utc::now() > job.effective_start() {
                    outcome.has_gap = true;
                }
                let wait = backoff(attempt).min(remaining(job));
                attempt += 1;
                if delay_or_stop(wait, stop).await {
                    break;
                }
                continue;
            }
        };
        while Utc::now() < job.effective_start() && stop.borrow().is_none() {
            if delay_or_stop(Duration::from_millis(250), stop).await {
                break;
            }
        }
        if remaining(job).is_zero() || stop.borrow().is_some() {
            break;
        }
        if Utc::now() > job.effective_start() + chrono::Duration::seconds(5) {
            outcome.has_gap = true;
        }
        // A fresh UUID prevents overwriting even empty files left by abrupt termination.
        let part =
            job.parts_dir()
                .join(format!("{:06}_{}.ts", parts.len(), Uuid::new_v4().simple()));
        let mut cmd = command(binary);
        input_options(&mut cmd, &url);
        cmd.args([
            "-map",
            "0:a:0",
            "-vn",
            "-c:a",
            "copy",
            "-f",
            "mpegts",
            "-flush_packets",
            "1",
            "-progress",
            "pipe:1",
            "-n",
        ])
        .arg(&part)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("启动录音 FFmpeg 失败")?;
        tracing::info!(job_id = %job.id, pid = ?child.id(), part = %part.display(), "录音 FFmpeg 已启动");
        let stderr = tokio::spawn(stderr_tail(
            child.stderr.take().context("FFmpeg stderr 不可用")?,
            format!("录制 {}", job.id),
            child.id(),
        ));
        let mut progress =
            BufReader::new(child.stdout.take().context("FFmpeg stdout 不可用")?).lines();
        status(
            tx,
            job.id,
            JobStatus::Recording,
            if outcome.has_gap {
                "正在录制；有缺失内容"
            } else {
                "正在录制"
            },
        );
        let baseline_seconds = outcome.seconds;
        let baseline_bytes = outcome.bytes;
        let mut last_bytes = 0;
        let mut last_activity = Instant::now();
        let mut last_tick = Utc::now();
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        let mut progress_open = true;
        let mut expected_stop = false;
        let mut forced_gap = false;
        let mut exited = None;
        loop {
            tokio::select! {
                _ = stop.changed() => { expected_stop = true; break; }
                line = progress.next_line(), if progress_open => {
                    match line {
                        Ok(Some(line)) => if let Some(value) = line.strip_prefix("out_time_us=") && let Ok(value) = value.parse::<f64>() {
                            let new_seconds = baseline_seconds + value.max(0.0) / 1_000_000.0;
                            if new_seconds > outcome.seconds { last_activity = Instant::now(); }
                            outcome.seconds = new_seconds;
                        },
                        _ => progress_open = false,
                    }
                }
                _ = ticker.tick() => {
                    let now = Utc::now();
                    if (now-last_tick).num_seconds() > 3 { outcome.has_gap = true; }
                    last_tick = now;
                    let bytes = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
                    if bytes > last_bytes { last_bytes = bytes; last_activity = Instant::now(); }
                    outcome.bytes = baseline_bytes + bytes;
                    let _ = tx.send(MediaEvent::Progress { id: job.id, seconds: outcome.seconds, bytes: outcome.bytes });
                    if remaining(job).is_zero() { expected_stop = true; break; }
                    if let Some(exit) = child.try_wait()? { exited = Some(exit); break; }
                    if last_activity.elapsed() > Duration::from_secs(30) { forced_gap = true; break; }
                }
            }
        }
        // Keep draining progress while asking FFmpeg to close its output.
        let drainer =
            tokio::spawn(async move { while let Ok(Some(_)) = progress.next_line().await {} });
        if exited.is_none() {
            stop_child(&mut child).await;
        } else {
            let _ = child.wait().await;
        }
        let tail = stderr.await.unwrap_or_default();
        let _ = drainer.await;
        if tokio::fs::metadata(&part).await.is_ok_and(|m| m.len() > 0) {
            parts.push(part);
        } else {
            let _ = tokio::fs::remove_file(&part).await;
        }
        outcome.bytes = part_bytes(&parts).await;
        let warning = tail.to_ascii_lowercase();
        if warning.contains("http error")
            || warning.contains("failed to reload")
            || warning.contains("will reconnect")
            || warning.contains("failed to open segment")
        {
            outcome.has_gap = true;
        }
        if expected_stop {
            break;
        }
        tracing::warn!(job_id = %job.id, exit = ?exited, stalled = forced_gap, attempt, detail = %tail, "录音流提前结束");
        outcome.has_gap = true;
        if warning.contains("401") || warning.contains("403") {
            if auth_retries >= 1 {
                failure = Some("直播访问被拒绝；重新认证后仍无法收听，请检查地区权限".into());
                break;
            }
            auth_retries += 1;
        }
        if disk_error(&tail) {
            failure = Some(format!("磁盘写入失败：{tail}"));
            break;
        }
        status(
            tx,
            job.id,
            JobStatus::Preparing,
            &format!(
                "{}；重新连接。{tail}",
                if forced_gap {
                    "直播 30 秒无进度"
                } else {
                    "直播连接提前结束"
                }
            ),
        );
        source.invalidate().await;
        let wait = backoff(attempt).min(remaining(job));
        tracing::info!(job_id = %job.id, retry_seconds = wait.as_secs(), "已清除认证缓存，等待重新连接");
        attempt += 1;
        if delay_or_stop(wait, stop).await {
            break;
        }
    }
    if !parts.is_empty() {
        status(
            tx,
            job.id,
            JobStatus::Finalizing,
            "正在封装 M4A；原始片段保留在 .parts 目录",
        );
        outcome.output = Some(finalize(binary, job, &parts).await?);
    }
    let reason = *stop.borrow();
    outcome.status = match reason {
        Some(StopReason::Shutdown) => JobStatus::Interrupted,
        Some(StopReason::Cancel) => JobStatus::Cancelled,
        None if parts.is_empty() => JobStatus::Failed,
        None if outcome.has_gap || failure.is_some() => JobStatus::Partial,
        None => JobStatus::Complete,
    };
    if reason.is_some() {
        outcome.has_gap = true;
    }
    outcome.detail = failure.unwrap_or_else(|| match outcome.status {
        JobStatus::Complete => "录制完成".into(),
        JobStatus::Partial => "音频已保存，但存在缺口；原始片段已保留".into(),
        JobStatus::Cancelled => "预约已取消，已录内容已保留".into(),
        JobStatus::Interrupted => "退出时中断；重启可恢复录制窗口内的任务".into(),
        _ => "未收到音频，请检查网络、地区权限和 FFmpeg".into(),
    });
    Ok(())
}

pub fn backoff(attempt: usize) -> Duration {
    Duration::from_secs([1, 2, 4, 8, 16, 30][attempt.min(5)])
}
fn disk_error(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "no space left",
        "permission denied",
        "error writing",
        "read-only file system",
        "disk full",
    ]
    .iter()
    .any(|s| value.contains(s))
}

async fn existing_parts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().extension().is_some_and(|e| e == "ts") && entry.metadata().await?.len() > 0
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

async fn part_bytes(paths: &[PathBuf]) -> u64 {
    let mut total = 0;
    for path in paths {
        total += tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
    }
    total
}

pub async fn finalize(binary: &Path, job: &RecordingJob, parts: &[PathBuf]) -> Result<PathBuf> {
    let mut list = String::new();
    for part in parts {
        let name = part
            .file_name()
            .context("无效片段文件名")?
            .to_str()
            .context("片段文件名编码无效")?;
        if !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-')
        {
            bail!("片段名称无效");
        }
        list.push_str(&format!("file '{name}'\n"));
    }
    let list_path = job.parts_dir().join("concat.txt");
    tokio::fs::write(&list_path, list).await?;
    let mut output = job.output_dir.join(format!("{}.m4a", job.basename()));
    if output.exists() {
        output = job.output_dir.join(format!(
            "{}_恢复_{}.m4a",
            job.basename(),
            &Uuid::new_v4().simple().to_string()[..8]
        ));
    }
    let temp_output = tempfile::Builder::new()
        .suffix(".m4a")
        .tempfile_in(job.parts_dir())?
        .into_temp_path();
    let mut child = command(binary)
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostats",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_path)
        .args([
            "-map",
            "0:a:0",
            "-c:a",
            "copy",
            "-bsf:a",
            "aac_adtstoasc",
            "-movflags",
            "+faststart",
            "-y",
        ])
        .arg(&temp_output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法启动 M4A 封装")?;
    tracing::info!(job_id = %job.id, pid = ?child.id(), parts = parts.len(), "M4A 封装已启动");
    let stderr = tokio::spawn(stderr_tail(
        child.stderr.take().unwrap(),
        format!("封装 {}", job.id),
        child.id(),
    ));
    let result = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    let success = match result {
        Ok(status) => status?.success(),
        Err(_) => {
            tracing::error!(job_id = %job.id, "M4A 封装超过 60 秒");
            stop_child(&mut child).await;
            false
        }
    };
    let tail = stderr.await.unwrap_or_default();
    if !success {
        bail!("M4A 封装失败；原始 TS 片段已保留。{tail}");
    }
    if tokio::fs::metadata(&temp_output).await?.len() == 0 {
        bail!("M4A 输出为空");
    }
    temp_output
        .persist_noclobber(&output)
        .map_err(|e| anyhow::anyhow!("保存 M4A 失败：{}；原始 TS 片段仍可恢复", e.error))?;
    Ok(output)
}

#[derive(Clone)]
pub struct AudioControl {
    volume: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}
impl Default for AudioControl {
    fn default() -> Self {
        Self {
            volume: Arc::new(AtomicU32::new(0.5_f32.to_bits())),
            muted: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl AudioControl {
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
    pub fn set_volume(&self, value: f32) {
        self.volume
            .store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn toggle_mute(&self) {
        self.muted.fetch_xor(true, Ordering::Relaxed);
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

pub async fn preview(
    binary: PathBuf,
    source: StreamSource,
    tx: mpsc::UnboundedSender<MediaEvent>,
    controls: AudioControl,
    mut stop: watch::Receiver<Option<StopReason>>,
) {
    tracing::info!("试听任务启动");
    let result = preview_inner(&binary, &source, &tx, &controls, &mut stop).await;
    controls.stop();
    if let Err(error) = &result {
        tracing::error!(error = %format!("{error:#}"), "试听失败");
    }
    let detail = result
        .err()
        .map(|e| redact(&format!("试听停止：{e:#}")))
        .unwrap_or_else(|| "试听已停止".into());
    tracing::info!(detail, "试听任务结束");
    let _ = tx.send(MediaEvent::Preview {
        active: false,
        detail,
    });
}

async fn preview_inner(
    binary: &Path,
    source: &StreamSource,
    tx: &mpsc::UnboundedSender<MediaEvent>,
    controls: &AudioControl,
    stop: &mut watch::Receiver<Option<StopReason>>,
) -> Result<()> {
    let url =
        tokio::select! { url = source.resolve() => url?, _ = stop.changed() => return Ok(()) };
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let audio_control = controls.clone();
    let audio_thread = std::thread::Builder::new()
        .name("radiko-audio".into())
        .spawn(move || {
            let stream = match rodio::OutputStreamBuilder::open_default_stream() {
                Ok(mut stream) => {
                    stream.log_on_drop(false);
                    stream
                }
                Err(error) => {
                    tracing::error!(error = %error, "打开音频输出设备失败");
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            let sink = rodio::Sink::connect_new(stream.mixer());
            let _ = ready_tx.send(true);
            while !audio_control.stopped.load(Ordering::Relaxed) {
                sink.set_volume(if audio_control.muted() {
                    0.0
                } else {
                    audio_control.volume()
                });
                if sink.len() >= 8 {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                match audio_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(samples) => {
                        sink.append(rodio::buffer::SamplesBuffer::new(2, 48000, samples))
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
            sink.stop();
        })
        .context("无法启动音频播放线程")?;
    let result: Result<()> = async {
        if !ready_rx.await.unwrap_or(false) { bail!("无法打开音频输出设备；预约录音仍可使用"); }
        if stop.borrow().is_some() { return Ok(()); }
        let mut cmd = command(binary);
        input_options(&mut cmd, &url);
        let mut child = cmd.args(["-map", "0:a:0", "-vn", "-f", "f32le", "-acodec", "pcm_f32le", "-ar", "48000", "-ac", "2", "pipe:1"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("无法启动试听 FFmpeg")?;
        tracing::info!(pid = ?child.id(), "试听 FFmpeg 已启动");
        let stderr = tokio::spawn(stderr_tail(child.stderr.take().unwrap(), "试听".into(), child.id()));
        let mut stdout = child.stdout.take().unwrap();
        let _ = tx.send(MediaEvent::Preview { active: true, detail: "正在试听当前直播".into() });
        let mut bytes = [0u8; 8192];
        let mut carry = Vec::new();
        let mut error = None;
        'stream: loop {
            let read = tokio::select! { read = stdout.read(&mut bytes) => read, _ = stop.changed() => break };
            let count = match read { Ok(0) => { error = Some("试听流已结束".to_string()); break; }, Ok(n) => n, Err(_) => { error = Some("读取试听音频失败".into()); break; } };
            carry.extend_from_slice(&bytes[..count]);
            let aligned = carry.len() / 8 * 8; // full stereo f32 frames only
            let mut samples: Vec<f32> = carry[..aligned].chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
            carry.drain(..aligned);
            loop {
                match audio_tx.try_send(samples) {
                    Ok(_) => break,
                    Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                        samples = returned;
                        if delay_or_stop(Duration::from_millis(20), stop).await { break 'stream; }
                    },
                    Err(_) => { error = Some("音频播放线程已停止".into()); break 'stream; },
                }
            }
        }
        // Drain PCM while asking FFmpeg to quit, otherwise a full stdout pipe can prevent exit.
        let drain = tokio::spawn(async move { let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await; });
        stop_child(&mut child).await;
        let tail = stderr.await.unwrap_or_default();
        let _ = drain.await;
        if let Some(error) = error { bail!("{error}。{tail}"); }
        Ok(())
    }.await;
    controls.stop();
    drop(audio_tx);
    match tokio::task::spawn_blocking(move || audio_thread.join()).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::error!("音频播放线程 panic；原始原因及堆栈见 crash.log");
            return result.and(Err(anyhow::anyhow!("音频播放线程异常，详情见 crash.log")));
        }
        Err(error) => {
            tracing::error!(error = %error, "回收音频播放线程失败");
            return result.and(Err(
                anyhow::Error::new(error).context("回收音频播放线程失败")
            ));
        }
    }
    result
}
