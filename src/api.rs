use crate::model::{Program, parse_radiko_time, validate_station};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OnceCell};

const FALLBACK_KEY: &str = "bcd151073c03b352e1ef2fd66c32209da9ca0afa";

#[derive(Debug)]
pub struct PermanentError(pub String);
impl std::fmt::Display for PermanentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for PermanentError {}

#[derive(Clone)]
struct Auth {
    token: String,
    area: String,
    created: Instant,
}

pub struct Radiko {
    client: Client,
    api_base: String,
    web_base: String,
    key: OnceCell<String>,
    auth: Mutex<Option<Auth>>,
}

impl Radiko {
    pub fn new() -> Result<Self> {
        Self::with_endpoints("https://api.radiko.jp", "https://radiko.jp")
    }

    /// Endpoint injection also supports offline protocol tests; never populated from HAR credentials.
    pub fn with_endpoints(api: &str, web: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent("radiko-recorder/0.1")
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()?,
            api_base: api.trim_end_matches('/').into(),
            web_base: web.trim_end_matches('/').into(),
            key: OnceCell::new(),
            auth: Mutex::new(None),
        })
    }

    pub async fn schedule(&self, station: &str) -> Result<Vec<Program>> {
        let station = validate_station(station)?;
        let response = self
            .client
            .get(format!("{}/program/v3/weekly/{station}.xml", self.api_base))
            .send()
            .await
            .map_err(|e| network_error("获取节目表", e))?;
        if response.status() == StatusCode::NOT_FOUND {
            bail!("找不到电台 {station}");
        }
        let body = checked_text(response, "获取节目表").await?;
        parse_schedule(&body, &station)
    }

    async fn application_key(&self) -> &String {
        self.key
            .get_or_init(|| async {
                let fetched = async {
                    let response = self
                        .client
                        .get(format!("{}/apps/js/playerCommon.js", self.web_base))
                        .send()
                        .await
                        .ok()?;
                    if !response.status().is_success() {
                        return None;
                    }
                    let body = response.text().await.ok()?;
                    extract_application_key(&body)
                }
                .await;
                if fetched.is_none() {
                    tracing::warn!("无法提取应用密钥，使用已确认的公开备用值");
                }
                fetched.unwrap_or_else(|| FALLBACK_KEY.into())
            })
            .await
    }

    async fn authenticate(&self, force: bool) -> Result<Auth> {
        let mut cache = self.auth.lock().await;
        if !force
            && let Some(auth) = cache.as_ref()
            && auth.created.elapsed() < Duration::from_secs(3600)
        {
            return Ok(auth.clone());
        }
        let key = self.application_key().await;
        let response = self
            .client
            .get(format!("{}/v2/api/auth1", self.api_base))
            .header("X-Radiko-App", "pc_html5")
            .header("X-Radiko-App-Version", "0.0.1")
            .header("X-Radiko-User", "dummy_user")
            .header("X-Radiko-Device", "pc")
            .send()
            .await
            .map_err(|e| network_error("auth1", e))?;
        check_status(&response, "auth1")?;
        let header = |name| -> Result<&str> {
            response
                .headers()
                .get(name)
                .context("auth1 缺少必需响应头")?
                .to_str()
                .context("认证响应头无效")
        };
        let token = header("x-radiko-authtoken")?.to_string();
        if token.is_empty() {
            bail!(PermanentError("auth1 返回空 token".into()));
        }
        let offset = header("x-radiko-keyoffset")?
            .parse()
            .context("keyoffset 无效")?;
        let length = header("x-radiko-keylength")?
            .parse()
            .context("keylength 无效")?;
        let partial = partial_key(key, offset, length)?;
        let response = self
            .client
            .get(format!("{}/v2/api/auth2", self.api_base))
            .header("X-Radiko-AuthToken", &token)
            .header("X-Radiko-Partialkey", partial)
            .header("X-Radiko-User", "dummy_user")
            .header("X-Radiko-Device", "pc")
            .header("X-Radiko-Connection", "wifi")
            .send()
            .await
            .map_err(|e| network_error("auth2", e))?;
        let body = checked_text(response, "auth2").await?;
        let area = body.trim().split(',').next().unwrap_or_default();
        if area.len() != 4
            || !area.starts_with("JP")
            || !area[2..].bytes().all(|c| c.is_ascii_digit())
        {
            bail!(PermanentError(
                "认证未返回日本地区：当前网络可能无法收听 Radiko".into()
            ));
        }
        let auth = Auth {
            token,
            area: area.into(),
            created: Instant::now(),
        };
        *cache = Some(auth.clone());
        Ok(auth)
    }

    pub async fn invalidate_auth(&self) {
        *self.auth.lock().await = None;
    }

    pub async fn live_url(&self, station: &str) -> Result<String> {
        let station = validate_station(station)?;
        for attempt in 0..2 {
            let result = async {
                let auth = self.authenticate(attempt > 0).await?;
                self.resolve_live(&station, &auth).await
            }
            .await;
            match result {
                Ok(url) => return Ok(url),
                Err(e) if e.downcast_ref::<PermanentError>().is_some() && attempt == 0 => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn resolve_live(&self, station: &str, auth: &Auth) -> Result<String> {
        let response = self
            .client
            .get(format!(
                "{}/v3/station/stream/pc_html5/{station}.xml",
                self.api_base
            ))
            .send()
            .await
            .map_err(|e| network_error("获取直播入口", e))?;
        let endpoints = live_endpoints(&checked_text(response, "获取直播入口").await?)?;
        let mut last_error = anyhow::anyhow!("没有可用直播入口");
        for endpoint in endpoints {
            let mut url = Url::parse(&endpoint).context("直播入口 URL 无效")?;
            url.query_pairs_mut()
                .append_pair("station_id", station)
                .append_pair("l", "15")
                .append_pair("lsid", &uuid::Uuid::new_v4().simple().to_string())
                .append_pair("type", "b");
            let result = async {
                let response = self
                    .client
                    .get(url.clone())
                    .header("X-Radiko-AuthToken", &auth.token)
                    .header("X-Radiko-AreaId", &auth.area)
                    .send()
                    .await
                    .map_err(|e| network_error("获取直播清单", e))?;
                let base = response.url().clone();
                let body = checked_text(response, "获取直播清单").await?;
                media_playlist(&body, &base)
            }
            .await;
            match result {
                Ok(url) => return Ok(url),
                Err(e) => last_error = e,
            }
        }
        Err(last_error)
    }
}

fn network_error(stage: &str, error: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "{stage}：{}",
        if error.is_timeout() {
            "网络超时"
        } else if error.is_connect() {
            "连接失败"
        } else {
            "网络请求失败"
        }
    )
}

fn check_status(response: &Response, stage: &str) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    if status.is_client_error()
        && status != StatusCode::TOO_MANY_REQUESTS
        && status != StatusCode::REQUEST_TIMEOUT
    {
        bail!(PermanentError(format!(
            "{stage}：HTTP {status}，请检查当前地区的收听权限或电台 ID"
        )));
    }
    bail!("{stage}：HTTP {status}")
}

