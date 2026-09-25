// SPDX-License-Identifier: Apache-2.0
//! Minimal, synchronous GitHub REST client for posting the survivor report as
//! a PR comment. Idempotent: it finds a prior comment by [`COMMENT_MARKER`] and
//! updates it in place, so re-runs and merge-queue re-triggers don't stack
//! duplicate comments.
//!
//! Kept deliberately small — `ureq` + `serde_json`, no async runtime.

use std::env;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::report::COMMENT_MARKER;

/// The bits of GitHub Actions context needed to comment on a PR.
#[derive(Debug, Clone)]
pub struct GithubContext {
    pub api: String,
    pub owner: String,
    pub repo: String,
    pub pr: u64,
    pub token: String,
}

impl GithubContext {
    /// Assemble context from the standard GitHub Actions environment.
    pub fn from_env() -> Result<Self> {
        let token = env::var("GITHUB_TOKEN")
            .or_else(|_| env::var("INPUT_TOKEN"))
            .context("GITHUB_TOKEN is not set (needed to comment on the PR)")?;
        let repository = env::var("GITHUB_REPOSITORY").context("GITHUB_REPOSITORY is not set")?;
        let (owner, repo) = parse_repo(&repository)
            .with_context(|| format!("GITHUB_REPOSITORY `{repository}` is not `owner/repo`"))?;
        let pr = detect_pr_number().context(
            "could not determine the PR number from GITHUB_REF / GITHUB_EVENT_PATH / PR_NUMBER",
        )?;
        let api =
            env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".to_string());
        Ok(GithubContext {
            api,
            owner,
            repo,
            pr,
            token,
        })
    }
}

/// Post `body` as a PR comment, updating the existing slop-gate comment if one
/// is present.
pub fn post_or_update_comment(ctx: &GithubContext, body: &str) -> Result<()> {
    let agent = ureq::AgentBuilder::new().build();

    let list_url = format!(
        "{}/repos/{}/{}/issues/{}/comments?per_page=100",
        ctx.api, ctx.owner, ctx.repo, ctx.pr
    );
    let comments: Value = with_headers(agent.get(&list_url), ctx)
        .call()
        .context("listing PR comments")?
        .into_json()
        .context("parsing PR comments")?;

    if let Some(id) = find_marker_comment(&comments, COMMENT_MARKER) {
        let url = format!(
            "{}/repos/{}/{}/issues/comments/{}",
            ctx.api, ctx.owner, ctx.repo, id
        );
        with_headers(agent.request("PATCH", &url), ctx)
            .send_json(json!({ "body": body }))
            .context("updating existing PR comment")?;
    } else {
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            ctx.api, ctx.owner, ctx.repo, ctx.pr
        );
        with_headers(agent.post(&url), ctx)
            .send_json(json!({ "body": body }))
            .context("creating PR comment")?;
    }
    Ok(())
}

/// Which API dialect `api` points at.
///
/// The comment endpoints are the same shape on both — `…/repos/{owner}/{repo}/issues/{n}/comments`
/// — so this decides one thing only: the authorization header.
///
/// **Detected from the API root rather than configured**, because the three roots are already
/// distinct and a runner sets the variable for us: GitHub is `api.github.com`, GitHub Enterprise
/// Server is `…/api/v3`, and Gitea/Forgejo is `…/api/v1`. Asking a user to set a flag that can be
/// read off a value they have already supplied is a step to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Github,
    Gitea,
}

impl Dialect {
    fn of(api: &str) -> Self {
        // Gitea Actions sets GITHUB_API_URL to its own `/api/v1` root, which is what makes running
        // this Action on Gitea work at all.
        if api.trim_end_matches('/').ends_with("/api/v1") {
            Dialect::Gitea
        } else {
            Dialect::Github
        }
    }

    /// The `Authorization` value this dialect accepts.
    ///
    /// **Gitea does not accept `Bearer` for a personal access token.** Its documentation is
    /// explicit — *"for historical reasons, Gitea needs the word `token` included before the API
    /// key token in an authorization header"* — and `Bearer` there reserved for OAuth2 provider
    /// tokens, answering 401 or 404 for anything else (go-gitea/gitea#31936). This gate sent
    /// `Bearer` unconditionally, so `--comment` could never have worked against a Gitea instance;
    /// it failed as a warning, which is best-effort by contract, so the verdict was right and the
    /// comment silently never appeared.
    fn authorization(self, token: &str) -> String {
        match self {
            Dialect::Github => format!("Bearer {token}"),
            Dialect::Gitea => format!("token {token}"),
        }
    }
}

