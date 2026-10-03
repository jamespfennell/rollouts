//! A GitHub client.

use std::sync;
use std::{collections::HashMap, time::Duration};

use chrono::TimeZone;
use chrono::Utc;

use crate::database;
use crate::metrics;

/// A GitHub Repo.
#[derive(Clone, Debug)]
pub struct Repo {
    /// GitHub user that owns the repo e.g. jamespfennell.
    pub user: String,
    /// Repository name e.g. rollouts.
    pub name: String,
}

impl serde::Serialize for Repo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let raw = format!("github.com/{}/{}", self.user, self.name,);
        str::serialize(&raw, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Repo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        use serde::de::Unexpected;
        let raw: &str = serde::Deserialize::deserialize(deserializer)?;
        let err = D::Error::invalid_value(
            Unexpected::Str(raw),
            &"a string of the form github.com/<user>/<name>",
        );
        let Some(raw_1) = raw.strip_prefix("github.com/") else {
            return Err(err);
        };
        let Some(i) = raw_1.find('/') else {
            return Err(err);
        };
        Ok(Self {
            user: raw_1[..i].to_string(),
            name: raw_1[i + 1..].to_string(),
        })
    }
}

/// A GitHub client.
///
/// This is a "good citizen" client that honors rate limiting information,
///     and tries to cache requests using the HTTP etag header.
pub struct Client<'a> {
    agent: ureq::Agent,
    /// Map from URL to the etag of the last response and the workflow run in it.
    /// The workflow run is None if the response contained no workflow runs;
    /// these responses are cached too so that subsequent requests are conditional.
    cache: sync::Mutex<HashMap<String, (String, Option<WorkflowRun>)>>,
    rate_limiter: sync::Mutex<RateLimiter>,
    db: &'a dyn database::DB,
}

impl<'a> Client<'a> {
    pub fn new(db: &'a dyn database::DB) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(1000))
            .build();
        let cache = sync::Mutex::new(
            database::get_typed(db, "github_client/cache")
                .unwrap_or_default()
                .unwrap_or_default(),
        );
        let rate_limiter = RateLimiter::new(db);
        rate_limiter.update_metrics();
        let rate_limiter = sync::Mutex::new(rate_limiter);
        Self {
            agent,
            cache,
            rate_limiter,
            db,
        }
    }

