use crate::{
    api::{Radiko, redact},
    diagnostics::{self, guard_task},
    media::{self, AudioControl, MediaEvent, Outcome, StopReason, StreamSource},
    model::{JobStatus, Program, RecordingJob, jst, parse_editor_time, validate_station},
    scheduler::{self, Decision},
    store::Store,
};
use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate, Utc};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::HashMap,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};
use uuid::Uuid;

pub struct Config {
    pub station: Option<String>,
    pub output_dir: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffmpeg_error: Option<String>,
}
enum AppEvent {
    Schedule {
        station: String,
        result: Result<Vec<Program>, String>,
    },
}
struct Worker {
    stop: watch::Sender<Option<StopReason>>,
    handle: JoinHandle<()>,
}
struct Editor {
    job: RecordingJob,
    existing: bool,
    fields: [String; 4],
    selected: usize,
    error: String,
}

struct App {
    config: Config,
    station: String,
    station_input: String,
    programs: Vec<Program>,
    jobs: Vec<RecordingJob>,
    date: Option<NaiveDate>,
    program_state: TableState,
    job_state: TableState,
    focus: u8,
    detail_scroll: u16,
    message: String,
    loading: bool,
    editor: Option<Editor>,
    confirm_exit: bool,
    shutting_down: bool,
    workers: HashMap<Uuid, Worker>,
    preview: Option<Worker>,
    preview_active: bool,
    audio: AudioControl,
    last_refresh: Instant,
    last_save: Instant,
    dirty: bool,
}

impl App {
    fn new(config: Config, jobs: Vec<RecordingJob>) -> Self {
        Self {
            station: config.station.clone().unwrap_or_default(),
            station_input: String::new(),
            message: config
                .ffmpeg_error
                .clone()
                .unwrap_or_else(|| "选择节目后按 Enter 预约，p 试听当前直播".into()),
            config,
            programs: Vec::new(),
            jobs,
            date: None,
            program_state: TableState::default().with_selected(0),
            job_state: TableState::default().with_selected(0),
            focus: 0,
            detail_scroll: 0,
            loading: false,
            editor: None,
            confirm_exit: false,
            shutting_down: false,
            workers: HashMap::new(),
            preview: None,
            preview_active: false,
            audio: AudioControl::default(),
            last_refresh: Instant::now(),
            last_save: Instant::now(),
            dirty: false,
        }
    }

    fn dates(&self) -> Vec<NaiveDate> {
        let now = Utc::now();
        let mut dates: Vec<_> = self
            .programs
            .iter()
            .filter(|p| p.end > now)
            .map(|p| p.start.with_timezone(&jst()).date_naive())
            .collect();
        dates.sort();
        dates.dedup();
        dates
    }

    fn visible_programs(&self) -> Vec<&Program> {
        let now = Utc::now();
        self.programs
            .iter()
            .filter(|p| {
                p.end > now && Some(p.start.with_timezone(&jst()).date_naive()) == self.date
            })
            .collect()
    }

    fn visible_jobs(&self) -> Vec<usize> {
        self.jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| j.program.station_id == self.station)
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_program(&self) -> Option<&Program> {
        self.visible_programs()
            .get(self.program_state.selected().unwrap_or(0))
            .copied()
    }

    fn request_schedule(&mut self, api: Arc<Radiko>, tx: mpsc::UnboundedSender<AppEvent>) {
        if self.loading || self.station.is_empty() {
            return;
        }
        self.loading = true;
        self.last_refresh = Instant::now();
        let station = self.station.clone();
        tokio::spawn(async move {
            tracing::info!(station, "请求节目表");
            let result = match guard_task("节目表请求", api.schedule(&station)).await {
                Ok(Ok(programs)) => {
                    tracing::info!(station, programs = programs.len(), "节目表请求成功");
                    Ok(programs)
                }
                Ok(Err(error)) => {
                    tracing::error!(station, error = %format!("{error:#}"), "节目表请求失败");
                    Err(redact(&format!("{error:#}")))
                }
                Err(error) => Err(error),
            };
            let _ = tx.send(AppEvent::Schedule { station, result });
        });
    }

