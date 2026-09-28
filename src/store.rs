use crate::model::RecordingJob;
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub struct Store {
    dir: PathBuf,
    _lock: File,
}
#[derive(Serialize, Deserialize)]
struct Database {
    version: u32,
    jobs: Vec<RecordingJob>,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).context("无法创建数据目录")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("recorder.lock"))?;
        lock.try_lock_exclusive()
            .context("该数据目录正被另一实例使用")?;
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn load(&self) -> Result<Vec<RecordingJob>> {
        let path = self.dir.join("jobs.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let db: Database = serde_json::from_reader(File::open(path)?)
            .context("预约文件损坏，已保留原文件；请修复后重启")?;
        if db.version != 1 {
            bail!("不支持的预约文件版本 {}", db.version);
        }
        for job in &db.jobs {
            if !job.output_dir.is_absolute()
                || job.end <= job.start
                || job.pre_seconds > 3600
                || job.post_seconds > 3600
            {
                bail!("预约文件包含无效录制设置");
            }
            crate::model::validate_station(&job.program.station_id)?;
        }
        Ok(db.jobs)
    }

    pub fn save(&self, jobs: &[RecordingJob]) -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer_pretty(
            &mut tmp,
            &Database {
                version: 1,
                jobs: jobs.to_vec(),
            },
        )?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.dir.join("jobs.json"))
            .context("无法原子保存预约")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_lock_atomic_replace_and_corrupt_database() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        assert!(Store::open(temp.path()).is_err());
        store.save(&[]).unwrap();
        store.save(&[]).unwrap();
        assert!(store.load().unwrap().is_empty());
        fs::write(temp.path().join("jobs.json"), "{broken").unwrap();
        assert!(store.load().is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join("jobs.json")).unwrap(),
            "{broken"
        );
    }
}
