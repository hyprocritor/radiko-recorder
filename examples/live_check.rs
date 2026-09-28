//! Explicit live smoke check. No token or stream URL is written to output.
use anyhow::Result;
use chrono::{Duration, Utc};
use radiko_recorder::{
    api::Radiko,
    media::{self, MediaEvent, StreamSource},
    model::RecordingJob,
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, watch};

#[tokio::main]
async fn main() -> Result<()> {
    let station = std::env::args().nth(1).unwrap_or_else(|| "RN2".into());
    let binary: PathBuf = std::env::var_os("RADIKO_TEST_FFMPEG")
        .unwrap_or_else(|| "ffmpeg".into())
        .into();
    let api = Arc::new(Radiko::new()?);
    let programs = api.schedule(&station).await?;
    println!("Fetched {} programs for {station}", programs.len());
    let program = programs
        .into_iter()
        .find(|p| p.start <= Utc::now() && p.end > Utc::now())
        .ok_or_else(|| anyhow::anyhow!("no currently airing program"))?;
    println!("Current program: {}", program.title);
    let directory = std::env::current_dir()?.join("target/live-check");
    let mut job = RecordingJob::new(program, directory);
    job.start = Utc::now();
    job.end = job.start + Duration::seconds(25);
    job.pre_seconds = 0;
    job.post_seconds = 0;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = watch::channel(None);
    let task = tokio::spawn(media::record(
        job,
        binary,
        StreamSource::Radiko(api, station),
        tx,
        stop_rx,
    ));
    while let Some(event) = rx.recv().await {
        match event {
            MediaEvent::Status { status, detail, .. } => println!("{status:?}: {detail}"),
            MediaEvent::Finished { outcome, .. } => {
                println!("{:?}: {}", outcome.status, outcome.detail);
                task.await?;
                if let Some(path) = outcome.output {
                    println!("Output: {}", path.display());
                    return Ok(());
                }
                anyhow::bail!("Live recording unavailable: {}", outcome.detail);
            }
            _ => {}
        }
    }
    Ok(())
}
