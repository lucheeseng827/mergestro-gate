//! The baseline service client. A baseline is the gate's precondition, and "commit it or
//! cache it" is the friction that gets a gate uninstalled: a shallow clone has no history
//! to build one from, and a cache key that missed means a cold full-history walk on a PR.
//! The Mergestro plane stores one file per repository; this fetches it before a run and
//! pushes it back after a refresh, with the same key the run's telemetry uses.
//!
//! Small and synchronous on purpose (`ureq`, bounded timeouts): a slow or unreachable plane
//! must never hang a customer's CI, and a missing file is `Ok(false)`, not an error — the
//! caller decides whether a cold start is acceptable.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context};

/// Connect/read/write timeout for one transfer. A baseline is at most tens of megabytes.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// Refuse to read a response larger than this — the plane's own upload cap.
pub const MAX_BYTES: u64 = 64 << 20;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(TIMEOUT)
        .timeout_write(TIMEOUT)
        .build()
}

fn url(base: &str, repo: &str) -> String {
    let base = base.trim_end_matches('/');
    // Slashes are path structure the plane's wildcard route accepts; everything else that
    // is not URL-safe is percent-encoded.
    let repo: String = repo
        .split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("{base}/v1/turnover/baseline/{repo}")
}

/// Download `repo`'s baseline into `dest`. `Ok(false)` when the plane has none.
pub fn fetch(base_url: &str, token: &str, repo: &str, dest: &Path) -> anyhow::Result<bool> {
    let u = url(base_url, repo);
    let resp = match agent()
        .get(&u)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "turnover")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => return Ok(false),
        Err(ureq::Error::Status(code, r)) => {
            return Err(anyhow!(
                "fetch baseline from {u}: HTTP {code}: {}",
                r.into_string().unwrap_or_default()
            ))
        }
        Err(e) => return Err(anyhow!("fetch baseline from {u}: {e}")),
    };
    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("reading baseline from {u}"))?;
    if body.len() as u64 > MAX_BYTES {
        return Err(anyhow!("baseline from {u} exceeds {MAX_BYTES} bytes"));
    }
    if let Some(dir) = dest.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
    }
    let tmp = dest.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("rename to {}", dest.display()))?;
    Ok(true)
}

/// Upload `src` as `repo`'s baseline, replacing the plane's copy.
pub fn push(base_url: &str, token: &str, repo: &str, src: &Path) -> anyhow::Result<()> {
    let u = url(base_url, repo);
    let body = std::fs::read(src).with_context(|| format!("read {}", src.display()))?;
    if body.len() as u64 > MAX_BYTES {
        return Err(anyhow!(
            "{} exceeds the plane's {MAX_BYTES}-byte baseline cap",
            src.display()
        ));
    }
    agent()
        .put(&u)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "turnover")
        .send_bytes(&body)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                anyhow!(
                    "push baseline to {u}: HTTP {code}: {}",
                    r.into_string().unwrap_or_default()
                )
            }
            other => anyhow!("push baseline to {u}: {other}"),
        })?;
    Ok(())
}

/// The token for the plane: `TURNOVER_TOKEN`, else `METRICS_TOKEN` (the Mergestro gate's
/// ingest token — one key for telemetry and baselines).
pub fn token_from_env() -> Option<String> {
    ["TURNOVER_TOKEN", "METRICS_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_keep_slashes_and_encode_the_rest() {
        assert_eq!(
            url("https://plane.example/", "acme/api"),
            "https://plane.example/v1/turnover/baseline/acme/api"
        );
        assert_eq!(
            url("http://h:8088", "a b/c#d"),
            "http://h:8088/v1/turnover/baseline/a%20b/c%23d"
        );
    }
}
