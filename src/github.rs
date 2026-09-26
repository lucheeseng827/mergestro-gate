// SPDX-License-Identifier: Apache-2.0
//! Minimal, synchronous GitHub REST client for posting the survivor report as
//! a PR comment. Idempotent: it finds a prior comment by [`COMMENT_MARKER`] and
//! updates it in place, so re-runs and merge-queue re-triggers don't stack
//! duplicate comments.
//!
//! Kept deliberately small — `ureq` + `serde_json`, no async runtime.

use std::collections::{BTreeSet, HashMap};
use std::env;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::report::{GateReport, COMMENT_MARKER};

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
    let existing = find_comment(ctx)?.map(|(id, _)| id);
    upsert_comment(ctx, existing, body)
}

/// The existing slop-gate comment on the PR, as `(id, body)`: its body carries
/// the previous run's survivor state (see [`crate::report::parse_survivor_state`]).
pub fn find_comment(ctx: &GithubContext) -> Result<Option<(u64, String)>> {
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
    Ok(find_marker_comment(&comments, COMMENT_MARKER).map(|id| {
        let body = comments
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|c| c.get("id").and_then(Value::as_u64) == Some(id))
            })
            .and_then(|c| c.get("body")?.as_str())
            .unwrap_or_default()
            .to_string();
        (id, body)
    }))
}

/// Update comment `existing`, or create one when there is none.
pub fn upsert_comment(ctx: &GithubContext, existing: Option<u64>, body: &str) -> Result<()> {
    let agent = ureq::AgentBuilder::new().build();
    if let Some(id) = existing {
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
/// Post the run to the PR: the summary comment, and with `inline_diff` a
/// review of the survivors that are new since the last run.
///
/// The previous comment carries the last run's survivors, so it is read first
/// and this one can say what is new, still open and resolved since then.
///
/// **The summary always goes out.** The inline review is an extra, and it can
/// fail — Gitea has no compatible review API, a token may lack review
/// permission, GitHub may refuse a line or be briefly unavailable — so its
/// failure is a warning, never a reason to skip the summary. The summary
/// records which survivors have an inline comment *only once one has posted*,
/// so the survivors of a failed review are tried again on the next run.
pub fn publish(ctx: &GithubContext, report: &GateReport, inline_diff: Option<&str>) -> Result<()> {
    let existing = find_comment(ctx)?;
    let mut report = report.clone();
    let body = existing.as_ref().map(|(_, body)| body.as_str());
    report.previous_survivors = body.and_then(crate::report::parse_survivor_state);
    report.previous_inlined = body.and_then(crate::report::parse_inlined_state);
    if let Some(diff) = inline_diff {
        if let Some((review, keys)) = inline_review(&report, &commentable_lines(diff)) {
            match post_review(ctx, &review) {
                Ok(()) => report.inlined = keys,
                Err(e) => eprintln!(
                    "slop-gate: warning: could not post the inline review (retried next run): {e:#}"
                ),
            }
        }
    }
    upsert_comment(ctx, existing.map(|(id, _)| id), &report.render_markdown())
}

/// Head-side lines each file's diff shows (hunk ranges, context included):
/// the only lines GitHub accepts a review comment on. One comment outside them
/// and the API rejects the whole review.
pub fn commentable_lines(unified_diff: &str) -> HashMap<String, BTreeSet<u32>> {
    let mut out: HashMap<String, BTreeSet<u32>> = HashMap::new();
    let mut file: Option<String> = None;
    // Lines the current hunk still has on each side. While either is non-zero
    // every line is hunk content — an added line that reads `++ b/x` shows up
    // as `+++ b/x` and must not be taken for the next file's header.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in unified_diff.lines() {
        if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'+') => new_left = new_left.saturating_sub(1),
                Some(b'-') => old_left = old_left.saturating_sub(1),
                Some(b'\\') => {} // "\ No newline at end of file"
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ b/") {
            file = Some(path.to_string());
        } else if let (Some(f), Some(hunk)) = (&file, line.strip_prefix("@@ ")) {
            // `@@ -a,b +c,d @@`: each side starts at a / c and spans b / d lines
            // (1 when the count is omitted).
            let range = |sign: char| -> (u32, u32) {
                let Some(tok) = hunk.split_whitespace().find(|t| t.starts_with(sign)) else {
                    return (0, 0);
                };
                let mut nums = tok[1..].splitn(2, ',');
                let start = nums.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let len = nums.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                (start, len)
            };
            let (start, len) = range('+');
            (old_left, new_left) = (range('-').1, len);
            if start > 0 {
                out.entry(f.clone()).or_default().extend(start..start + len);
            }
        }
    }
    out
}

