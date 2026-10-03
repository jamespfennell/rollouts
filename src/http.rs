//! Module http contains a HTTP service that exposes status information about the agent.

use std::sync;
use std::thread;

use crate::{github, project};

pub struct Service<'a> {
    hostname: String,
    github_client: &'a github::Client<'a>,
    project_manager: &'a project::Manager<'a>,
}

impl<'a> Service<'a> {
    pub fn new(
        hostname: String,
        github_client: &'a github::Client,
        project_manager: &'a project::Manager,
    ) -> Self {
        Self {
            hostname,
            github_client,
            project_manager,
        }
    }
    pub fn start<'scope>(&'a self, scope: &'scope thread::Scope<'scope, 'a>, port: u16) -> Stopper {
        let server = sync::Arc::new(tiny_http::Server::http(format!("0.0.0.0:{port}")).unwrap());
        let server_cloned = server.clone();
        scope.spawn(move || {
            eprintln!("[http_service] listening for requests on port {port}");
            for request in server_cloned.incoming_requests() {
                match request.method() {
                    tiny_http::Method::Get => {}
                    _ => {
                        let response = tiny_http::Response::empty(tiny_http::StatusCode(405));
                        request.respond(response).unwrap();
                        continue;
                    }
                }
                let path = request.url().split('?').next().unwrap_or("/");
                let (data, content_type) = match path {
                    "/" | "/index.html" => (self.index_html(), "text/html; charset=UTF-8"),
                    "/status.json" => (self.status_json(), "application/json; charset=UTF-8"),
                    "/metrics" => (
                        crate::metrics::get().render(),
                        "text/plain; version=0.0.4; charset=UTF-8",
                    ),
                    _ => {
                        let response = tiny_http::Response::empty(tiny_http::StatusCode(404));
                        request.respond(response).unwrap();
                        continue;
                    }
                };
                let header = tiny_http::Header::from_bytes("Content-Type", content_type).unwrap();
                let response = tiny_http::Response::from_string(data).with_header(header);
                request.respond(response).unwrap();
            }
        });
        Stopper { server }
    }
    fn index_html(&self) -> String {
        STATUS_DOT_HTML.replace("__AGENTS__", "[]")
    }
    fn status_json(&self) -> String {
        let data = Data {
            hostname: self.hostname.clone(),
            projects: self.project_manager.projects(),
            rate_limit_info: RateLimitInfo {
                resource_to_info: self.github_client.rate_limit_info(),
            },
        };
        serde_json::to_string_pretty(&data).unwrap()
    }
}

pub struct Stopper {
    server: sync::Arc<tiny_http::Server>,
}

impl Stopper {
    pub fn stop(self) {
        eprintln!("[http_service] shutdown signal received");
        self.server.unblock();
        eprintln!("[http_service] unblocked listening thread; waiting to stop");
    }
}

pub fn start_ui<'scope>(
    scope: &'scope thread::Scope<'scope, '_>,
    port: u16,
    agents: Vec<String>,
) -> Stopper {
    let agents_json = serde_json::to_string(&agents).unwrap();
    let html = STATUS_DOT_HTML.replace("__AGENTS__", &agents_json);
    let server = sync::Arc::new(tiny_http::Server::http(format!("0.0.0.0:{port}")).unwrap());
    let server_cloned = server.clone();
    scope.spawn(move || {
        eprintln!("[http_service] ui-only server listening on port {port}");
        for request in server_cloned.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("/");
            let (status, body, content_type) = match (request.method(), path) {
                (tiny_http::Method::Get, "/" | "/index.html") => {
                    (200u16, html.clone(), "text/html; charset=UTF-8")
                }
                _ => (404, "Not found".to_string(), "text/plain"),
            };
            let header = tiny_http::Header::from_bytes("Content-Type", content_type).unwrap();
            let response = tiny_http::Response::from_string(body)
                .with_status_code(tiny_http::StatusCode(status))
                .with_header(header);
            request.respond(response).unwrap();
        }
    });
    Stopper { server }
}

static STATUS_DOT_HTML: &str = include_str!("status.html");

#[derive(Debug, serde::Serialize)]
struct Data {
    hostname: String,
    projects: Vec<project::Project>,
    rate_limit_info: RateLimitInfo,
}

#[derive(Debug, serde::Serialize)]
struct RateLimitInfo {
    resource_to_info: std::collections::HashMap<String, github::RateLimitInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config, database, email};

    const TOKEN: &str = "ghp_test123";

    #[test]
    fn status_json_does_not_contain_auth_tokens() {
        let db = database::new_in_memory_db();
        // State as it would have been written before auth tokens were redacted.
        database::set_typed(
            &db,
            "github_client/rate_limiter".to_string(),
            &serde_json::json!({
                "auth_token_to_resource": {TOKEN: "core"},
                "resource_to_info": {"core": {
                    "limit": 5000, "remaining": 4999, "used": 1,
                    "reset": "2026-01-01T00:00:00Z", "resource": "core",
                }},
            }),
        )
        .unwrap();
        database::set_typed(
            &db,
            "project_manager/projects/project".to_string(),
            &serde_json::json!({
                "config": {"name": "project", "repo": "github.com/user/repo", "branch": "main", "auth_token": TOKEN},
                "last_workflow_run": null,
                "pending": null,
                "run_results": [{
                    "config": {"name": "project", "auth_token": TOKEN},
                    "success": true,
                    "workflow_run": {
                        "id": 1, "display_title": "", "run_number": 1, "head_sha": "", "html_url": "",
                        "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
                    },
                    "steps": [],
                }],
            }),
        )
        .unwrap();
        let project_config: config::ProjectConfig = serde_yaml::from_str(&format!(
            "{{name: project, repo: github.com/user/repo, branch: main, auth_token: {TOKEN}}}"
        ))
        .unwrap();
        assert_eq!(project_config.auth_token.expose(), TOKEN);

        let github_client = github::Client::new(&db);
        let notifier = email::NoOpNotifier {};
        let project_manager = project::Manager::new(
            &db,
            &notifier,
            &github_client,
            "example.com".to_string(),
            vec![project_config],
            chrono::Duration::seconds(60),
        );
        let service = Service::new("example.com".to_string(), &github_client, &project_manager);

        let status_json = service.status_json();
        assert!(!status_json.contains(TOKEN), "{status_json}");
        assert!(status_json.contains(config::Secret::REDACTED));
        assert!(status_json.contains("4999"));
        let debug = format!("{:?}", project_manager.projects());
        assert!(!debug.contains(TOKEN), "{debug}");
        let stored: serde_json::Value =
            database::get_typed(&db, "project_manager/projects/project")
                .unwrap()
                .unwrap();
        assert!(!stored.to_string().contains(TOKEN), "{stored}");
    }
}