/// Apply the API headers to a request, for whichever forge `ctx.api` names.
fn with_headers(req: ureq::Request, ctx: &GithubContext) -> ureq::Request {
    let dialect = Dialect::of(&ctx.api);
    let req = req
        .set("Authorization", &dialect.authorization(&ctx.token))
        .set("User-Agent", "slop-gate");
    match dialect {
        // Gitea ignores both of these, but sending a GitHub media type and an API version to a
        // forge that is not GitHub is a claim about the server that this is not in a position to
        // make. Sent only where they mean something.
        Dialect::Github => req
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28"),
        Dialect::Gitea => req.set("Accept", "application/json"),
    }
}

/// Split `owner/repo` into its parts.
fn parse_repo(s: &str) -> Option<(String, String)> {
    let (owner, repo) = s.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Determine the PR number from the environment, trying the cheapest sources
/// first: an explicit `PR_NUMBER`, then `GITHUB_REF`, then the event payload.
fn detect_pr_number() -> Option<u64> {
    if let Ok(n) = env::var("PR_NUMBER") {
        if let Ok(n) = n.trim().parse() {
            return Some(n);
        }
    }
    if let Ok(r) = env::var("GITHUB_REF") {
        if let Some(n) = parse_pr_from_ref(&r) {
            return Some(n);
        }
    }
    if let Ok(path) = env::var("GITHUB_EVENT_PATH") {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(event) = serde_json::from_str::<Value>(&text) {
                return pr_number_from_event(&event);
            }
        }
    }
    None
}

/// Parse `refs/pull/<n>/merge` (or `/head`) into the PR number.
fn parse_pr_from_ref(r: &str) -> Option<u64> {
    let rest = r.strip_prefix("refs/pull/")?;
    let (num, _) = rest.split_once('/')?;
    num.parse().ok()
}

/// Pull the PR number out of a `pull_request` webhook payload.
fn pr_number_from_event(event: &Value) -> Option<u64> {
    event
        .get("pull_request")
        .and_then(|pr| pr.get("number"))
        .or_else(|| event.get("number"))
        .and_then(Value::as_u64)
}

