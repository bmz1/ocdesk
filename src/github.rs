//! The open pull request of a checkout's branch, read through the `gh` CLI
//! (its login, hosts and Enterprise setup come for free): checks, failure
//! logs, unresolved review threads and conversation comments. Read-only.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// Lines of failed-step log kept per check.
const LOG_LINES: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckState {
    Pass,
    Fail,
    Pending,
    Skipped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub workflow: String,
    pub state: CheckState,
    /// The check's page (an Actions job URL for GitHub Actions).
    pub link: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

impl Check {
    /// "4m 12s": run time so far, or in total once finished.
    pub fn duration(&self) -> Option<String> {
        let start = unix(self.started_at.as_deref()?)?;
        let end = match &self.completed_at {
            Some(t) => unix(t)?,
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64,
        };
        let s = (end - start).max(0);
        Some(match s {
            0..=59 => format!("{s}s"),
            60..=3599 => format!("{}m {}s", s / 60, s % 60),
            _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        })
    }

    /// The Actions job id, from `…/actions/runs/<run>/job/<job>`.
    pub fn job_id(&self) -> Option<u64> {
        let (_, rest) = self.link.split_once("/job/")?;
        rest.split(['/', '?', '#']).next()?.parse().ok()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub url: String,
}

/// An unresolved review thread on a line of the diff.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub path: String,
    pub line: Option<u64>,
    pub outdated: bool,
    pub diff_hunk: String,
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub draft: bool,
    /// APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED or empty.
    pub review_decision: String,
    pub head_sha: String,
    pub checks: Vec<Check>,
    pub threads: Vec<Thread>,
    /// Conversation comments and review summaries with a body.
    pub comments: Vec<Comment>,
}

impl PullRequest {
    pub fn count(&self, state: CheckState) -> usize {
        self.checks.iter().filter(|c| c.state == state).count()
    }
}

/// Seconds since the epoch of an RFC 3339 UTC time (`2026-11-08T09:09:51Z`).
fn unix(t: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

const GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

async fn gh(dir: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("gh")
        .args(args)
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .kill_on_drop(true)
        .output();
    // A hung `gh` (network, auth prompt) would otherwise keep the PR entry
    // "loading" forever, which blocks every later refresh. Timing out drops
    // the child, which kills it.
    let out = tokio::time::timeout(GH_TIMEOUT, out)
        .await
        .map_err(|_| anyhow::anyhow!("gh {} timed out", args.first().unwrap_or(&"")))??;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether the `gh` CLI is installed (not whether it is logged in).
pub async fn available() -> bool {
    let status = tokio::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    tokio::time::timeout(GH_TIMEOUT, status)
        .await
        .is_ok_and(|s| s.is_ok_and(|s| s.success()))
}

const THREADS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100){nodes{isResolved isOutdated path line originalLine comments(first:50){nodes{author{login} body url diffHunk}}}} reviews(first:50){nodes{author{login} body url}}}}}";