/// A PR review with one comment per survivor that **has no inline comment
/// yet** — per the previous comment's record of those that posted
/// ([`GateReport::previous_inlined`]) — and sits on a line the diff shows.
/// Returns the review and the survivor keys it covers, so the caller records
/// them as commented only once the review has actually posted; a review that
/// failed leaves its survivors eligible next run. `None` when there is nothing
/// to post.
pub fn inline_review(
    report: &GateReport,
    commentable: &HashMap<String, BTreeSet<u32>>,
) -> Option<(Value, Vec<String>)> {
    let done = report.previous_inlined.as_deref().unwrap_or_default();
    let (keys, comments): (Vec<String>, Vec<Value>) = report
        .survivor_keys()
        .into_iter()
        .filter(|(key, _)| !done.contains(key))
        .filter(|(_, m)| {
            commentable
                .get(&m.file)
                .is_some_and(|lines| lines.contains(&m.line))
        })
        .map(|(key, m)| {
            let comment = json!({
                "path": m.file,
                "line": m.line,
                "side": "RIGHT",
                "body": format!(
                    "**Surviving mutation ({}):** the tests still pass with `{}` applied \
                     here. Add a test that fails when this line's behaviour changes.",
                    crate::severity::classify(m).label(),
                    m.description
                ),
            });
            (key, comment)
        })
        .unzip();
    if comments.is_empty() {
        return None;
    }
    let review = json!({
        "event": "COMMENT",
        "body": format!(
            "Mergestro Gate: {} surviving mutation(s) on changed lines — details in the summary comment.",
            comments.len()
        ),
        "comments": comments,
    });
    Some((review, keys))
}

/// Post `review` (from [`inline_review`]) on the PR. GitHub's review API only:
/// Gitea's takes a different comment shape, so it is refused rather than sent
/// something it would reject.
pub fn post_review(ctx: &GithubContext, review: &Value) -> Result<()> {
    anyhow::ensure!(
        Dialect::of(&ctx.api) == Dialect::Github,
        "inline review comments are only supported on GitHub (the summary comment still works)"
    );
    let url = format!(
        "{}/repos/{}/{}/pulls/{}/reviews",
        ctx.api, ctx.owner, ctx.repo, ctx.pr
    );
    with_headers(ureq::AgentBuilder::new().build().post(&url), ctx)
        .send_json(review.clone())
        .context("posting the inline review")?;
    Ok(())
}

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

    const DIFF: &str = "\
diff --git a/src/x.rs b/src/x.rs
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,3 +1,4 @@
 fn a() {}
+fn b() -> i32 { 1 + 1 }
 fn c() {}
 fn d() {}
