use radiko_recorder::diagnostics::{self, Diagnostics};
use std::{fs, process::Command};

// Run crash cases in separate processes so their hooks/subscribers and failure
// exit codes cannot interfere with the parent test runner.
#[test]
#[ignore = "subprocess helper, invoked by the tests below"]
fn diagnostic_child() {
    let directory = std::env::var_os("RADIKO_DIAGNOSTIC_TEST_DIR").unwrap();
    let logger = Diagnostics::install(std::path::Path::new(&directory)).unwrap();
    tracing::info!("diagnostic-child-ready");
    match std::env::var("RADIKO_DIAGNOSTIC_TEST_CASE")
        .unwrap()
        .as_str()
    {
        "panic" => panic!(
            "synthetic panic token=synthetic-secret https://example.invalid/?lsid=fake-session"
        ),
        "worker" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("synthetic-runtime")
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let error = tokio::spawn(async {
                    diagnostics::guard_task("synthetic-recording-job", async {
                        panic!("synthetic worker panic");
                    })
                    .await
                })
                .await
                .unwrap()
                .unwrap_err();
                assert!(error.contains("synthetic worker panic"));
            });
        }
        "thread" => {
            assert!(
                std::thread::Builder::new()
                    .name("synthetic-audio-thread".into())
                    .spawn(|| panic!("synthetic audio panic"))
                    .unwrap()
                    .join()
                    .is_err()
            );
        }
        "fatal" => {
            let error = anyhow::anyhow!(
                "root-cause-marker {} tail-marker token=synthetic-secret",
                "x".repeat(4000)
            )
            .context("outer-context-marker");
            diagnostics::report_error("synthetic fatal error", &error);
            // Proves logging doesn't depend on a destructor/async queue draining.
            std::process::exit(23);
        }
        "normal" => {
            tracing::warn!(
                "token=synthetic-secret X-Radiko-Authtoken: synthetic-header https://example.invalid/?lsid=fake-session"
            );
            tracing::info!("normal-exit-marker");
            logger.flush().unwrap();
        }
        _ => panic!("unexpected diagnostic case"),
    }
}

fn child(case: &str) -> (String, String, std::process::ExitStatus) {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "diagnostic_child", "--nocapture"])
        .env("RADIKO_DIAGNOSTIC_TEST_DIR", dir.path())
        .env("RADIKO_DIAGNOSTIC_TEST_CASE", case)
        .env_remove("RUST_BACKTRACE")
        .output()
        .unwrap();
    let log = fs::read_to_string(dir.path().join("recorder.log")).unwrap();
    let crash = fs::read_to_string(dir.path().join("crash.log")).unwrap();
    assert!(log.contains("diagnostic-child-ready"));
    for text in [&log, &crash] {
        assert!(!text.contains("synthetic-secret"), "credential leaked");
        assert!(!text.contains("synthetic-header"), "header leaked");
        assert!(!text.contains("fake-session"), "session URL leaked");
    }
    (log, crash, output.status)
}

#[test]
fn panic_records_location_and_stack_without_rust_backtrace() {
    let (log, crash, status) = child("panic");
    assert!(!status.success());
    for text in [&log, &crash] {
        assert!(text.contains("PANIC"));
        assert!(text.contains("synthetic panic"));
        assert!(text.contains("diagnostics.rs:"));
        assert!(text.contains("Backtrace:"));
        assert!(text.contains("diagnostic_child"));
        assert!(text.contains("run="));
        assert!(text.contains("pid="));
    }
}

#[test]
fn background_panic_is_logged_and_notifies_caller() {
    let (log, crash, status) = child("worker");
    assert!(status.success());
    assert!(crash.contains("synthetic worker panic"));
    assert!(crash.contains("synthetic-runtime"));
    assert!(log.contains("synthetic-recording-job"));
}

#[test]
fn audio_thread_panic_is_logged() {
    let (_, crash, status) = child("thread");
    assert!(status.success());
    assert!(crash.contains("synthetic audio panic"));
    assert!(crash.contains("synthetic-audio-thread"));
}

#[test]
fn fatal_error_is_not_truncated_and_survives_immediate_exit() {
    let (log, crash, status) = child("fatal");
    assert_eq!(status.code(), Some(23));
    for text in [&log, &crash] {
        assert!(text.contains("outer-context-marker"));
        assert!(text.contains("root-cause-marker"));
        assert!(text.contains("tail-marker"));
        assert!(text.contains("Backtrace:"));
    }
}

#[test]
fn normal_logs_are_redacted_and_flushed() {
    let (log, crash, status) = child("normal");
    assert!(status.success());
    assert!(log.contains("normal-exit-marker"));
    assert!(log.contains("[已隐藏网络凭证/地址]"));
    assert!(crash.is_empty());
}

#[test]
fn binary_logs_startup_errors_before_opening_store_or_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_radiko-recorder"))
        .arg("invalid/station")
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap()
        .status;
    assert!(!status.success());
    let crash = fs::read_to_string(dir.path().join("crash.log")).unwrap();
    assert!(crash.contains("电台 ID 无效"));
    assert!(crash.contains("程序异常退出"));
    assert!(crash.contains("Backtrace:"));
}

#[test]
fn binary_logs_store_lock_failure() {
    let dir = tempfile::tempdir().unwrap();
    let _store = radiko_recorder::store::Store::open(dir.path()).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_radiko-recorder"))
        .arg("JORF")
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap()
        .status;
    assert!(!status.success());
    let crash = fs::read_to_string(dir.path().join("crash.log")).unwrap();
    assert!(crash.contains("该数据目录正被另一实例使用"));
}

#[test]
fn binary_uses_temporary_logs_when_data_directory_is_unusable() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("blocked-data");
    fs::write(&blocked, "a file cannot be used as a directory").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_radiko-recorder"))
        .arg("JORF")
        .arg("--data-dir")
        .arg(&blocked)
        .env("TMP", dir.path())
        .env("TEMP", dir.path())
        .env("TMPDIR", dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let crash = fs::read_to_string(dir.path().join("radiko-recorder-logs/crash.log")).unwrap();
    assert!(crash.contains("LOG_INIT_ERROR"));
    assert!(crash.contains("打开预约数据目录失败"));
    assert_eq!(
        fs::read_to_string(blocked).unwrap(),
        "a file cannot be used as a directory"
    );
}

#[test]
fn logs_are_preserved_across_process_restarts() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "diagnostic_child"])
            .env("RADIKO_DIAGNOSTIC_TEST_DIR", dir.path())
            .env("RADIKO_DIAGNOSTIC_TEST_CASE", "normal")
            .output()
            .unwrap()
            .status;
        assert!(status.success());
    }
    let log = fs::read_to_string(dir.path().join("recorder.log")).unwrap();
    assert_eq!(log.matches("normal-exit-marker").count(), 2);
}
