//! Checks the live decoding/device pipeline muted; no audible playback is required.
use anyhow::Result;
use radiko_recorder::{
    api::Radiko,
    media::{self, AudioControl, MediaEvent, StopReason, StreamSource},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

#[tokio::main]
async fn main() -> Result<()> {
    let station = std::env::args().nth(1).unwrap_or_else(|| "JORF".into());
    let binary: PathBuf = std::env::var_os("RADIKO_TEST_FFMPEG")
        .unwrap_or_else(|| "ffmpeg".into())
        .into();
    let audio = AudioControl::default();
    audio.toggle_mute();
    audio.set_volume(0.25);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = watch::channel(None);
    let task = tokio::spawn(media::preview(
        binary,
        StreamSource::Radiko(Arc::new(Radiko::new()?), station),
        tx,
        audio.clone(),
        stop_rx,
    ));
    match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await? {
        Some(MediaEvent::Preview { active: true, .. }) => {
            println!("Audio output device opened; preview decoding started (muted).")
        }
        Some(event) => anyhow::bail!("{event:?}"),
        None => anyhow::bail!("preview worker stopped unexpectedly"),
    }
    tokio::time::sleep(Duration::from_secs(8)).await;
    audio.set_volume(0.6); // Verify control changes do not restart the stream.
    stop.send(Some(StopReason::Cancel))?;
    task.await?;
    match rx.recv().await {
        Some(MediaEvent::Preview {
            active: false,
            detail,
        }) if detail == "试听已停止" => {
            println!("Preview exited cleanly after 8 seconds; volume and mute controls accepted.")
        }
        event => anyhow::bail!("unexpected preview result: {event:?}"),
    }
    Ok(())
}
