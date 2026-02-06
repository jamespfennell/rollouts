//! A GitHub client.

use std::sync;
use std::{collections::HashMap, time::Duration};

use chrono::TimeZone;
use chrono::Utc;

use crate::database;

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
    cache: sync::Mutex<HashMap<String, (String, WorkflowRun)>>,
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
        let rate_limiter = sync::Mutex::new(RateLimiter::new(db));
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
    pub fn get_latest_successful_workflow_run(
        &self,
        repo: &Repo,
        branch: &str,
        auth_token: &str,
    ) -> Result<Option<WorkflowRun>, String> {
        self.rate_limiter.lock().unwrap().check(auth_token)?;

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
        if let Some((etag, _)) = self.cache.lock().unwrap().get(&url) {
            request = request.set("if-none-match", etag);
            // Adding an authorization header with a dummy value seems
            // necessary in order for cached requests to not count against
            // the GitHub rate limit
            // https://stackoverflow.com/questions/60885496/github-304-responses-seem-to-count-against-rate-limit
            request = request.set("authorization", "none");
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(err) => return Err(format!("failed to make GitHub API request: {err}")),
        };
        self.rate_limiter
            .lock()
            .unwrap()
            .update(self.db, auth_token, &response);

        if response.status() == 304 {
            if let Some((_, workflow_run)) = self.cache.lock().unwrap().get(&url) {
                return Ok(Some(workflow_run.clone()));
            }
        }

        let etag = response.header("etag").map(str::to_string);
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
        let workflow_run = match build.workflow_runs.pop() {
            Some(workflow_run) => workflow_run,
            // GitHub only retains workflows for 1 year, so it's expected that projects with
            // no recent commits have no workflows.
            None => return Ok(None),
        };

        // Update the cache before exiting.
        let mut cache = self.cache.lock().unwrap();
        if let Some((old_etag, cached_workflow_run)) = cache.get(&url) {
            if workflow_run.created_at < cached_workflow_run.created_at {
                return Err(format!["GitHub returned a stale workflow run! old_etag={old_etag}, new_etag={etag:?},\ncached_workflow={cached_workflow_run:#?}\nbody=<begin>\n{body}\n<end>"]);
            }
        }
        if let Some(etag) = etag {
            cache.insert(url, (etag, workflow_run.clone()));
        }
        use std::ops::Deref;
        database::set_typed(self.db, "github_client/cache".to_string(), cache.deref()).unwrap();
        Ok(Some(workflow_run))
    }

    pub fn rate_limit_info(&self) -> RateLimiter {
        self.rate_limiter.lock().unwrap().clone()
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

#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct RateLimiter {
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
    fn update(&mut self, db: &dyn database::DB, auth_token: &str, response: &ureq::Response) {
        let Some(rate_limit_info) = RateLimitInfo::build(response) else {
            return;
        };
        self.auth_token_to_resource
            .insert(auth_token.to_string(), rate_limit_info.resource.clone());
        self.resource_to_info
            .insert(rate_limit_info.resource.clone(), rate_limit_info);
        database::set_typed(db, "github_client/rate_limiter".to_string(), self).unwrap();
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