async fn checked_text(response: Response, stage: &str) -> Result<String> {
    check_status(&response, stage)?;
    response.text().await.map_err(|e| network_error(stage, e))
}

pub fn partial_key(key: &str, offset: usize, length: usize) -> Result<String> {
    let end = offset.checked_add(length).context("密钥范围溢出")?;
    if length == 0 || !key.is_ascii() {
        bail!("应用密钥或切片长度无效");
    }
    Ok(STANDARD.encode(
        key.as_bytes()
            .get(offset..end)
            .context("服务端密钥范围超出应用密钥")?,
    ))
}

pub fn extract_application_key(js: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"new\s+RadikoJSPlayer\([^,]+,\s*['"]pc_html5['"]\s*,\s*['"]([0-9a-fA-F]+)['"]"#,
        )
        .unwrap()
    });
    re.captures(js).map(|c| c[1].to_string())
}

pub fn redact(text: &str) -> String {
    redact_full(text).chars().take(1200).collect()
}

/// File diagnostics retain the complete error chain and backtrace.
pub fn redact_full(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)https?://[^\s\x22<>]+|(?:x-radiko-(?:authtoken|partialkey|session)|radiko_session|session|lsid|auth[_-]?token|partial[_-]?key|token)[\x22'\\]*\s*[:=]\s*[\x22'\\]*[^\s,;&\x22'\\]+"#).unwrap())
        .replace_all(text, "[已隐藏网络凭证/地址]").chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

#[derive(Deserialize)]
struct ScheduleXml {
    stations: StationsXml,
}
#[derive(Deserialize)]
struct StationsXml {
    #[serde(default, rename = "station")]
    stations: Vec<StationXml>,
}
#[derive(Deserialize)]
struct StationXml {
    #[serde(rename = "@id")]
    id: String,
    name: String,
    #[serde(default)]
    progs: Vec<ProgramsXml>,
}
#[derive(Deserialize)]
struct ProgramsXml {
    #[serde(default, rename = "prog")]
    programs: Vec<ProgramXml>,
}
#[derive(Deserialize)]
struct ProgramXml {
    #[serde(rename = "@id")]
    id: String,
    #[serde(rename = "@ft")]
    start: String,
    #[serde(rename = "@to")]
    end: String,
    title: String,
    #[serde(default)]
    pfm: String,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    info: String,
}