    fn apply_schedule(&mut self, result: Result<Vec<Program>, String>) {
        self.loading = false;
        match result {
            Ok(programs) => {
                for job in &mut self.jobs {
                    if let Some(program) = programs
                        .iter()
                        .find(|p| p.id == job.program.id && p.station_id == job.program.station_id)
                    {
                        job.schedule_changed = program.start != job.program.start
                            || program.end != job.program.end
                            || program.title != job.program.title;
                    }
                }
                self.programs = programs;
                let dates = self.dates();
                if self.date.is_none_or(|date| !dates.contains(&date)) {
                    self.date = dates.first().copied();
                    self.program_state.select(Some(0));
                }
                self.message = format!("节目表已更新 · {} 个可选日期 · JST (UTC+9)", dates.len());
                self.dirty = true;
            }
            Err(error) => self.message = format!("节目表获取失败：{error}；r 重试，s 修改电台 ID"),
        }
    }

    fn begin_shutdown(&mut self) {
        if !self.shutting_down {
            tracing::info!(
                recordings = self.workers.len(),
                preview = self.preview.is_some(),
                "开始退出清理"
            );
        }
        self.shutting_down = true;
        self.confirm_exit = false;
        self.message = "正在停止试听和录音、封装已有音频，请稍候…".into();
        for worker in self.workers.values() {
            let _ = worker.stop.send(Some(StopReason::Shutdown));
        }
        if let Some(worker) = &self.preview {
            let _ = worker.stop.send(Some(StopReason::Shutdown));
        }
    }

    fn begin_editor(&mut self) {
        if let Some(error) = &self.config.ffmpeg_error {
            self.message = error.clone();
            return;
        }
        let selected = if self.focus == 2 {
            self.visible_jobs()
                .get(self.job_state.selected().unwrap_or(0))
                .map(|&i| (self.jobs[i].clone(), true))
        } else {
            self.selected_program().map(|p| {
                (
                    RecordingJob::new(p.clone(), self.config.output_dir.clone()),
                    false,
                )
            })
        };
        if let Some((job, existing)) = selected {
            if existing && job.status != JobStatus::Scheduled {
                self.message = "只能编辑尚未准备录制的预约".into();
                return;
            }
            let fields = [
                job.start
                    .with_timezone(&jst())
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
                job.end
                    .with_timezone(&jst())
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
                job.pre_seconds.to_string(),
                job.post_seconds.to_string(),
            ];
            self.editor = Some(Editor {
                job,
                existing,
                fields,
                selected: 0,
                error: String::new(),
            });
        }
    }

    fn edit_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.editor = None;
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => editor.selected = (editor.selected + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => editor.selected = (editor.selected + 3) % 4,
            KeyCode::Backspace => {
                editor.fields[editor.selected].pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                editor.fields[editor.selected].clear()
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.save_editor()
            }
            KeyCode::Enter => {
                if editor.selected < 3 {
                    editor.selected += 1;
                } else {
                    self.save_editor();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && (c.is_ascii_digit() || "-: ".contains(c)) =>
            {
                if editor.fields[editor.selected].len() < 32 {
                    editor.fields[editor.selected].push(c);
                }
            }
            _ => {}
        }
    }

    fn save_editor(&mut self) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let result: Result<()> = (|| {
            editor.job.start = parse_editor_time(&editor.fields[0])?;
            editor.job.end = parse_editor_time(&editor.fields[1])?;
            editor.job.pre_seconds = editor.fields[2]
                .parse()
                .map_err(|_| anyhow::anyhow!("提前秒数必须是整数"))?;
            editor.job.post_seconds = editor.fields[3]
                .parse()
                .map_err(|_| anyhow::anyhow!("延后秒数必须是整数"))?;
            editor.job.validate(Utc::now())?;
            if editor.job.effective_start() < Utc::now() {
                editor.job.has_gap = true;
                editor.job.detail = "将立即录制剩余内容，无法补录已播出部分".into();
            }
            Ok(())
        })();
        if let Err(error) = result {
            editor.error = error.to_string();
            return;
        }
        let editor = self.editor.take().unwrap();
        self.message = if editor.job.has_gap {
            "预约已保存：将立即录制，已播出部分无法补录".into()
        } else {
            "预约已保存；请保持程序开启".into()
        };
        if editor.existing {
            if let Some(job) = self.jobs.iter_mut().find(|j| j.id == editor.job.id) {
                *job = editor.job;
            }
        } else {
            self.jobs.push(editor.job);
        }
        self.dirty = true;
    }

