//! Synchronous file logging. Fatal reports do not depend on the async runtime or
//! the tracing writer's mutex, so a panic during shutdown can still be recorded.
use crate::api::redact_full;
use anyhow::{Context, Result};
use chrono::Local;
use futures_util::FutureExt;
use std::{
    backtrace::Backtrace,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Write},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, TryLockError,
        atomic::{AtomicU64, Ordering},
    },
};
use tracing_subscriber::fmt::MakeWriter;

static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();
static REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct Diagnostics(Arc<LogFiles>);

struct LogFiles {
    recorder: Mutex<File>,
    // A separate, pre-opened append handle: never acquire a mutex in the panic path.
    crash: File,
    directory: PathBuf,
    run_id: String,
}

impl Diagnostics {
    /// Call before opening the job store or creating the Tokio runtime.
    pub fn install(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory).context("无法创建日志目录")?;
        let open = |name| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join(name))
        };
        let logger = Self(Arc::new(LogFiles {
            recorder: Mutex::new(open("recorder.log").context("无法打开运行日志")?),
            crash: open("crash.log").context("无法打开崩溃日志")?,
            directory: directory.to_path_buf(),
            run_id: uuid::Uuid::new_v4().simple().to_string(),
        }));
        // Compile the redactor before installing the panic hook.
        let _ = redact_full("");
        let panic_logger = logger.clone();
        panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let location = info
                .location()
                .map(|p| format!("{}:{}:{}", p.file(), p.line(), p.column()))
                .unwrap_or_else(|| "未知位置".into());
            let message = panic_message(info.payload());
            panic_logger.report(
                "PANIC",
                &format!(
                    "线程: {} ({:?})\n位置: {location}\n原因: {message}",
                    thread.name().unwrap_or("unnamed"),
                    thread.id(),
                ),
            );
        }));
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_writer(logger.clone())
            .with_max_level(tracing::Level::INFO)
            .try_init()
            .map_err(|error| anyhow::anyhow!("无法初始化日志: {error}"))?;
        let _ = DIAGNOSTICS.set(logger.clone());
        Ok(logger)
    }

    pub fn directory(&self) -> &Path {
        &self.0.directory
    }

    /// Force a stack capture even when RUST_BACKTRACE is unset (e.g. double-click).
    /// Write the emergency report before touching the regular logger.
    pub fn report(&self, kind: &str, reason: &str) {
        let sequence = REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let report = redact_full(&format!(
            "\n{} {kind} version={} pid={} run={} report={sequence}\n{reason}\n",
            Local::now().to_rfc3339(),
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            self.0.run_id,
        ));
        // Save the reason first; symbol lookup can be slow or fail in a damaged
        // process. Matching run/report IDs associate concurrent panic stacks.
        self.append_report(&report);
        self.append_report(&redact_full(&format!(
            "run={} report={sequence} Backtrace:\n{}\n--- end {kind} report={sequence} ---\n",
            self.0.run_id,
            Backtrace::force_capture(),
        )));
    }

    fn append_report(&self, report: &str) {
        let mut crash = &self.0.crash;
        if let Err(error) = crash
            .write_all(report.as_bytes())
            .and_then(|_| crash.flush())
            .and_then(|_| crash.sync_all())
        {
            eprintln!("无法写入 crash.log: {error}\n{report}");
        }
        // A panic may occur while the same thread owns this mutex. Never block.
        let file = match self.0.recorder.try_lock() {
            Ok(file) => Some(file),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(mut file) = file
            && let Err(error) = file
                .write_all(report.as_bytes())
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all())
        {
            eprintln!("无法写入 recorder.log: {error}");
        }
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut file = self.0.recorder.lock().unwrap_or_else(|e| e.into_inner());
        file.flush()?;
        file.sync_all()?;
        self.0.crash.sync_all()
    }
}

pub fn report_error(context: &str, error: &anyhow::Error) {
    if let Some(logger) = DIAGNOSTICS.get() {
        logger.report("ERROR", &format!("{context}\n错误链: {error:#}"));
    } else {
        tracing::error!(context, error = %redact_full(&format!("{error:#}")), "程序错误");
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("非字符串 panic；请查看堆栈")
}

/// Tokio catches panics in tasks; also notify their caller so the UI can stop
/// waiting for a completion event. The global hook has already saved the stack.
pub async fn guard_task<T>(name: &str, future: impl Future<Output = T>) -> Result<T, String> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|payload| {
            let error = redact_full(&format!(
                "{name} 异常: {}；详情见 crash.log",
                panic_message(&*payload)
            ));
            tracing::error!(task = name, error, "后台任务 panic");
            error
        })
}

pub struct LogWriter {
    logger: Diagnostics,
    buffer: Vec<u8>,
}

impl<'a> MakeWriter<'a> for Diagnostics {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            logger: self.clone(),
            buffer: Vec::new(),
        }
    }
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // Redact the complete event, including credentials split across writes.
        let message = redact_full(&String::from_utf8_lossy(&self.buffer));
        let message = message.trim_end_matches('\n');
        self.buffer.clear();
        let mut file = self
            .logger
            .0
            .recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        writeln!(
            file,
            "pid={} run={} {message}",
            std::process::id(),
            self.logger.0.run_id
        )?;
        file.flush()
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            self.logger
                .report("LOG_WRITE_ERROR", &format!("写入运行日志失败: {error}"));
        }
    }
}