    /// Get the latest successful workflow run for the provided repo in branch.
    ///
    /// Returns an error if there have been no successful workflow runs for the branch.
    ///
    /// The provided auth token can be empty.
    /// See the commands on the auth token config for more information about this.
    ///
    /// The project name is only used to label metrics.
    pub fn get_latest_successful_workflow_run(
        &self,
        project: &str,
        repo: &Repo,
        branch: &str,
        auth_token: &str,
    ) -> Result<Option<WorkflowRun>, String> {
        let metrics = metrics::get();
        if let Err(err) = self.rate_limiter.lock().unwrap().check(auth_token) {
            metrics
                .github_rate_limited
                .with_label_values(&[project])
                .inc();
            return Err(err);
        }

        let url = format![
            "https://api.github.com/repos/{}/{}/actions/runs?branch={}&event=push&status=success&per_page=1&exclude_pull_requests=true",
            repo.user, repo.name, branch];
        let mut request = self
            .agent
            .get(&url)
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if !auth_token.is_empty() {
            request = request.set("Authorization", &format!["Bearer {auth_token}"]);
        }
        let old_etag = self.cache.lock().unwrap().get(&url).map(|(s, _)| s.clone());
        if let Some(etag) = &old_etag {
            request = request.set("if-none-match", etag);
            // Adding an authorization header with a dummy value seems
            // necessary in order for cached requests to not count against
            // the GitHub rate limit
            // https://stackoverflow.com/questions/60885496/github-304-responses-seem-to-count-against-rate-limit
            request = request.set("authorization", "none");
        }
        let timer = metrics.github_request_duration.start_timer();
        let response = request.call();
        timer.observe_duration();
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                metrics
                    .github_requests
                    .with_label_values(&[project, "error"])
                    .inc();
                return Err(format!("failed to make GitHub API request: {err}"));
            }
        };
        let result = if response.status() == 304 {
            "not_modified"
        } else {
            "ok"
        };
        metrics
            .github_requests
            .with_label_values(&[project, result])
            .inc();
        // Conditional requests are sent with a dummy authorization header (see above), and
        // GitHub accounts for these in a separate bucket. The rate limit headers on those
        // responses thus don't describe the quota of the auth token, so we ignore them.
        if old_etag.is_none() {
            if let Some(info) = RateLimitInfo::build(&response) {
                self.rate_limiter
                    .lock()
                    .unwrap()
                    .update(self.db, auth_token, info);
            }
        }

        if response.status() == 304 {
            if let Some((_, workflow_run)) = self.cache.lock().unwrap().get(&url) {
                return Ok(workflow_run.clone());
            }
        }

        let new_etag = response.header("etag").map(str::to_string);
        eprintln!("[github] url={url}, old_etag={old_etag:?}, new_etag={new_etag:?}");
        let body: String = match response.into_string() {
            Ok(body) => body,
            Err(err) => return Err(format!("failed to read GitHub API response: {err}")),
        };
        let mut build: Build = match serde_json::from_str(&body) {
            Ok(build) => build,
            Err(err) => {
                return Err(format!(
                    "failed to deserialize GitHub API json response: {err}"
                ))
            }
        };
        // GitHub only retains workflows for 1 year, so it's expected that projects with
        // no recent commits have no workflows.
        let workflow_run = build.workflow_runs.pop();

        // Update the cache before exiting.
        let mut cache = self.cache.lock().unwrap();
        if let (Some(workflow_run), Some((old_etag, Some(cached_workflow_run)))) =
            (&workflow_run, cache.get(&url))
        {
            if workflow_run.created_at < cached_workflow_run.created_at {
                return Err(format!["GitHub returned a stale workflow run! old_etag={old_etag}, new_etag={new_etag:?},\ncached_workflow={cached_workflow_run:#?}\nbody=<begin>\n{body}\n<end>"]);
            }
        }
        if let Some(etag) = new_etag {
            cache.insert(url, (etag, workflow_run.clone()));
        }
        use std::ops::Deref;
        database::set_typed(self.db, "github_client/cache".to_string(), cache.deref()).unwrap();
        Ok(workflow_run)
    }

    /// Refresh the rate limit information for the provided auth token.
    ///
    /// Most requests are conditional and don't report the auth token's quota, so this
    ///     is needed to keep the rate limit information current.
    /// Calls to this endpoint don't count against the GitHub rate limit.
    pub fn refresh_rate_limit(&self, auth_token: &str) -> Result<(), String> {
        let mut request = self
            .agent
            .get("https://api.github.com/rate_limit")
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if !auth_token.is_empty() {
            request = request.set("Authorization", &format!["Bearer {auth_token}"]);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(err) => return Err(format!("failed to make GitHub rate limit request: {err}")),
        };
        let body: String = match response.into_string() {
            Ok(body) => body,
            Err(err) => return Err(format!("failed to read GitHub rate limit response: {err}")),
        };
        let body: RateLimitResponse = match serde_json::from_str(&body) {
            Ok(body) => body,
            Err(err) => {
                return Err(format!(
                    "failed to deserialize GitHub rate limit response: {err}"
                ))
            }
        };
        let core = body.resources.core;
        let Some(reset) = chrono::DateTime::from_timestamp(core.reset, 0) else {
            return Err(format!("invalid rate limit reset time {}", core.reset));
        };
        let info = RateLimitInfo {
            limit: core.limit,
            remaining: core.remaining,
            used: core.used,
            reset,
            resource: "core".to_string(),
        };
        self.rate_limiter
            .lock()
            .unwrap()
            .update(self.db, auth_token, info);
        Ok(())
    }

    /// Returns the rate limit information for each resource.
    pub fn rate_limit_info(&self) -> HashMap<String, RateLimitInfo> {
        self.rate_limiter.lock().unwrap().resource_to_info.clone()
    }
}

