use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use chrono::{Duration as ChronoDuration, Utc};
use radiko_recorder::{
    media::{self, MediaEvent, Outcome, StopReason, StreamSource},
    model::{JobStatus, Program, RecordingJob},
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};

fn ffmpeg() -> PathBuf {
    std::env::var_os("RADIKO_TEST_FFMPEG")
        .expect("Set RADIKO_TEST_FFMPEG to the FFmpeg executable")
        .into()
}

#[derive(Clone)]
struct Hls {
    audio: Arc<Vec<u8>>,
    start: Instant,
    finite_once: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
}

async fn playlist(State(state): State<Hls>, headers: HeaderMap) -> axum::response::Response {
    // Match the real service's response to FFmpeg's default Range header.
    if headers.contains_key("range") {
        return "ERROR\n".into_response();
    }
    state.requests.fetch_add(1, Ordering::SeqCst);
    let finite = state.finite_once.swap(false, Ordering::SeqCst);
    let head = state.start.elapsed().as_secs() + 3;
    let mut body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{}\n",
        head - 3
    );
    for index in head - 3..head {
        body.push_str(&format!("#EXTINF:1.024,\n/segment/{index}.aac\n"));
    }
    if finite {
        body.push_str("#EXT-X-ENDLIST\n");
    }
    ([("content-type", "application/vnd.apple.mpegurl")], body).into_response()
}

fn job(dir: &Path, seconds: i64) -> RecordingJob {
    let start = Utc::now();
    let mut job = RecordingJob::new(
        Program {
            id: "synthetic".into(),
            station_id: "TEST".into(),
            station_name: "合成音频".into(),
            title: "テスト / 测试".into(),
            performer: String::new(),
            description: String::new(),
            start,
            end: start + ChronoDuration::seconds(seconds),
        },
        dir.to_path_buf(),
    );
    job.pre_seconds = 0;
    job.post_seconds = 0;
    job
}

async fn outcome(rx: &mut mpsc::UnboundedReceiver<MediaEvent>) -> Outcome {
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            if let Some(MediaEvent::Finished { outcome, .. }) = rx.recv().await {
                return outcome;
            }
        }
    })
    .await
    .expect("recording did not terminate")
}

async fn decode_check(binary: &Path, path: &Path) {
    let output = Command::new(binary)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires FFmpeg; uses only a local synthetic HLS server"]
async fn ffmpeg_recording_reconnect_cancel_recovery_and_failed_mux() {
    let binary = ffmpeg();
    media::check_ffmpeg(&binary).await.unwrap();
    let sample = Command::new(&binary)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-frames:a",
            "48",
            "-c:a",
            "aac",
            "-f",
            "adts",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(sample.status.success());
    let finite_once = Arc::new(AtomicBool::new(false));
    let state = Hls {
        audio: Arc::new(sample.stdout),
        start: Instant::now(),
        finite_once: finite_once.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/playlist.m3u8", get(playlist))
        .route(
            "/segment/{name}",
            get(|State(state): State<Hls>, _: HeaderMap| async move {
                (
                    [("content-type", "audio/aac")],
                    state.audio.as_ref().clone(),
                )
            }),
        )
        .route(
            "/denied.m3u8",
            get(|| async { StatusCode::FORBIDDEN.into_response() }),
        )
        .with_state(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let source = StreamSource::Url(format!("{base}/playlist.m3u8"));

    let normal_job = job(dir.path(), 8);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = watch::channel(None);
    let started = Instant::now();
    let worker = tokio::spawn(media::record(
        normal_job.clone(),
        binary.clone(),
        source.clone(),
        tx,
        stop_rx,
    ));
    let normal = outcome(&mut rx).await;
    worker.await.unwrap();
    assert_eq!(normal.status, JobStatus::Complete, "{}", normal.detail);
    assert!(started.elapsed() < Duration::from_secs(16));
    assert!(normal.bytes > 1000 && normal.seconds > 0.0);
    decode_check(&binary, normal.output.as_ref().unwrap()).await;

    finite_once.store(true, Ordering::SeqCst);
    let retry_job = job(dir.path(), 12);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = watch::channel(None);
    let worker = tokio::spawn(media::record(
        retry_job.clone(),
        binary.clone(),
        source.clone(),
        tx,
        stop_rx,
    ));
    let retry = outcome(&mut rx).await;
    worker.await.unwrap();
    assert_eq!(retry.status, JobStatus::Partial, "{}", retry.detail);
    let parts: Vec<_> = std::fs::read_dir(retry_job.parts_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|s| s == "ts"))
        .collect();
    assert!(
        parts.len() >= 2,
        "an early EOF must restart into a new segment"
    );
    decode_check(&binary, retry.output.as_ref().unwrap()).await;

    let cancel_job = job(dir.path(), 60);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = watch::channel(None);
    let worker = tokio::spawn(media::record(
        cancel_job.clone(),
        binary.clone(),
        source.clone(),
        tx,
        stop_rx,
    ));
    tokio::time::timeout(Duration::from_secs(12), async {
        while let Some(event) = rx.recv().await {
            if matches!(event,MediaEvent::Progress { bytes,.. } if bytes>1000) {
                break;
            }
        }
    })
    .await
    .unwrap();
    stop.send(Some(StopReason::Shutdown)).unwrap();
    let interrupted = outcome(&mut rx).await;
    worker.await.unwrap();
    assert_eq!(interrupted.status, JobStatus::Interrupted);
    let first_output = interrupted.output.clone().unwrap();
    let first_bytes = std::fs::read(&first_output).unwrap();
    let mut resumed = cancel_job.clone();
    resumed.end = Utc::now() + ChronoDuration::seconds(6);
    resumed.has_gap = true;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = watch::channel(None);
    let worker = tokio::spawn(media::record(resumed, binary.clone(), source, tx, stop_rx));
    tokio::time::sleep(Duration::from_secs(3)).await;
    stop.send(Some(StopReason::Cancel)).unwrap();
    let cancelled = outcome(&mut rx).await;
    worker.await.unwrap();
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert_ne!(cancelled.output.as_ref().unwrap(), &first_output);
    assert_eq!(
        std::fs::read(&first_output).unwrap(),
        first_bytes,
        "never overwrite earlier output"
    );
    decode_check(&binary, cancelled.output.as_ref().unwrap()).await;

    let bad_job = job(dir.path(), 30);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = watch::channel(None);
    let worker = tokio::spawn(media::record(
        bad_job,
        binary.clone(),
        StreamSource::Url(format!("{base}/denied.m3u8")),
        tx,
        stop_rx,
    ));
    let denied = outcome(&mut rx).await;
    worker.await.unwrap();
    assert_eq!(denied.status, JobStatus::Failed);
    assert!(denied.detail.contains("拒绝"));

    let bad_mux = job(dir.path(), 1);
    std::fs::create_dir_all(bad_mux.parts_dir()).unwrap();
    let invalid = bad_mux.parts_dir().join("000000_invalid.ts");
    std::fs::write(&invalid, b"not an audio stream").unwrap();
    assert!(
        media::finalize(&binary, &bad_mux, std::slice::from_ref(&invalid))
            .await
            .is_err()
    );
    assert!(
        invalid.exists(),
        "a failed mux must preserve source fragments"
    );
    server.abort();
}
