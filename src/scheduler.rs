use crate::model::{JobStatus, RecordingJob};
use chrono::{DateTime, Duration, Utc};

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Wait,
    Prepare,
    Missed,
}

fn has_parts(job: &RecordingJob) -> bool {
    std::fs::read_dir(job.parts_dir()).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.path().extension().is_some_and(|ext| ext == "ts")
                && e.metadata().is_ok_and(|m| m.len() > 0)
        })
    })
}

pub fn decide(job: &RecordingJob, now: DateTime<Utc>) -> Decision {
    if !matches!(job.status, JobStatus::Scheduled | JobStatus::Interrupted) {
        return Decision::Wait;
    }
    if job.status == JobStatus::Interrupted && has_parts(job) {
        // Includes offline jobs whose window ended: worker will only finalize existing parts.
        Decision::Prepare
    } else if now >= job.effective_end() {
        Decision::Missed
    } else if now >= job.effective_start() - Duration::seconds(30) {
        Decision::Prepare
    } else {
        Decision::Wait
    }
}

pub fn recover(jobs: &mut [RecordingJob], now: DateTime<Utc>) {
    for job in jobs {
        if !job.status.pending() {
            continue;
        }
        let was_active = matches!(
            job.status,
            JobStatus::Recording | JobStatus::Finalizing | JobStatus::Interrupted
        );
        if now >= job.effective_end() {
            job.status = if has_parts(job) {
                job.has_gap = true;
                JobStatus::Interrupted
            } else if was_active {
                JobStatus::Partial
            } else {
                JobStatus::Missed
            };
            job.detail = "程序离线期间录制窗口结束；已有音频和临时片段已保留".into();
        } else {
            job.status = JobStatus::Scheduled;
            if was_active || now > job.effective_start() {
                job.has_gap = true;
                job.detail = "恢复预约：将录制剩余内容，无法补录离线部分".into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Program;

    fn job() -> RecordingJob {
        let start = DateTime::parse_from_rfc3339("2026-09-28T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        RecordingJob::new(
            Program {
                id: "1".into(),
                station_id: "JORF".into(),
                station_name: String::new(),
                title: "test".into(),
                performer: String::new(),
                description: String::new(),
                start,
                end: start + Duration::hours(1),
            },
            std::env::temp_dir(),
        )
    }

    #[test]
    fn prepare_and_end_boundaries() {
        let j = job();
        assert_eq!(
            decide(&j, j.effective_start() - Duration::seconds(31)),
            Decision::Wait
        );
        assert_eq!(
            decide(&j, j.effective_start() - Duration::seconds(30)),
            Decision::Prepare
        );
        assert_eq!(
            decide(&j, j.start + Duration::minutes(30)),
            Decision::Prepare
        );
        assert_eq!(decide(&j, j.effective_end()), Decision::Missed);
    }

    #[test]
    fn overlapping_jobs_are_independent_and_recovery_marks_gaps() {
        let j = job();
        let now = j.start + Duration::minutes(10);
        let mut jobs = vec![j.clone(), j];
        jobs[0].status = JobStatus::Recording;
        recover(&mut jobs, now);
        assert!(
            jobs.iter()
                .all(|j| j.has_gap && decide(j, now) == Decision::Prepare)
        );
        jobs[0].status = JobStatus::Cancelled;
        assert_eq!(decide(&jobs[0], now), Decision::Wait);
    }

    #[test]
    fn expired_tasks_only_finalize_when_fragments_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut recording = job();
        recording.output_dir = dir.path().to_path_buf();
        recording.status = JobStatus::Recording;
        std::fs::create_dir_all(recording.parts_dir()).unwrap();
        std::fs::write(
            recording.parts_dir().join("000000_test.ts"),
            b"synthetic fragment",
        )
        .unwrap();
        let now = recording.effective_end() + Duration::hours(1);
        let mut queued = job();
        queued.output_dir = dir.path().to_path_buf();
        let mut jobs = vec![recording, queued];
        recover(&mut jobs, now);
        assert_eq!(decide(&jobs[0], now), Decision::Prepare);
        assert!(jobs[0].has_gap);
        assert_eq!(jobs[1].status, JobStatus::Missed);
    }
}