#[derive(Debug, serde::Deserialize)]
struct Build {
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub display_title: String,
    pub run_number: u64,
    pub head_sha: String,
    pub html_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize)]
struct RateLimitResponse {
    resources: RateLimitResources,
}

#[derive(serde::Deserialize)]
struct RateLimitResources {
    core: RateLimitResource,
}

#[derive(serde::Deserialize)]
struct RateLimitResource {
    limit: u64,
    remaining: u64,
    used: u64,
    reset: i64,
}

/// Rate limiter state; this is persisted in the database.
///
/// This contains auth tokens and so must not be shown on the status page.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct RateLimiter {
    auth_token_to_resource: HashMap<String, String>,
    resource_to_info: HashMap<String, RateLimitInfo>,
}

impl RateLimiter {
    fn new(db: &dyn database::DB) -> Self {
        database::get_typed(db, "github_client/rate_limiter")
            .unwrap_or_default()
            .unwrap_or_default()
    }
    fn check(&self, auth_token: &str) -> Result<(), String> {
        let resource = match self.auth_token_to_resource.get(auth_token) {
            None => return Ok(()),
            Some(resource) => resource,
        };
        let info = match self.resource_to_info.get(resource) {
            None => return Ok(()),
            Some(info) => info,
        };
        if info.remaining > 0 {
            return Ok(());
        }
        // We were out of quota the call, but now we are after the quota reset time
        // so we can make the call.
        if info.reset <= chrono::Utc::now() {
            return Ok(());
        }
        Err(format!("reached GitHub API rate limit for this auth token; resource={resource}, limit={}, reset_time={}", info.limit, info.reset))
    }
    fn update(&mut self, db: &dyn database::DB, auth_token: &str, rate_limit_info: RateLimitInfo) {
        self.auth_token_to_resource
            .insert(auth_token.to_string(), rate_limit_info.resource.clone());
        self.resource_to_info
            .insert(rate_limit_info.resource.clone(), rate_limit_info);
        self.update_metrics();
        database::set_typed(db, "github_client/rate_limiter".to_string(), self).unwrap();
    }
    fn update_metrics(&self) {
        let metrics = metrics::get();
        for (resource, info) in &self.resource_to_info {
            let labels = [resource.as_str()];
            metrics
                .github_rate_limit_remaining
                .with_label_values(&labels)
                .set(info.remaining as i64);
            metrics
                .github_rate_limit_limit
                .with_label_values(&labels)
                .set(info.limit as i64);
            metrics
                .github_rate_limit_reset
                .with_label_values(&labels)
                .set(info.reset.timestamp());
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RateLimitInfo {
    pub limit: u64,
    pub remaining: u64,
    pub used: u64,
    pub reset: chrono::DateTime<Utc>,
    pub resource: String,
}

impl RateLimitInfo {
    fn build(response: &ureq::Response) -> Option<RateLimitInfo> {
        let resource = match response.header("x-ratelimit-resource") {
            None => return None,
            Some(s) => s.to_string(),
        };
        let mut info = RateLimitInfo {
            limit: 0,
            remaining: 0,
            used: 0,
            reset: chrono::Utc::now(),
            resource,
        };
        let mut reset_unix = 0_u64;
        for (u, header_name) in [
            (&mut info.limit, "x-ratelimit-limit"),
            (&mut info.remaining, "x-ratelimit-remaining"),
            (&mut info.used, "x-ratelimit-used"),
            (&mut reset_unix, "x-ratelimit-reset"),
        ] {
            *u = match response.header(header_name) {
                None => return None,
                Some(s) => match s.parse::<u64>() {
                    Ok(u) => u,
                    Err(_) => return None,
                },
            };
        }
        info.reset = match chrono::Utc.timestamp_opt(reset_unix as i64, 0) {
            chrono::LocalResult::Single(t) => t,
            _ => {
                return None;
            }
        };
        Some(info)
    }
}
