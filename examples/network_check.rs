//! Diagnostic output is redacted; never writes token or session URLs.
use anyhow::Result;
use radiko_recorder::api::{Radiko, redact};
use std::process::Stdio;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{}", redact(&format!("{error:#}")));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let station = std::env::args().nth(1).unwrap_or_else(|| "JORF".into());
    let api = Radiko::new()?;
    let url = api.live_url(&station).await?;
    println!(
        "Media host: {}",
        reqwest::Url::parse(&url)?.host_str().unwrap_or_default()
    );
    let response = reqwest::Client::new().get(&url).send().await?;
    println!("Media HTTP status: {}", response.status());
    let body = response.text().await?;
    println!(
        "Media playlist: {} bytes, {} segments",
        body.len(),
        body.lines().filter(|l| l.starts_with("#EXTINF")).count()
    );
    for (name, request) in [
        (
            "Lavf UA",
            reqwest::Client::new()
                .get(&url)
                .header("User-Agent", "Lavf/63.1.102"),
        ),
        (
            "Range",
            reqwest::Client::new().get(&url).header("Range", "bytes=0-"),
        ),
        (
            "Icy",
            reqwest::Client::new().get(&url).header("Icy-MetaData", "1"),
        ),
    ] {
        let body = request.send().await?.text().await?;
        println!(
            "Variant {name}: {} bytes; HLS={}",
            body.len(),
            body.starts_with("#EXTM3U")
        );
    }
    if let Some(segment) = body.lines().find(|l| !l.is_empty() && !l.starts_with('#')) {
        let segment = reqwest::Url::parse(&url)?.join(segment)?;
        let response = reqwest::Client::new().get(segment).send().await?;
        println!("First segment HTTP status: {}", response.status());
    }
    let binary = std::env::var_os("RADIKO_TEST_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let mut command = tokio::process::Command::new(binary);
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "debug",
                "-rw_timeout",
                "10000000",
                "-seekable",
                "0",
                "-http_seekable",
                "0",
                "-user_agent",
                "radiko-recorder/0.1",
                "-i",
                &url,
                "-t",
                "2",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    let log = String::from_utf8_lossy(&output.stderr);
    for line in log
        .lines()
        .rev()
        .take(35)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        println!("{}", redact(line));
    }
    Ok(())
}
