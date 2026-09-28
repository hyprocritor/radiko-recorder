use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

pub fn jst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).unwrap()
}

pub fn parse_radiko_time(value: &str) -> Result<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S").context("节目时间格式无效")?;
    Ok(jst()
        .from_local_datetime(&naive)
        .single()
        .context("无效 JST 时间")?
        .with_timezone(&Utc))
}

pub fn parse_editor_time(value: &str) -> Result<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M:%S")
        .context("时间格式应为 YYYY-MM-DD HH:MM:SS（JST）")?;
    Ok(jst()
        .from_local_datetime(&naive)
        .single()
        .context("无效 JST 时间")?
        .with_timezone(&Utc))
}

pub fn validate_station(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_uppercase();
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("station ID 只能包含英文字母、数字、连字符和下划线（最多 32 字符）");
    }
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Program {
    pub id: String,
    pub station_id: String,
    pub station_name: String,
    pub title: String,
    pub performer: String,
    pub description: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStatus {
    Scheduled,
    Preparing,
    Recording,
    Finalizing,
    Complete,
    Partial,
    Interrupted,
    Missed,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn pending(self) -> bool {
        matches!(
            self,
            Self::Scheduled
                | Self::Preparing
                | Self::Recording
                | Self::Finalizing
                | Self::Interrupted
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Scheduled => "已预约",
            Self::Preparing => "准备中",
            Self::Recording => "录制中",
            Self::Finalizing => "封装中",
            Self::Complete => "已完成",
            Self::Partial => "部分完成",
            Self::Interrupted => "已中断，可恢复",
            Self::Missed => "已错过",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamIssue {
    pub id: Uuid,
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
    /// True means timestamps came from HLS program-date-time, otherwise wall clock estimates.
    pub exact: bool,
    pub state: RecoveryState,
    pub reason: String,
    pub attempts: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecoveryState {
    Retrying,
    Recovered,
    Unavailable,
    Suspected,
}

impl RecoveryState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Retrying => "尝试恢复",
            Self::Recovered => "已恢复",
            Self::Unavailable => "未补回",
            Self::Suspected => "可能缺口",
        }
    }
}

impl StreamIssue {
    pub fn new(
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
        exact: bool,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            start,
            end,
            exact,
            state: RecoveryState::Retrying,
            reason: reason.into(),
            attempts: 0,
        }
    }

    pub fn description(&self) -> String {
        format!(
            "{} — {} JST · {}{} · 重试 {} 次\n{}",
            self.start.with_timezone(&jst()).format("%m/%d %H:%M:%S"),
            self.end
                .map(|t| t.with_timezone(&jst()).format("%m/%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "持续中".into()),
            self.state.label(),
            if self.exact {
                "（分片时间）"
            } else {
                "（估计时间）"
            },
            self.attempts,
            self.reason
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingJob {
    pub id: Uuid,
    pub program: Program,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub pre_seconds: u32,
    pub post_seconds: u32,
    pub output_dir: PathBuf,
    pub status: JobStatus,
    pub detail: String,
    pub has_gap: bool,
    pub recorded_seconds: f64,
    pub bytes: u64,
    pub output: Option<PathBuf>,
    #[serde(default)]
    pub schedule_changed: bool,
    #[serde(default)]
    pub issues: Vec<StreamIssue>,
}

impl RecordingJob {
    pub fn new(program: Program, output_dir: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4(),
            start: program.start,
            end: program.end,
            program,
            pre_seconds: 15,
            post_seconds: 60,
            output_dir,
            status: JobStatus::Scheduled,
            detail: String::new(),
            has_gap: false,
            recorded_seconds: 0.0,
            bytes: 0,
            output: None,
            schedule_changed: false,
            issues: Vec::new(),
        }
    }

    pub fn effective_start(&self) -> DateTime<Utc> {
        self.start - Duration::seconds(self.pre_seconds as i64)
    }
    pub fn effective_end(&self) -> DateTime<Utc> {
        self.end + Duration::seconds(self.post_seconds as i64)
    }

    pub fn validate(&self, now: DateTime<Utc>) -> Result<()> {
        if self.end <= self.start {
            bail!("结束时间必须晚于开始时间");
        }
        if self.pre_seconds > 3600 || self.post_seconds > 3600 {
            bail!("提前和延后秒数须在 0–3600 之间");
        }
        if self.effective_end() <= now {
            bail!("录制时间已结束；第一版不支持回听下载");
        }
        if !self.output_dir.is_absolute() {
            bail!("录音目录必须是绝对路径");
        }
        Ok(())
    }

    pub fn basename(&self) -> String {
        format!(
            "{}_{}_{}_{}",
            self.program.station_id,
            self.start.with_timezone(&jst()).format("%Y%m%d_%H%M%S"),
            safe_filename(&self.program.title),
            &self.id.simple().to_string()[..8]
        )
    }

    pub fn parts_dir(&self) -> PathBuf {
        self.output_dir.join(".parts").join(self.id.to_string())
    }
}

pub fn safe_filename(value: &str) -> String {
    let s: String = value
        .chars()
        .take(60)
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim_matches([' ', '.']);
    if s.is_empty() {
        "节目".into()
    } else {
        s.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_uses_real_date_and_jst() {
        assert_eq!(
            parse_radiko_time("20260922010000").unwrap().to_rfc3339(),
            "2026-09-21T16:00:00+00:00"
        );
        assert!(parse_radiko_time("20260921250000").is_err());
        assert_eq!(
            parse_editor_time("2026-09-22 01:00:00").unwrap(),
            parse_radiko_time("20260922010000").unwrap()
        );
    }

    #[test]
    fn filenames_and_station_paths_are_safe() {
        assert_eq!(safe_filename("番組: /深夜? ."), "番組_ _深夜_");
        assert!(validate_station("../auth1").is_err());
        assert_eq!(validate_station("joak-fm").unwrap(), "JOAK-FM");
    }
}
