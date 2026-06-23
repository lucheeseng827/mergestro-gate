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

/// Apply the standard GitHub API headers to a request.
fn with_headers(req: ureq::Request, ctx: &GithubContext) -> ureq::Request {
    req.set("Authorization", &format!("Bearer {}", ctx.token))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "slop-gate")
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