pub fn plain_text(value: &str) -> String {
    let html = scraper::Html::parse_fragment(value);
    html.root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

pub fn parse_schedule(xml: &str, station: &str) -> Result<Vec<Program>> {
    let doc: ScheduleXml = quick_xml::de::from_str(xml).context("节目表 XML 无法解析")?;
    let station = doc
        .stations
        .stations
        .into_iter()
        .find(|s| s.id == station)
        .context("节目表中找不到该电台")?;
    let mut programs = Vec::new();
    for group in station.progs {
        for p in group.programs {
            let start = parse_radiko_time(&p.start)?;
            let end = parse_radiko_time(&p.end)?;
            if end <= start {
                bail!("节目表包含无效时间范围");
            }
            programs.push(Program {
                id: p.id,
                station_id: station.id.clone(),
                station_name: plain_text(&station.name),
                title: plain_text(&p.title),
                performer: plain_text(&p.pfm),
                description: plain_text(&format!("{} {}", p.desc, p.info)),
                start,
                end,
            });
        }
    }
    programs.sort_by_key(|p| p.start);
    programs.dedup_by(|a, b| a.id == b.id && a.start == b.start);
    Ok(programs)
}

#[derive(Deserialize)]
struct Endpoints {
    #[serde(rename = "url", default)]
    urls: Vec<Endpoint>,
}
#[derive(Deserialize)]
struct Endpoint {
    #[serde(rename = "@timefree")]
    timefree: String,
    #[serde(rename = "@areafree")]
    areafree: String,
    playlist_create_url: String,
}

pub fn live_endpoints(xml: &str) -> Result<Vec<String>> {
    let endpoints: Endpoints = quick_xml::de::from_str(xml).context("直播入口 XML 无效")?;
    let urls: Vec<_> = endpoints
        .urls
        .into_iter()
        .filter(|u| u.timefree == "0" && u.areafree == "0")
        .map(|u| u.playlist_create_url)
        .collect();
    if urls.is_empty() {
        bail!(PermanentError("该电台没有当前地区免费直播入口".into()));
    }
    Ok(urls)
}

pub fn media_playlist(body: &str, base: &Url) -> Result<String> {
    if !body.trim_start().starts_with("#EXTM3U") {
        bail!("服务器未返回 HLS 清单");
    }
    let mut variant = false;
    for line in body.lines().map(str::trim) {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            variant = true;
        } else if variant && !line.is_empty() && !line.starts_with('#') {
            let url = base.join(line).context("媒体清单地址无效")?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!("媒体清单必须使用 HTTP(S)");
            }
            return Ok(url.to_string());
        }
    }
    // Avoid handing authenticated master URLs to FFmpeg, whose stderr could expose credentials.
    bail!("直播主清单没有可用的媒体清单")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_is_ascii_slice_not_hex_decode() {
        assert_eq!(
            partial_key(FALLBACK_KEY, 9, 16).unwrap(),
            "YzAzYjM1MmUxZWYyZmQ2Ng=="
        );
        assert!(partial_key(FALLBACK_KEY, usize::MAX, 16).is_err());
        assert_eq!(
            extract_application_key("new RadikoJSPlayer($audio[0], 'pc_html5', 'abcd', {})"),
            Some("abcd".into())
        );
    }
    #[test]
    fn har_fixtures_parse_without_cookies() {
        let programs =
            parse_schedule(include_str!("../tests/fixtures/schedule.xml"), "JORF").unwrap();
        assert_eq!(programs.len(), 2);
        assert_eq!(
            programs[1]
                .start
                .with_timezone(&crate::model::jst())
                .format("%H:%M")
                .to_string(),
            "01:00"
        );
        assert!(!programs[0].description.contains('<'));
        let urls = live_endpoints(include_str!("../tests/fixtures/streams.xml")).unwrap();
        assert_eq!(urls.len(), 2);
    }
    #[test]
    fn playlist_resolution_and_redaction() {
        let url = media_playlist(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=52973\n../medialist?session=secret",
            &Url::parse("https://example.test/so/playlist.m3u8").unwrap(),
        )
        .unwrap();
        assert_eq!(url, "https://example.test/medialist?session=secret");
        assert!(
            !redact("GET /medialist?session=supersecret&station_id=JORF HTTP/1.1")
                .contains("secret")
        );
        assert!(
            !redact(&format!("failed: {url} X-Radiko-AuthToken: supersecret")).contains("secret")
        );
        for value in [
            r#"{"X-Radiko-AuthToken": "supersecret"}"#,
            r#"error=\"auth_token=supersecret\""#,
            "X-Radiko-PartialKey: supersecret",
        ] {
            assert!(!redact_full(value).contains("supersecret"));
        }
    }
}