    fn key(
        &mut self,
        key: KeyEvent,
        api: &Arc<Radiko>,
        app_tx: &mpsc::UnboundedSender<AppEvent>,
        media_tx: &mpsc::UnboundedSender<MediaEvent>,
    ) {
        if key.kind == KeyEventKind::Release || self.shutting_down {
            return;
        }
        if self.confirm_exit {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.begin_shutdown(),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.confirm_exit = false,
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_exit();
            return;
        }
        if self.station.is_empty() {
            match key.code {
                KeyCode::Enter => match validate_station(&self.station_input) {
                    Ok(station) => {
                        self.station = station;
                        self.request_schedule(api.clone(), app_tx.clone());
                    }
                    Err(e) => self.message = e.to_string(),
                },
                KeyCode::Backspace => {
                    self.station_input.pop();
                }
                KeyCode::Esc => self.request_exit(),
                KeyCode::Char(c) if c.is_ascii_alphanumeric() || c == '-' || c == '_' => {
                    if self.station_input.len() < 32 {
                        self.station_input.push(c);
                    }
                }
                _ => {}
            }
            return;
        }
        if self.editor.is_some() {
            self.edit_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.request_exit(),
            KeyCode::Char('r') => self.request_schedule(api.clone(), app_tx.clone()),
            KeyCode::Tab => self.focus = (self.focus + 1) % 3,
            KeyCode::BackTab => self.focus = (self.focus + 2) % 3,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => {
                if self.focus == 1 {
                    self.detail_scroll = self.detail_scroll.saturating_add(8);
                } else {
                    self.move_selection(8);
                }
            }
            KeyCode::PageUp => {
                if self.focus == 1 {
                    self.detail_scroll = self.detail_scroll.saturating_sub(8);
                } else {
                    self.move_selection(-8);
                }
            }
            KeyCode::Left | KeyCode::Right => {
                let dates = self.dates();
                if !dates.is_empty() {
                    let at = dates
                        .iter()
                        .position(|d| Some(*d) == self.date)
                        .unwrap_or(0) as isize;
                    let next = (at + if key.code == KeyCode::Left { -1 } else { 1 })
                        .clamp(0, dates.len() as isize - 1);
                    self.date = Some(dates[next as usize]);
                    self.program_state.select(Some(0));
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Enter => self.begin_editor(),
            KeyCode::Char('x') => self.cancel_selected(),
            KeyCode::Char('s')
                if self.workers.is_empty()
                    && self.preview.is_none()
                    && !self
                        .jobs
                        .iter()
                        .any(|j| j.program.station_id == self.station && j.status.pending()) =>
            {
                self.station_input = self.station.clone();
                self.station.clear();
                self.loading = false;
                self.programs.clear();
                self.date = None;
            }
            KeyCode::Char('p') => self.toggle_preview(api, media_tx),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.audio.set_volume(self.audio.volume() + 0.05)
            }
            KeyCode::Char('-') => self.audio.set_volume(self.audio.volume() - 0.05),
            KeyCode::Char('m') => self.audio.toggle_mute(),
            _ => {}
        }
    }

    fn request_exit(&mut self) {
        if self.jobs.iter().any(|j| j.status.pending()) || !self.workers.is_empty() {
            self.confirm_exit = true;
        } else {
            self.begin_shutdown();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.focus == 1 {
            self.detail_scroll = self.detail_scroll.saturating_add_signed(delta as i16);
            return;
        }
        let count = if self.focus == 0 {
            self.visible_programs().len()
        } else {
            self.visible_jobs().len()
        };
        if count == 0 {
            return;
        }
        let state = if self.focus == 0 {
            &mut self.program_state
        } else {
            &mut self.job_state
        };
        state.select(Some(
            (state.selected().unwrap_or(0) as isize + delta).clamp(0, count as isize - 1) as usize,
        ));
        if self.focus == 0 {
            self.detail_scroll = 0;
        }
    }

    fn cancel_selected(&mut self) {
        let Some(&index) = self
            .visible_jobs()
            .get(self.job_state.selected().unwrap_or(0))
        else {
            return;
        };
        let job = &mut self.jobs[index];
        if !job.status.pending() {
            return;
        }
        if let Some(worker) = self.workers.get(&job.id) {
            let _ = worker.stop.send(Some(StopReason::Cancel));
            self.message = "正在停止并保存已录音频…".into();
        } else {
            job.status = JobStatus::Cancelled;
            job.detail = "用户取消预约".into();
            self.dirty = true;
        }
    }

    fn toggle_preview(&mut self, api: &Arc<Radiko>, tx: &mpsc::UnboundedSender<MediaEvent>) {
        if let Some(worker) = &self.preview {
            let _ = worker.stop.send(Some(StopReason::Cancel));
            return;
        }
        if let Some(error) = &self.config.ffmpeg_error {
            self.message = error.clone();
            return;
        }
        let audio = AudioControl::default();
        audio.set_volume(self.audio.volume());
        if self.audio.muted() {
            audio.toggle_mute();
        }
        self.audio = audio;
        let (stop, rx) = watch::channel(None);
        let future = media::preview(
            self.config.ffmpeg.clone(),
            StreamSource::Radiko(api.clone(), self.station.clone()),
            tx.clone(),
            self.audio.clone(),
            rx,
        );
        let tx = tx.clone();
        let audio = self.audio.clone();
        let handle = tokio::spawn(async move {
            if let Err(error) = guard_task("直播试听", future).await {
                audio.stop();
                let _ = tx.send(MediaEvent::Preview {
                    active: false,
                    detail: error,
                });
            }
        });
        self.preview = Some(Worker { stop, handle });
        self.message = "正在准备试听…".into();
    }

    fn launch_due(&mut self, api: &Arc<Radiko>, tx: &mpsc::UnboundedSender<MediaEvent>) {
        if self.shutting_down || self.station.is_empty() {
            return;
        }
        let now = Utc::now();
        for job in &mut self.jobs {
            if job.program.station_id != self.station || self.workers.contains_key(&job.id) {
                continue;
            }
            match scheduler::decide(job, now) {
                Decision::Wait => {}
                Decision::Missed => {
                    tracing::warn!(job_id = %job.id, "预约窗口已错过");
                    job.status = JobStatus::Missed;
                    job.detail = "录制窗口已结束".into();
                    self.dirty = true;
                }
                Decision::Prepare => {
                    if let Some(error) = &self.config.ffmpeg_error {
                        job.detail = error.clone();
                        continue;
                    }
                    job.status = JobStatus::Preparing;
                    self.dirty = true;
                    let (stop, rx) = watch::channel(None);
                    let future = media::record(
                        job.clone(),
                        self.config.ffmpeg.clone(),
                        StreamSource::Radiko(api.clone(), self.station.clone()),
                        tx.clone(),
                        rx,
                    );
                    let backup = job.clone();
                    let tx = tx.clone();
                    let handle = tokio::spawn(async move {
                        if let Err(error) =
                            guard_task(&format!("录制任务 {}", backup.id), future).await
                        {
                            let _ = tx.send(MediaEvent::Finished {
                                id: backup.id,
                                outcome: Outcome {
                                    status: JobStatus::Failed,
                                    detail: format!(
                                        "{error}；原始片段保留在 {}",
                                        backup.parts_dir().display()
                                    ),
                                    output: backup.output,
                                    has_gap: true,
                                    bytes: backup.bytes,
                                    seconds: backup.recorded_seconds,
                                },
                            });
                        }
                    });
                    self.workers.insert(job.id, Worker { stop, handle });
                }
            }
        }
    }

    async fn media_event(&mut self, event: MediaEvent) {
        match event {
            MediaEvent::Status { id, status, detail } => {
                if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                    job.status = status;
                    job.detail = detail;
                    self.dirty = true;
                }
            }
            MediaEvent::Progress { id, seconds, bytes } => {
                if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                    job.recorded_seconds = seconds;
                    job.bytes = bytes;
                    self.dirty = true;
                }
            }
            MediaEvent::Finished { id, outcome } => {
                if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                    job.status = outcome.status;
                    job.detail = outcome.detail;
                    job.output = outcome.output;
                    job.has_gap = outcome.has_gap;
                    job.bytes = outcome.bytes;
                    job.recorded_seconds = outcome.seconds;
                    self.message = format!("{}：{}", job.program.title, job.detail);
                    self.dirty = true;
                }
                if let Some(worker) = self.workers.remove(&id)
                    && let Err(error) = worker.handle.await
                {
                    tracing::error!(job_id = %id, error = %error, "回收录制任务失败");
                }
            }
            MediaEvent::Preview { active, detail } => {
                self.preview_active = active;
                self.message = detail;
                if !active
                    && let Some(worker) = self.preview.take()
                    && let Err(error) = worker.handle.await
                {
                    tracing::error!(error = %error, "回收试听任务失败");
                }
            }
        }
    }

    async fn save(&mut self, store: &Arc<Store>) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let store = store.clone();
        let jobs = self.jobs.clone();
        tokio::task::spawn_blocking(move || store.save(&jobs))
            .await
            .context("保存预约的后台任务异常")?
            .context("保存预约状态失败")?;
        self.dirty = false;
        self.last_save = Instant::now();
        Ok(())
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(error) = terminal::disable_raw_mode() {
            tracing::warn!(error = %error, "恢复终端输入模式失败");
        }
        if let Err(error) = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show) {
            tracing::warn!(error = %error, "恢复终端画面失败");
        }
    }
}