/// The pull request for `dir`'s current branch; `None` when there is none.
pub async fn pull_request(dir: &Path) -> Result<Option<PullRequest>> {
    let view = match gh(
        dir,
        &[
            "pr",
            "view",
            "--json",
            "number,title,url,state,isDraft,reviewDecision,headRefOid,statusCheckRollup,comments",
        ],
    )
    .await
    {
        Ok(v) => v,
        Err(e) if e.to_string().contains("no pull requests found") => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut pr = parse_view(&serde_json::from_str(&view)?)
        .ok_or_else(|| anyhow!("unexpected `gh pr view` output"))?;
    let number = pr.number.to_string();
    let threads = gh(
        dir,
        &[
            "api",
            "graphql",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-F",
            &format!("number={number}"),
            "-f",
            &format!("query={THREADS_QUERY}"),
        ],
    )
    .await?;
    let (threads, reviews) = parse_threads(&serde_json::from_str(&threads)?);
    pr.threads = threads;
    // Review summaries ("Request changes: …") read like conversation comments.
    pr.comments.extend(reviews);
    Ok(Some(pr))
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn parse_view(v: &Value) -> Option<PullRequest> {
    let checks = v["statusCheckRollup"]
        .as_array()
        .map(|a| a.iter().map(parse_check).collect())
        .unwrap_or_default();
    let comments = v["comments"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| Comment {
                    author: c["author"]["login"].as_str().unwrap_or("ghost").into(),
                    body: s(c, "body"),
                    url: s(c, "url"),
                })
                .filter(|c| !c.body.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(PullRequest {
        number: v["number"].as_u64()?,
        title: s(v, "title"),
        url: s(v, "url"),
        state: s(v, "state"),
        draft: v["isDraft"].as_bool().unwrap_or(false),
        review_decision: s(v, "reviewDecision"),
        head_sha: s(v, "headRefOid"),
        checks,
        threads: Vec::new(),
        comments,
    })
}

fn parse_check(c: &Value) -> Check {
    // GitHub's zero time stands for "not set".
    let time = |k: &str| Some(s(c, k)).filter(|t| !t.is_empty() && !t.starts_with("0001"));
    if c["__typename"] == "StatusContext" {
        let state = match c["state"].as_str().unwrap_or_default() {
            "SUCCESS" => CheckState::Pass,
            "FAILURE" | "ERROR" => CheckState::Fail,
            _ => CheckState::Pending,
        };
        return Check {
            name: s(c, "context"),
            workflow: String::new(),
            state,
            link: s(c, "targetUrl"),
            started_at: time("startedAt"),
            completed_at: None,
        };
    }
    let state = if c["status"] != "COMPLETED" {
        CheckState::Pending
    } else {
        match c["conclusion"].as_str().unwrap_or_default() {
            "SUCCESS" | "NEUTRAL" => CheckState::Pass,
            "SKIPPED" => CheckState::Skipped,
            _ => CheckState::Fail,
        }
    };
    Check {
        name: s(c, "name"),
        workflow: s(c, "workflowName"),
        state,
        link: s(c, "detailsUrl"),
        started_at: time("startedAt"),
        completed_at: time("completedAt"),
    }
}

fn parse_threads(v: &Value) -> (Vec<Thread>, Vec<Comment>) {
    let pr = &v["data"]["repository"]["pullRequest"];
    let comment = |c: &Value| Comment {
        author: c["author"]["login"].as_str().unwrap_or("ghost").into(),
        body: s(c, "body"),
        url: s(c, "url"),
    };
    let threads = pr["reviewThreads"]["nodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t["isResolved"] != true)
                .map(|t| {
                    let nodes = t["comments"]["nodes"].as_array();
                    Thread {
                        path: s(t, "path"),
                        line: t["line"].as_u64().or_else(|| t["originalLine"].as_u64()),
                        outdated: t["isOutdated"] == true,
                        diff_hunk: nodes
                            .and_then(|n| n.first())
                            .map(|c| s(c, "diffHunk"))
                            .unwrap_or_default(),
                        comments: nodes
                            .map(|n| n.iter().map(comment).collect())
                            .unwrap_or_default(),
                    }
                })
                .filter(|t| !t.comments.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let reviews = pr["reviews"]["nodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(comment)
                .filter(|c| !c.body.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    (threads, reviews)
}

/// Why a check failed: the tail of its failed steps' log, or GitHub's
/// annotations when there is no log (a job that never started).
pub async fn failure_log(dir: &Path, check: &Check) -> Result<String> {
    let Some(job) = check.job_id() else {
        bail!("No log for this check; open it on GitHub.");
    };
    let job = job.to_string();
    if let Ok(log) = gh(dir, &["run", "view", "--job", &job, "--log-failed"]).await {
        let tail = log_tail(&log, LOG_LINES);
        if !tail.trim().is_empty() {
            return Ok(tail);
        }
    }
    let notes = gh(
        dir,
        &[
            "api",
            &format!("repos/{{owner}}/{{repo}}/check-runs/{job}/annotations"),
        ],
    )
    .await?;
    let text = annotations(&serde_json::from_str(&notes)?);
    if text.is_empty() {
        bail!("GitHub has no log or annotations for this check yet.");
    }
    Ok(text)
}

/// `gh run view --log-failed` lines are `job\tstep\t<timestamp> text`;
/// keep the text of the last `n`.
fn log_tail(log: &str, n: usize) -> String {
    let lines: Vec<&str> = log
        .lines()
        .map(|l| l.splitn(3, '\t').nth(2).unwrap_or(l))
        .map(|l| match l.split_once(' ') {
            Some((ts, rest)) if ts.len() >= 20 && ts.ends_with('Z') && ts.contains('T') => rest,
            _ => l,
        })
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn annotations(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| {
                    let msg = n["message"].as_str()?.trim();
                    (!msg.is_empty()).then(|| {
                        let level = n["annotation_level"].as_str().unwrap_or("note");
                        format!("{level}: {msg}")
                    })
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The prompt for "fix these failing checks".
pub fn ci_prompt(pr: &PullRequest, failures: &[(Check, String)]) -> String {
    let sha = pr.head_sha.get(..8).unwrap_or(&pr.head_sha);
    let mut s = format!(
        "CI is failing on PR #{} ({}) at commit {sha}. Find the cause and fix it; \
         don't just silence or skip the check.\n",
        pr.number, pr.title
    );
    for (check, log) in failures {
        let name = if check.workflow.is_empty() {
            check.name.clone()
        } else {
            format!("{} / {}", check.workflow, check.name)
        };
        s.push_str(&format!("\n## {name}\n{}\n", check.link));
        if !log.trim().is_empty() {
            s.push_str(&format!("```\n{}\n```\n", log.trim_end()));
        }
    }
    s
}

/// The prompt for "address these review comments".
pub fn comments_prompt(pr: &PullRequest, threads: &[&Thread], comments: &[&Comment]) -> String {
    let mut s = format!(
        "Please address these review comments on PR #{} ({}):\n",
        pr.number, pr.title
    );
    for t in threads {
        let at = match t.line {
            Some(l) => format!("{}:{l}", t.path),
            None => t.path.clone(),
        };
        s.push_str(&format!(
            "\n## {at}{}\n",
            if t.outdated { " (outdated)" } else { "" }
        ));
        if !t.diff_hunk.is_empty() {
            let hunk: Vec<&str> = t.diff_hunk.lines().collect();
            let hunk = &hunk[hunk.len().saturating_sub(6)..];
            s.push_str(&format!("```diff\n{}\n```\n", hunk.join("\n")));
        }
        for c in &t.comments {
            s.push_str(&format!("- @{}: {}\n", c.author, c.body.trim()));
        }
    }
    if !comments.is_empty() {
        s.push_str("\n## Conversation\n");
        for c in comments {
            s.push_str(&format!("- @{}: {}\n", c.author, c.body.trim()));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view() -> Value {
        json!({
            "number": 15, "title": "UI polish", "url": "https://github.com/o/r/pull/15",
            "state": "OPEN", "isDraft": false, "reviewDecision": "",
            "headRefOid": "75d40bf3e092a3a6f8322336a8e50f731a35194b",
            "comments": [{"author": {"login": "amy"}, "body": "nice", "url": "u"},
                         {"author": {"login": "bot"}, "body": " ", "url": "u2"}],
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "Linux", "status": "IN_PROGRESS",
                 "conclusion": "", "workflowName": "CI",
                 "startedAt": "2026-11-08T09:09:51Z", "completedAt": "0001-01-01T00:00:00Z",
                 "detailsUrl": "https://github.com/o/r/actions/runs/42/job/128"},
                {"__typename": "CheckRun", "name": "macOS", "status": "COMPLETED",
                 "conclusion": "CANCELLED", "workflowName": "CI",
                 "detailsUrl": "https://github.com/o/r/actions/runs/42/job/129"},
                {"__typename": "CheckRun", "name": "docs", "status": "COMPLETED",
                 "conclusion": "SKIPPED", "detailsUrl": ""},
                {"__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS",
                 "targetUrl": "https://ci.example/1"}
            ]
        })
    }

    #[::core::prelude::v1::test]
    fn parses_pr_and_checks() {
        let pr = parse_view(&view()).unwrap();
        assert_eq!(pr.number, 15);
        assert_eq!(pr.comments.len(), 1);
        let states: Vec<_> = pr.checks.iter().map(|c| c.state.clone()).collect();
        assert_eq!(
            states,
            [
                CheckState::Pending,
                CheckState::Fail,
                CheckState::Skipped,
                CheckState::Pass
            ]
        );
        assert_eq!(pr.checks[0].completed_at, None);
        assert_eq!(pr.checks[1].job_id(), Some(129));
        assert_eq!(pr.checks[3].job_id(), None);
        assert_eq!(pr.count(CheckState::Fail), 1);
        assert_eq!(unix("1970-01-02T00:00:01Z"), Some(86401));
        assert_eq!(unix("2026-11-08T09:09:51Z"), Some(1794128991));
        let done = Check {
            started_at: Some("2026-11-08T09:09:51Z".into()),
            completed_at: Some("2026-11-08T09:14:03Z".into()),
            ..pr.checks[0].clone()
        };
        assert_eq!(done.duration().as_deref(), Some("4m 12s"));
    }

    #[::core::prelude::v1::test]
    fn keeps_unresolved_threads_and_review_bodies() {
        let v = json!({"data": {"repository": {"pullRequest": {
            "reviewThreads": {"nodes": [
                {"isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 12,
                 "comments": {"nodes": [
                    {"author": {"login": "amy"}, "body": "leaks", "url": "u", "diffHunk": "@@ -1 +1 @@\n+x"},
                    {"author": {"login": "bo"}, "body": "agreed", "url": "u2", "diffHunk": ""}]}},
                {"isResolved": true, "path": "src/b.rs", "line": 1,
                 "comments": {"nodes": [{"author": {"login": "amy"}, "body": "done", "url": "u3"}]}},
                {"isResolved": false, "isOutdated": true, "path": "src/c.rs", "line": null, "originalLine": 7,
                 "comments": {"nodes": [{"author": null, "body": "old", "url": "u4"}]}}
            ]},
            "reviews": {"nodes": [
                {"author": {"login": "amy"}, "body": "Please fix", "url": "r1"},
                {"author": {"login": "bo"}, "body": "", "url": "r2"}
            ]}
        }}}});
        let (threads, reviews) = parse_threads(&v);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].comments.len(), 2);
        assert_eq!(threads[1].line, Some(7));
        assert!(threads[1].outdated);
        assert_eq!(threads[1].comments[0].author, "ghost");
        assert_eq!(reviews.len(), 1);

        let pr = parse_view(&view()).unwrap();
        let p = comments_prompt(&pr, &[&threads[0]], &[&reviews[0]]);
        assert!(p.contains("## src/a.rs:12"));
        assert!(p.contains("- @amy: leaks"));
        assert!(p.contains("```diff\n@@ -1 +1 @@\n+x\n```"));
        assert!(p.contains("## Conversation\n- @amy: Please fix"));
    }

    #[::core::prelude::v1::test]
    fn trims_failed_logs_and_reads_annotations() {
        let log = "Linux\tbuild\t2026-11-08T09:10:00.1234567Z error[E0425]: nope\n\
                   Linux\tbuild\t2026-11-08T09:10:01.0000000Z exit 101";
        assert_eq!(log_tail(log, 80), "error[E0425]: nope\nexit 101");
        assert_eq!(log_tail(log, 1), "exit 101");
        let notes = json!([{"annotation_level": "failure",
                            "message": "The configuration 'macos-14-arm64-us-xl' is not supported"}]);
        assert_eq!(
            annotations(&notes),
            "failure: The configuration 'macos-14-arm64-us-xl' is not supported"
        );
        let pr = parse_view(&view()).unwrap();
        let p = ci_prompt(&pr, &[(pr.checks[1].clone(), "boom".into())]);
        assert!(p.contains("PR #15 (UI polish) at commit 75d40bf3"));
        assert!(p.contains(
            "## CI / macOS\nhttps://github.com/o/r/actions/runs/42/job/129\n```\nboom\n```"
        ));
    }
}