/// Find the id of the first comment whose body carries `marker`.
fn find_marker_comment(comments: &Value, marker: &str) -> Option<u64> {
    comments.as_array()?.iter().find_map(|c| {
        let body = c.get("body")?.as_str()?;
        if body.contains(marker) {
            c.get("id")?.as_u64()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One recorded request: `(method, path, authorization)`.
    type SeenRequest = (String, String, String);

    /// A forge that answers `responses` in order and records what it was asked.
    ///
    /// The comment path is HTTP, and the bug this module just fixed was invisible to every test
    /// that did not make a request: the URLs were always right and the *header* was wrong. So
    /// these drive the real `post_or_update_comment` against a real socket.
    fn fake_forge(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<SeenRequest>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (code, body) in responses {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut out = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    break;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut auth = String::new();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 {
                        break;
                    }
                    let h = h.trim_end().to_string();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        match k.trim().to_ascii_lowercase().as_str() {
                            "authorization" => auth = v.trim().to_string(),
                            "content-length" => len = v.trim().parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                if len > 0 {
                    let mut buf = vec![0u8; len];
                    reader.read_exact(&mut buf).ok();
                }
                seen.push((method, path, auth));
                let resp = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                out.write_all(resp.as_bytes()).ok();
                out.flush().ok();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    fn ctx_for(api: &str) -> GithubContext {
        GithubContext {
            api: api.to_string(),
            owner: "octo".into(),
            repo: "api".into(),
            pr: 7,
            token: "fake-pat".into(),
        }
    }

    #[test]
    fn a_comment_on_gitea_carries_the_header_gitea_accepts() {
        // The end-to-end form of the bug. Before this fix the request below went out with
        // `Bearer fake-pat`, which Gitea answers 401/404 — so `--comment` warned and the comment
        // never appeared, on every Gitea instance, silently.
        let (base, handle) = fake_forge(vec![(200, "[]"), (201, r#"{"id": 5}"#)]);
        let ctx = ctx_for(&format!("{base}/api/v1"));
        post_or_update_comment(&ctx, "hello").expect("the comment posts");
        let seen = handle.join().unwrap();
        assert_eq!(seen.len(), 2, "one listing, one create");
        assert_eq!(
            seen[0].1,
            "/api/v1/repos/octo/api/issues/7/comments?per_page=100"
        );
        assert_eq!(seen[1].0, "POST");
        assert_eq!(seen[1].1, "/api/v1/repos/octo/api/issues/7/comments");
        for (_, path, auth) in &seen {
            assert_eq!(auth, "token fake-pat", "wrong scheme for {path}");
        }
    }

    #[test]
    fn a_comment_on_a_github_host_still_carries_bearer() {
        // The other half: fixing Gitea must not change what GitHub gets.
        let (base, handle) = fake_forge(vec![(200, "[]"), (201, r#"{"id": 5}"#)]);
        // No `/api/v1` suffix, so this reads as the GitHub dialect — the shape GitHub Enterprise
        // Server's `/api/v3` root also takes.
        let ctx = ctx_for(&base);
        post_or_update_comment(&ctx, "hello").expect("the comment posts");
        let seen = handle.join().unwrap();
        assert_eq!(seen[1].2, "Bearer fake-pat");
    }

    #[test]
    fn an_existing_comment_is_edited_in_place_on_gitea_too() {
        // Gitea's edit endpoint has the same shape as GitHub's — the comment addressed directly,
        // with no PR number — so the marker discipline carries over unchanged.
        let listing = format!(r#"[{{"id": 42, "body": "old {COMMENT_MARKER}"}}]"#);
        let leaked: &'static str = Box::leak(listing.into_boxed_str());
        let (base, handle) = fake_forge(vec![(200, leaked), (200, "{}")]);
        let ctx = ctx_for(&format!("{base}/api/v1"));
        post_or_update_comment(&ctx, "new").expect("the comment updates");
        let seen = handle.join().unwrap();
        assert_eq!(seen[1].0, "PATCH");
        assert_eq!(seen[1].1, "/api/v1/repos/octo/api/issues/comments/42");
        assert_eq!(seen[1].2, "token fake-pat");
    }

    #[test]
    fn gitea_gets_the_only_authorization_scheme_it_accepts() {
        // The bug this split exists to fix. Gitea's docs are explicit that a personal access token
        // needs the word `token`, and reserve `Bearer` for OAuth2 provider tokens — so the gate
        // sending `Bearer` unconditionally meant `--comment` could never work there. It failed as
        // a warning (the comment is best-effort by contract), so the verdict stayed right and the
        // comment silently never appeared, which is the hardest kind of breakage to notice.
        assert_eq!(
            Dialect::Gitea.authorization("fake-pat"),
            "token fake-pat",
            "Gitea answers 401/404 to `Bearer` for a personal access token"
        );
        assert_eq!(Dialect::Github.authorization("fake-pat"), "Bearer fake-pat");
    }

    #[test]
    fn the_dialect_is_read_off_the_api_root_each_forge_already_publishes() {
        // Gitea and Forgejo serve `/api/v1`; GitHub Enterprise Server serves `/api/v3`; the public
        // host serves the root. All three are distinct, and a runner sets the variable, so nobody
        // has to be asked which forge they are on.
        assert_eq!(
            Dialect::of("https://git.corp.internal/api/v1"),
            Dialect::Gitea
        );
        assert_eq!(
            Dialect::of("https://git.corp.internal/api/v1/"),
            Dialect::Gitea
        );
        assert_eq!(Dialect::of("https://api.github.com"), Dialect::Github);
        assert_eq!(
            Dialect::of("https://ghe.corp.internal/api/v3"),
            Dialect::Github
        );
        // A false positive costs a visible 401 against GitHub rather than a silent wrong answer,
        // which is the right direction for a heuristic to fail in.
    }

    #[test]
    fn parses_owner_repo() {
        assert_eq!(
            parse_repo("octocat/hello"),
            Some(("octocat".into(), "hello".into()))
        );
        assert!(parse_repo("noslash").is_none());
        assert!(parse_repo("/repo").is_none());
    }

    #[test]
    fn parses_pr_from_ref() {
        assert_eq!(parse_pr_from_ref("refs/pull/175/merge"), Some(175));
        assert_eq!(parse_pr_from_ref("refs/pull/9/head"), Some(9));
        assert_eq!(parse_pr_from_ref("refs/heads/main"), None);
    }

    #[test]
    fn pr_number_from_event_payload() {
        let pull = json!({ "pull_request": { "number": 42 } });
        assert_eq!(pr_number_from_event(&pull), Some(42));
        let top = json!({ "number": 7 });
        assert_eq!(pr_number_from_event(&top), Some(7));
        let none = json!({ "action": "opened" });
        assert_eq!(pr_number_from_event(&none), None);
    }

    #[test]
    fn finds_marked_comment_id() {
        let comments = json!([
            { "id": 1, "body": "just a comment" },
            { "id": 2, "body": format!("{COMMENT_MARKER}\nhello") },
        ]);
        assert_eq!(find_marker_comment(&comments, COMMENT_MARKER), Some(2));
        let none = json!([{ "id": 1, "body": "nope" }]);
        assert_eq!(find_marker_comment(&none, COMMENT_MARKER), None);
    }
}
