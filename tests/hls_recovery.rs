use axum::{
    Router,
    extract::{Path as RoutePath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use chrono::{Duration as CDuration, Utc};
use radiko_recorder::{
    hls,
    media::{self, MediaEvent, StopReason, StreamSource},
    model::{JobStatus, Program, RecordingJob, RecoveryState},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

#[test]
fn timeline_parser_handles_midnight_relative_urls_and_discontinuities() {
    let base = reqwest::Url::parse("https://example.invalid/live/media?session=test").unwrap();
    let text = "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:99\n#EXT-X-PROGRAM-DATE-TIME:2026-09-28T23:59:58+09:00\n#EXTINF:5.035,\na.aac\n#EXTINF:5.035,\n../b.aac\n";
    let list = hls::parse(text, &base).unwrap().unwrap();
    assert_eq!(list.segments.len(), 2);
    assert_eq!(
        (list.segments[1].start - list.segments[0].start).num_milliseconds(),
        5035
    );
    assert_eq!(list.segments[1].url.path(), "/b.aac");
    assert!(
        hls::parse("#EXTM3U\n#EXTINF:5,\na.aac", &base)
            .unwrap()
            .is_none()
    );
    assert!(
        hls::parse("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"", &base)
            .unwrap()
            .is_none()
    );
    assert!(hls::audio_extension(b"ERROR\n").is_err());
    assert!(hls::audio_extension(&[0xff, 0xf1, 0, 0, 9, 0, 0]).is_err());
}

#[derive(Clone)]
struct Server {
    audio: Arc<Vec<u8>>,
    origin: chrono::DateTime<Utc>,
    clock: Instant,
    requests: Arc<Mutex<HashMap<u64, usize>>>,
    permanent: bool,
}

async fn playlist(State(state): State<Server>) -> axum::response::Response {
    let elapsed = state.clock.elapsed().as_secs_f64();
    if (2.5..4.5).contains(&elapsed) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let head = (elapsed / 1.024) as u64 + 2;
    let low = head.saturating_sub(10);
    let mut text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{low}\n");
    for number in low..head {
        let time = state.origin + CDuration::milliseconds(number as i64 * 1024);
        text.push_str(&format!(
            "#EXT-X-PROGRAM-DATE-TIME:{}\n#EXTINF:1.024,\n/segment/{number}.aac\n",
            time.to_rfc3339()
        ));
    }
    ([("content-type", "application/vnd.apple.mpegurl")], text).into_response()
}

async fn segment(
    State(state): State<Server>,
    RoutePath(name): RoutePath<String>,
) -> axum::response::Response {
    let number: u64 = name.trim_end_matches(".aac").parse().unwrap();
    let mut requests = state.requests.lock().unwrap();
    let count = requests.entry(number).or_default();
    *count += 1;
    if number == 2 && (*count <= 2 || state.permanent) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if (2.5..4.5).contains(&state.clock.elapsed().as_secs_f64()) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if number == 3 && *count == 1 {
        return b"truncated download".to_vec().into_response();
    }
    (
        [("content-type", "audio/aac")],
        state.audio.as_ref().clone(),
    )
        .into_response()
}

async fn check_capture(permanent: bool) {
    let binary =
        PathBuf::from(std::env::var_os("RADIKO_TEST_FFMPEG").expect("set RADIKO_TEST_FFMPEG"));
    let sample = tokio::process::Command::new(&binary)
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
        .output()
        .await
        .unwrap();
    assert!(sample.status.success());
    assert_eq!(hls::audio_extension(&sample.stdout).unwrap(), "aac");
    let dir = tempfile::tempdir().unwrap();
    let state = Server {
        audio: Arc::new(sample.stdout),
        origin: Utc::now() - CDuration::milliseconds(2048),
        clock: Instant::now(),
        requests: Arc::new(Mutex::new(HashMap::new())),
        permanent,
    };
    let requests = state.requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = StreamSource::Url(format!("http://{}/live", listener.local_addr().unwrap()));
    let app = Router::new()
        .route("/live", get(playlist))
        .route("/segment/{number}", get(segment))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let program = Program {
        id: "synthetic".into(),
        station_id: "TEST".into(),
        station_name: "测试".into(),
        title: "短暂断网补片".into(),
        performer: String::new(),
        description: String::new(),
        start: state.origin,
        end: Utc::now() + CDuration::seconds(10),
    };
    let mut job = RecordingJob::new(program, dir.path().to_path_buf());
    job.pre_seconds = 0;
    job.post_seconds = 0;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = watch::channel(None);
    let worker = tokio::spawn(media::record(
        job.clone(),
        binary.clone(),
        source,
        tx,
        stop_rx,
    ));
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = rx.recv().await {
            if let MediaEvent::Finished { outcome, .. } = event {
                return outcome;
            }
        }
        panic!("no completion event");
    })
    .await
    .unwrap();
    worker.await.unwrap();
    server.abort();
    assert!(
        requests.lock().unwrap()[&2] >= 3,
        "failed segment should be retried"
    );
    assert!(outcome.issues.iter().any(|i| i.exact && i.attempts > 0));
    assert!(
        outcome
            .issues
            .iter()
            .any(|i| !i.exact && i.state == RecoveryState::Recovered),
        "playlist outage recovered"
    );
    if permanent {
        assert_eq!(outcome.status, JobStatus::Partial, "{}", outcome.detail);
        assert!(
            outcome
                .issues
                .iter()
                .any(|i| i.exact && i.state == RecoveryState::Unavailable)
        );
    } else {
        assert_eq!(outcome.status, JobStatus::Complete, "{:?}", outcome.issues);
        assert!(!outcome.has_gap);
        assert!(
            outcome
                .issues
                .iter()
                .all(|i| i.state == RecoveryState::Recovered)
        );
    }
    let index_text = std::fs::read_to_string(job.parts_dir().join("segments.json")).unwrap();
    assert!(!index_text.contains("http://"));
    let index: serde_json::Value = serde_json::from_str(&index_text).unwrap();
    let entries = index["segments"].as_array().unwrap();
    let times: Vec<_> = entries
        .iter()
        .map(|e| e["start"].as_str().unwrap())
        .collect();
    assert!(
        times.windows(2).all(|s| s[0] < s[1]),
        "recovered audio must be ordered without duplicates"
    );
    let decoded = tokio::process::Command::new(&binary)
        .args(["-v", "error", "-i"])
        .arg(outcome.output.as_ref().unwrap())
        .args(["-f", "null", "-"])
        .output()
        .await
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    // Reload the index and finalize again without duplicating or overwriting segments/audio.
    let mut resume = job.clone();
    resume.end = Utc::now() - CDuration::seconds(1);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = watch::channel(Some(StopReason::Shutdown));
    let task = tokio::spawn(media::record(
        resume,
        binary,
        StreamSource::Url("http://127.0.0.1:1/unreachable".into()),
        tx,
        stop_rx,
    ));
    while let Some(event) = rx.recv().await {
        if let MediaEvent::Finished {
            outcome: resumed, ..
        } = event
        {
            assert!(resumed.output.is_some());
            assert_ne!(resumed.output, outcome.output);
            break;
        }
    }
    task.await.unwrap();
}

#[tokio::test]
#[ignore = "requires FFmpeg; uses only local HLS"]
async fn short_outage_and_failed_segments_are_recovered_in_order() {
    check_capture(false).await;
}

#[tokio::test]
#[ignore = "requires FFmpeg; uses only local HLS"]
async fn unavailable_segment_keeps_its_exact_time_range() {
    check_capture(true).await;
}

#[tokio::test]
#[ignore = "requires FFmpeg and audio output; muted local HLS only"]
async fn preview_reconnects_after_404_and_remains_cancellable() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let binary = PathBuf::from(std::env::var_os("RADIKO_TEST_FFMPEG").unwrap());
    let sample = tokio::process::Command::new(&binary)
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
        .output()
        .await
        .unwrap();
    assert!(sample.status.success());
    let state = Server {
        audio: Arc::new(sample.stdout),
        origin: Utc::now() - CDuration::milliseconds(2048),
        clock: Instant::now(),
        requests: Arc::new(Mutex::new(HashMap::new())),
        permanent: false,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let router = Router::new()
        .route(
            "/flaky",
            get(move |State(state): State<Server>| {
                let calls = handler_calls.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        StatusCode::NOT_FOUND.into_response()
                    } else {
                        playlist(State(state)).await
                    }
                }
            }),
        )
        .route("/segment/{number}", get(segment))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = StreamSource::Url(format!("http://{}/flaky", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = watch::channel(None);
    let controls = media::AudioControl::default();
    controls.set_volume(0.0);
    let worker = tokio::spawn(media::preview(
        binary,
        source,
        tx,
        controls.clone(),
        stop_rx,
    ));
    let mut retry = false;
    let mut recovered = false;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = rx.recv().await {
            match event {
                MediaEvent::Issues { id: None, issues } => {
                    retry |= issues.iter().any(|i| i.state == RecoveryState::Retrying);
                    recovered |= issues.iter().any(|i| i.state == RecoveryState::Recovered);
                }
                MediaEvent::Preview {
                    active: true,
                    detail,
                } if recovered && detail.contains("正在试听") => break,
                MediaEvent::Preview {
                    active: false,
                    detail,
                } => panic!("preview unexpectedly stopped: {detail}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(retry && recovered);
    assert_eq!(controls.volume(), 0.0);
    let stopped = Instant::now();
    stop.send(Some(StopReason::Cancel)).unwrap();
    tokio::time::timeout(Duration::from_secs(8), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(stopped.elapsed() < Duration::from_secs(8));
    assert!(calls.load(Ordering::SeqCst) >= 2);
    server.abort();
}