pub async fn run(config: Config, api: Arc<Radiko>, store: Arc<Store>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("请在交互式终端运行，例如 Windows Terminal / PowerShell");
    }
    let mut jobs = store.load().context("加载预约失败")?;
    scheduler::recover(&mut jobs, Utc::now());
    store.save(&jobs).context("保存恢复后的预约失败")?;
    tracing::info!(jobs = jobs.len(), "预约加载与恢复完成");
    let mut app = App::new(config, jobs);
    terminal::enable_raw_mode().context("启用终端原始输入模式失败")?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen).context("进入终端全屏模式失败")?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("初始化终端界面失败")?;
    terminal.clear().context("清空终端画面失败")?;
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    let (media_tx, mut media_rx) = mpsc::unbounded_channel();
    app.request_schedule(api.clone(), app_tx.clone());
    let mut keys = EventStream::new();
    let mut input_open = true;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let result: Result<()> = async {
        loop {
            terminal.draw(|frame| draw(frame, &mut app)).context("绘制终端界面失败")?;
            tokio::select! {
                event = keys.next(), if input_open => match event {
                    Some(Ok(Event::Key(key))) => { app.key(key, &api, &app_tx, &media_tx); app.save(&store).await?; },
                    Some(Err(e)) => return Err(anyhow::Error::new(e).context("读取终端输入事件失败")),
                    None => { tracing::warn!("终端输入流关闭，开始退出"); input_open = false; app.begin_shutdown(); },
                    _ => {},
                },
                Some(event) = app_rx.recv() => { match event { AppEvent::Schedule { station, result } => if station == app.station { app.apply_schedule(result); } } },
                Some(event) = media_rx.recv() => {
                    let important = !matches!(event, MediaEvent::Progress { .. });
                    app.media_event(event).await;
                    if important { app.save(&store).await?; }
                },
                signal = tokio::signal::ctrl_c() => { signal.context("监听 Ctrl+C 失败")?; tracing::info!("收到 Ctrl+C"); app.request_exit(); },
                _ = tick.tick() => {
                    app.launch_due(&api, &media_tx);
                    if app.last_refresh.elapsed() >= Duration::from_secs(1800) && !app.shutting_down { app.request_schedule(api.clone(), app_tx.clone()); }
                    if app.last_save.elapsed() >= Duration::from_secs(5) { app.save(&store).await?; }
                },
            }
            if app.shutting_down && app.workers.is_empty() && app.preview.is_none() { app.save(&store).await?; break; }
        }
        Ok(())
    }.await;
    // Persist the original failure BEFORE cleanup. A later interruption/finalize
    // failure must not obscure the reason the interface stopped.
    if let Err(error) = &result {
        diagnostics::report_error("界面事件循环异常，准备停止录音", error);
    }
    // This cleanup also runs when terminal I/O or persistence fails.
    app.begin_shutdown();
    while !app.workers.is_empty() || app.preview.is_some() {
        match tokio::time::timeout(Duration::from_secs(75), media_rx.recv()).await {
            Ok(Some(event)) => app.media_event(event).await,
            _ => {
                tracing::error!(
                    recordings = app.workers.len(),
                    preview = app.preview.is_some(),
                    "退出清理超时或通道关闭，强制停止剩余任务"
                );
                for (_, worker) in app.workers.drain() {
                    worker.handle.abort();
                    let _ = worker.handle.await;
                }
                if let Some(worker) = app.preview.take() {
                    app.audio.stop();
                    worker.handle.abort();
                    let _ = worker.handle.await;
                }
                break;
            }
        }
    }
    app.dirty = true;
    let save_result = app.save(&store).await;
    if let Err(error) = &save_result {
        diagnostics::report_error("退出时保存预约失败", error);
    }
    result.and(save_result)
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 40 || area.height < 14 {
        frame.render_widget(
            Paragraph::new("窗口过小，请扩大至至少 40×14。\n预约与录音仍在运行；q 退出。")
                .wrap(Wrap { trim: false }),
            area,
        );
        if app.confirm_exit {
            frame.render_widget(Paragraph::new("退出将停止录音。\ny 确认 / n 返回"), area);
        }
        return;
    }
    let layout = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(8),
        Constraint::Length(3),
    ])
    .split(area);
    let heading = format!(
        " Radiko Recorder · {}{}  {} JST",
        if app.station.is_empty() {
            "请输入电台 ID"
        } else {
            &app.station
        },
        if app.loading { " · 加载中…" } else { "" },
        Utc::now().with_timezone(&jst()).format("%m-%d %H:%M:%S")
    );
    let audio_status = format!(
        " {} · 音量 {:.0}%{} · 保持程序开启才能录音",
        if app.preview_active {
            "♫ 试听中"
        } else {
            "试听关闭"
        },
        app.audio.volume() * 100.0,
        if app.audio.muted() {
            "（静音）"
        } else {
            ""
        }
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                heading,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from(audio_status),
        ]),
        layout[0],
    );
    let columns = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(layout[1]);
    let selected = app.selected_program().cloned();
    let programs = app.visible_programs();
    let rows: Vec<_> = programs
        .iter()
        .map(|p| {
            Row::new(vec![
                p.start.with_timezone(&jst()).format("%H:%M").to_string(),
                p.end
                    .with_timezone(&jst())
                    .format("%m/%d %H:%M")
                    .to_string(),
                format!(
                    "{}{}",
                    if p.start <= Utc::now() { "● " } else { "" },
                    p.title
                ),
            ])
        })
        .collect();
    let program_count = rows.len();
    if app
        .program_state
        .selected()
        .is_some_and(|i| i >= program_count)
    {
        app.program_state.select(program_count.checked_sub(1));
    }
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(11),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(["开始", "结束", "节目"]).style(Style::default().fg(Color::Yellow)))
    .block(panel(
        format!(
            " 节目表 {} · JST · ←/→ 日期 ",
            app.date.map(|d| d.to_string()).unwrap_or_default()
        ),
        app.focus == 0,
    ))
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(table, columns[0], &mut app.program_state);
    let detail = selected
        .map(|p| {
            format!(
                "{}\n{}\n\nJST：{} — {}\n本地：{} — {}\n\n{}",
                p.title,
                p.performer,
                p.start.with_timezone(&jst()).format("%m/%d %H:%M"),
                p.end.with_timezone(&jst()).format("%m/%d %H:%M"),
                p.start.with_timezone(&Local).format("%m/%d %H:%M %:z"),
                p.end.with_timezone(&Local).format("%m/%d %H:%M %:z"),
                p.description
            )
        })
        .unwrap_or_else(|| "暂无节目。按 r 刷新；s 可在无预约时修改电台 ID。".into());
    frame.render_widget(
        Paragraph::new(detail)
            .block(panel(" 节目详情 · Tab 后 ↑/↓ 滚动 ", app.focus == 1))
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        columns[1],
    );
    let jobs = app.visible_jobs();
    let rows: Vec<_> = jobs
        .iter()
        .map(|&i| {
            let j = &app.jobs[i];
            let countdown = (j.effective_start() - Utc::now()).num_seconds().max(0);
            Row::new(vec![
                format!(
                    "{}{}",
                    j.status.label(),
                    if j.schedule_changed {
                        " ⚠节目变更"
                    } else {
                        ""
                    }
                ),
                j.start
                    .with_timezone(&jst())
                    .format("%m/%d %H:%M")
                    .to_string(),
                j.program.title.clone(),
                if j.status == JobStatus::Scheduled {
                    format!("{countdown}s 后")
                } else {
                    format!(
                        "{:.0}s {:.1}MB",
                        j.recorded_seconds,
                        j.bytes as f64 / 1_048_576.0
                    )
                },
            ])
        })
        .collect();
    if app.job_state.selected().is_some_and(|i| i >= rows.len()) {
        app.job_state.select(rows.len().checked_sub(1));
    }
    let job_area = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(layout[2]);
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(13),
                Constraint::Length(11),
                Constraint::Min(10),
                Constraint::Length(17),
            ],
        )
        .block(panel(" 预约 · Enter 编辑 · x 取消 ", app.focus == 2))
        .row_highlight_style(Style::default().bg(Color::DarkGray)),
        job_area[0],
        &mut app.job_state,
    );
    let job_info = jobs
        .get(app.job_state.selected().unwrap_or(0))
        .map(|&i| {
            let j = &app.jobs[i];
            format!(
                "{}{}",
                j.detail,
                j.output
                    .as_ref()
                    .map(|p| format!(" · {}", p.display()))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(job_info).wrap(Wrap { trim: false }),
        job_area[1],
    );
    let footer = format!(
        "{}\nTab 面板 · Enter 预约 · r 刷新 · p 试听 · +/- 音量 · m 静音 · q 退出{}",
        app.message,
        app.config
            .ffmpeg_error
            .as_ref()
            .map(|_| "\n⚠ FFmpeg 不可用；配置后重启以启用录音/试听")
            .unwrap_or_default()
    );
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), layout[3]);
    if app.station.is_empty() {
        let popup = centered(area, 64, 8);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(format!(
                "输入 station ID，例如 JORF、RN2、INT\n\n> {}▏\n\nEnter 获取节目表 · Esc 退出",
                app.station_input
            ))
            .block(panel(" 选择电台 ", true)),
            popup,
        );
    }
    if let Some(editor) = &app.editor {
        let popup = centered(area, 76, 15);
        frame.render_widget(Clear, popup);
        let mut lines = vec![
            Line::from(editor.job.program.title.clone()),
            Line::from("时间均为 JST；格式 YYYY-MM-DD HH:MM:SS"),
            Line::from(""),
        ];
        for (i, label) in ["开始", "结束", "提前秒数", "延后秒数"]
            .into_iter()
            .enumerate()
        {
            lines.push(Line::styled(
                format!(
                    "{} {}: {}{}",
                    if i == editor.selected { ">" } else { " " },
                    label,
                    editor.fields[i],
                    if i == editor.selected { "▏" } else { "" }
                ),
                Style::default().fg(if i == editor.selected {
                    Color::Cyan
                } else {
                    Color::White
                }),
            ));
        }
        lines.extend([
            Line::from(""),
            Line::from("Tab/↑↓ 切换 · Ctrl+U 清空字段 · Backspace 删除"),
            Line::from("Ctrl+S 保存 · Enter 下一项/保存 · Esc 返回"),
            Line::styled(editor.error.clone(), Style::default().fg(Color::Red)),
            Line::from("已开始的节目只能从现在录制；节目变更不会自动改预约。"),
        ]);
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel(" 预约设置 ", true))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
    if app.confirm_exit {
        let popup = centered(area, 70, 8);
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new("退出会停止正在录制的任务。\n预约会保存，但程序关闭期间无法按时录制。\n已录音频会封装保存；重启可录制尚未结束的部分。\n\ny 确认退出 / n 或 Esc 返回").block(panel(" 退出确认 ",true)).wrap(Wrap { trim:false }), popup);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chinese_japanese_layout_handles_sizes_and_popups() {
        let mut app = App::new(
            Config {
                station: Some("JORF".into()),
                output_dir: std::env::temp_dir(),
                ffmpeg: "ffmpeg".into(),
                ffmpeg_error: None,
            },
            vec![],
        );
        let mut programs =
            crate::api::parse_schedule(include_str!("../tests/fixtures/schedule.xml"), "JORF")
                .unwrap();
        for p in &mut programs {
            p.start = Utc::now() + chrono::Duration::hours(1);
            p.end = p.start + chrono::Duration::minutes(30);
        }
        app.apply_schedule(Ok(programs));
        for (width, height) in [(120, 40), (80, 24), (40, 14), (20, 8)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            app.begin_editor();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            app.editor = None;
            app.confirm_exit = true;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            app.confirm_exit = false;
        }
    }
}
