use anyhow::{Context, Result};
use clap::Parser;
use radiko_recorder::{
    api::{Radiko, redact},
    diagnostics::{self, Diagnostics},
    media,
    model::validate_station,
    store::Store,
    ui::{self, Config},
};
use std::{
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

#[derive(Parser)]
#[command(version, about = "Radiko 节目表、直播试听与定时录音（需保持前台运行）")]
struct Args {
    /// 电台 ID，例如 JORF、RN2；省略时在 TUI 中输入
    station: Option<String>,
    /// 录音输出目录（默认 ./recordings）
    #[arg(long, default_value = "recordings")]
    output_dir: PathBuf,
    /// 预约与日志目录；默认为用户应用数据目录
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// FFmpeg 可执行文件路径
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "radiko-recorder")
        .map(|p| p.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".radiko-recorder"))
}

fn main() -> ExitCode {
    let parsed = Args::try_parse();
    if let Err(error) = &parsed
        && !error.use_stderr()
    {
        let _ = error.print();
        return ExitCode::SUCCESS;
    }
    let data_dir = parsed
        .as_ref()
        .ok()
        .and_then(|a| a.data_dir.clone())
        .unwrap_or_else(default_data_dir);
    let logger = match Diagnostics::install(&data_dir) {
        Ok(logger) => logger,
        Err(error) => {
            let fallback = std::env::temp_dir().join("radiko-recorder-logs");
            match Diagnostics::install(&fallback) {
                Ok(logger) => {
                    logger.report(
                        "LOG_INIT_ERROR",
                        &format!(
                            "无法使用日志目录 {}，改用 {}\n错误链: {error:#}",
                            data_dir.display(),
                            fallback.display()
                        ),
                    );
                    logger
                }
                Err(fallback_error) => {
                    eprintln!(
                        "无法初始化日志: {}；备用目录也不可写: {}",
                        redact(&format!("{error:#}")),
                        redact(&format!("{fallback_error:#}"))
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, arch = std::env::consts::ARCH, data_dir = %data_dir.display(), log_dir = %logger.directory().display(), "程序启动");
    // The hook is installed before both the store and runtime are initialized.
    let result = panic::catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        let args = parsed.context("命令行参数错误")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("radiko-worker")
            .enable_all()
            .build()
            .context("无法启动异步运行时")?;
        runtime.block_on(run(args, data_dir))
    }));
    let exit = match result {
        Ok(Ok(())) => {
            tracing::info!("程序正常退出");
            ExitCode::SUCCESS
        }
        Ok(Err(error)) => {
            diagnostics::report_error("程序异常退出", &error);
            eprintln!(
                "程序异常退出: {}\n日志目录: {}",
                redact(&format!("{error:#}")),
                logger.directory().display()
            );
            ExitCode::FAILURE
        }
        Err(_) => {
            // The hook already recorded the original panic stack before unwinding.
            tracing::error!("主线程 panic，程序退出；原始原因及堆栈见 crash.log");
            eprintln!("程序发生 panic。日志目录: {}", logger.directory().display());
            ExitCode::FAILURE
        }
    };
    if let Err(error) = logger.flush() {
        logger.report("LOG_FLUSH_ERROR", &format!("退出时同步日志失败: {error}"));
    }
    exit
}

async fn run(args: Args, data_dir: PathBuf) -> Result<()> {
    let station = args
        .station
        .as_deref()
        .map(validate_station)
        .transpose()
        .context("电台 ID 无效")?;
    let cwd = std::env::current_dir().context("无法读取工作目录")?;
    let output_dir = if args.output_dir.is_absolute() {
        args.output_dir
    } else {
        cwd.join(args.output_dir)
    };
    let store = Arc::new(Store::open(&data_dir).context("打开预约数据目录失败")?);
    let mut ffmpeg = if args.ffmpeg.components().count() > 1 && !args.ffmpeg.is_absolute() {
        cwd.join(args.ffmpeg)
    } else {
        args.ffmpeg
    };
    let mut ffmpeg_error = media::check_ffmpeg(&ffmpeg)
        .await
        .err()
        .map(|e| e.to_string());
    if ffmpeg_error.is_some() && ffmpeg == std::path::Path::new("ffmpeg") {
        let local = cwd.join("tools/bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if local.is_file() {
            ffmpeg_error = media::check_ffmpeg(&local)
                .await
                .err()
                .map(|e| e.to_string());
            ffmpeg = local;
        }
    }
    if let Some(error) = &ffmpeg_error {
        tracing::warn!(error = %error, ffmpeg = %ffmpeg.display(), "FFmpeg 检查失败，录制与试听不可用");
    } else {
        tracing::info!(ffmpeg = %ffmpeg.display(), "FFmpeg 检查完成");
    }
    tracing::info!(station = ?station, output_dir = %output_dir.display(), "准备启动界面");
    let api = Arc::new(Radiko::new().context("无法建立 HTTP 客户端")?);
    ui::run(
        Config {
            station,
            output_dir,
            ffmpeg,
            ffmpeg_error,
        },
        api,
        store,
    )
    .await
}