@@ -20 +21,2 @@
-old
+fn e() -> i32 { 2 }
+fn f() {}
";

    fn survivor(line: u32, desc: &str) -> crate::report::Mutant {
        crate::report::Mutant::parse_name(&format!("src/x.rs:{line}:5: {desc}")).unwrap()
    }

    #[test]
    fn commentable_lines_are_the_head_side_of_each_hunk() {
        let lines = commentable_lines(DIFF);
        let x: Vec<u32> = lines["src/x.rs"].iter().copied().collect();
        assert_eq!(x, [1, 2, 3, 4, 21, 22]);
    }

    #[test]
    fn an_added_line_that_looks_like_a_file_header_stays_hunk_content() {
        // The added line `++ b/fake.rs` is `+++ b/fake.rs` in the diff. Taken for
        // a header, it would move the next hunk onto a file that does not exist.
        let diff = "\
diff --git a/src/x.rs b/src/x.rs
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,2 +1,3 @@
 fn a() {}
+++ b/fake.rs
 fn c() {}
@@ -10 +11 @@
-fn d() {}
+fn e() {}
diff --git a/src/y.rs b/src/y.rs
--- a/src/y.rs
+++ b/src/y.rs
@@ -1 +1 @@
-old
+new
";
        let lines = commentable_lines(diff);
        assert!(!lines.contains_key("fake.rs"), "{lines:?}");
        let x: Vec<u32> = lines["src/x.rs"].iter().copied().collect();
        assert_eq!(x, [1, 2, 3, 11]);
        assert_eq!(lines["src/y.rs"].iter().copied().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn only_uncommented_survivors_on_diff_lines_get_an_inline_comment() {
        let mut r = GateReport::new("main", "HEAD");
        r.survivors = vec![
            survivor(2, "replace + with -"),  // on a diff line, not commented yet
            survivor(21, "replace b with 0"), // on a diff line, already commented
            survivor(40, "replace f with ()"), // not on a diff line: summary only
        ];
        r.previous_inlined = Some(vec!["src/x.rs|replace b with 0|0".into()]);
        let (review, keys) = inline_review(&r, &commentable_lines(DIFF)).expect("one comment");
        assert_eq!(keys, ["src/x.rs|replace + with -|0"]);
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["path"], "src/x.rs");
        assert_eq!(comments[0]["line"], 2);
        assert_eq!(comments[0]["side"], "RIGHT");
        assert!(comments[0]["body"]
            .as_str()
            .unwrap()
            .contains("`replace + with -`"));
        assert_eq!(review["event"], "COMMENT");
    }

    #[test]
    fn a_failed_review_is_retried_and_a_posted_one_is_not_repeated() {
        let lines = commentable_lines(DIFF);
        let mut r = GateReport::new("main", "HEAD");
        r.survivors = vec![survivor(2, "replace + with -")];
        let (_, keys) = inline_review(&r, &lines).expect("first run comments it");

        // The review failed: the summary records nothing as commented...
        let failed = r.render_markdown();
        let mut next = r.clone();
        next.previous_survivors = crate::report::parse_survivor_state(&failed);
        next.previous_inlined = crate::report::parse_inlined_state(&failed);
        // ...so the next run, which knows the survivor, still comments it.
        assert!(next.since_last_run().is_some_and(|d| d.new.is_empty()));
        assert!(inline_review(&next, &lines).is_some(), "retried");

        // The review posted: the summary records it, and the next run is quiet.
        next.inlined = keys;
        let posted = next.render_markdown();
        let mut after = r.clone();
        after.previous_inlined = crate::report::parse_inlined_state(&posted);
        assert!(inline_review(&after, &lines).is_none(), "not repeated");
    }

    #[test]
    fn a_failed_inline_review_still_posts_the_summary() {
        // List comments (none yet), the review is refused with a 422, and the
        // summary must still be created.
        let (api, server) = fake_forge(vec![
            (200, "[]"),
            (422, r#"{"message":"line must be part of the diff"}"#),
            (201, "{}"),
        ]);
        let mut r = GateReport::new("main", "HEAD");
        r.survivors = vec![survivor(2, "replace + with -")];
        publish(&ctx_for(&api), &r, Some(DIFF)).expect("the summary goes out");
        let seen = server.join().unwrap();
        let calls: Vec<(&str, &str)> = seen
            .iter()
            .map(|(m, p, _)| (m.as_str(), p.split('?').next().unwrap()))
            .collect();
        assert_eq!(
            calls,
            [
                ("GET", "/repos/octo/api/issues/7/comments"),
                ("POST", "/repos/octo/api/pulls/7/reviews"),
                ("POST", "/repos/octo/api/issues/7/comments"),
            ]
        );
    }

    #[test]
    fn on_gitea_the_summary_goes_out_without_a_review() {
        let (api, server) = fake_forge(vec![(200, "[]"), (201, "{}")]);
        let mut r = GateReport::new("main", "HEAD");
        r.survivors = vec![survivor(2, "replace + with -")];
        publish(&ctx_for(&format!("{api}/api/v1")), &r, Some(DIFF)).expect("summary");
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2, "no review request reaches Gitea: {seen:?}");
        assert_eq!(seen[1].0, "POST");
        assert!(seen[1].1.ends_with("/issues/7/comments"));
    }

    #[test]
    fn a_review_goes_to_the_pulls_endpoint_and_gitea_is_refused() {
        let (api, server) = fake_forge(vec![(200, "{}")]);
        post_review(&ctx_for(&api), &json!({"event": "COMMENT", "comments": []})).unwrap();
        let seen = server.join().unwrap();
        assert_eq!(seen[0].0, "POST");
        assert_eq!(seen[0].1, "/repos/octo/api/pulls/7/reviews");
        assert_eq!(seen[0].2, "Bearer fake-pat");
        assert!(post_review(&ctx_for("http://gitea.local/api/v1"), &json!({})).is_err());
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
