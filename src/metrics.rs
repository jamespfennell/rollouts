//! Prometheus metrics for the agent.
//!
//! Metrics are registered in a process-wide registry and served at /metrics by the HTTP service.

use std::sync::LazyLock;

use prometheus::{
    Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry,
};

pub struct Metrics {
    registry: Registry,

    pub github_requests: IntCounterVec,
    pub github_rate_limit_remaining: IntGaugeVec,
    pub github_rate_limit_limit: IntGaugeVec,
    pub github_rate_limit_reset: IntGaugeVec,
    pub github_rate_limited: IntCounterVec,
    pub github_request_duration: Histogram,

    pub poll_loop_duration: Histogram,
    pub project_poll_errors: IntCounterVec,
    pub project_last_poll_success: GaugeVec,

    pub deployments: IntCounterVec,
    pub deployment_duration: HistogramVec,
    pub deployment_step_failures: IntCounterVec,
    pub deployment_lag: HistogramVec,
    pub project_last_deployment: GaugeVec,
    pub project_pending: IntGaugeVec,
    pub project_paused: IntGaugeVec,

    pub build_info: IntGaugeVec,
    pub projects_configured: IntGauge,
    pub notifications: IntCounterVec,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// Returns the process-wide metrics.
pub fn get() -> &'static Metrics {
    &METRICS
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();
        let m = Self {
            github_requests: IntCounterVec::new(
                Opts::new(
                    "rollouts_github_requests_total",
                    "GitHub API requests made, by result (ok, not_modified, error).",
                ),
                &["project", "result"],
            )
            .unwrap(),
            github_rate_limit_remaining: IntGaugeVec::new(
                Opts::new(
                    "rollouts_github_rate_limit_remaining",
                    "Remaining GitHub API quota as reported by the last response.",
                ),
                &["resource"],
            )
            .unwrap(),
            github_rate_limit_limit: IntGaugeVec::new(
                Opts::new(
                    "rollouts_github_rate_limit_limit",
                    "GitHub API quota limit as reported by the last response.",
                ),
                &["resource"],
            )
            .unwrap(),
            github_rate_limit_reset: IntGaugeVec::new(
                Opts::new(
                    "rollouts_github_rate_limit_reset_timestamp_seconds",
                    "Unix time at which the GitHub API quota resets.",
                ),
                &["resource"],
            )
            .unwrap(),
            github_rate_limited: IntCounterVec::new(
                Opts::new(
                    "rollouts_github_rate_limited_total",
                    "Polls skipped because the GitHub API quota was exhausted.",
                ),
                &["project"],
            )
            .unwrap(),
            github_request_duration: Histogram::with_opts(
                HistogramOpts::new(
                    "rollouts_github_request_duration_seconds",
                    "Latency of GitHub API requests.",
                )
                .buckets(vec![0.05, 0.1, 0.2, 0.3, 0.5, 0.75, 1.0, 2.0]),
            )
            .unwrap(),

            poll_loop_duration: Histogram::with_opts(
                HistogramOpts::new(
                    "rollouts_poll_loop_duration_seconds",
                    "Time taken to poll all projects once, including any deployments.",
                )
                .buckets(vec![
                    0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
                ]),
            )
            .unwrap(),
            project_poll_errors: IntCounterVec::new(
                Opts::new(
                    "rollouts_project_poll_errors_total",
                    "Errors encountered while polling a project.",
                ),
                &["project"],
            )
            .unwrap(),
            project_last_poll_success: GaugeVec::new(
                Opts::new(
                    "rollouts_project_last_poll_success_timestamp_seconds",
                    "Unix time of the last successful poll of a project.",
                ),
                &["project"],
            )
            .unwrap(),

            deployments: IntCounterVec::new(
                Opts::new(
                    "rollouts_deployments_total",
                    "Deployments run, by result (success, failure).",
                ),
                &["project", "result"],
            )
            .unwrap(),
            deployment_duration: HistogramVec::new(
                HistogramOpts::new(
                    "rollouts_deployment_duration_seconds",
                    "Time taken to run all of the steps of a deployment.",
                )
                .buckets(vec![
                    1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
                ]),
                &["project"],
            )
            .unwrap(),
            deployment_step_failures: IntCounterVec::new(
                Opts::new(
                    "rollouts_deployment_step_failures_total",
                    "Deployment steps that failed.",
                ),
                &["project", "step"],
            )
            .unwrap(),
            deployment_lag: HistogramVec::new(
                HistogramOpts::new(
                    "rollouts_deployment_lag_seconds",
                    "Time from the GitHub workflow run being created to the deployment finishing.",
                )
                .buckets(vec![
                    60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0, 7200.0, 21600.0, 86400.0,
                ]),
                &["project"],
            )
            .unwrap(),
            project_last_deployment: GaugeVec::new(
                Opts::new(
                    "rollouts_project_last_deployment_timestamp_seconds",
                    "Unix time at which the last deployment with the given result finished.",
                ),
                &["project", "result"],
            )
            .unwrap(),
            project_pending: IntGaugeVec::new(
                Opts::new(
                    "rollouts_project_pending",
                    "1 if a new workflow run is waiting to be deployed, 0 otherwise.",
                ),
                &["project"],
            )
            .unwrap(),
            project_paused: IntGaugeVec::new(
                Opts::new(
                    "rollouts_project_paused",
                    "1 if the project is paused in the config, 0 otherwise.",
                ),
                &["project"],
            )
            .unwrap(),

            build_info: IntGaugeVec::new(
                Opts::new(
                    "rollouts_build_info",
                    "Always 1; labels describe the running build.",
                ),
                &["version"],
            )
            .unwrap(),
            projects_configured: IntGauge::new(
                "rollouts_projects_configured",
                "Number of projects in the config.",
            )
            .unwrap(),
            notifications: IntCounterVec::new(
                Opts::new(
                    "rollouts_notifications_total",
                    "Email notifications sent, by result (success, failure).",
                ),
                &["result"],
            )
            .unwrap(),

            registry,
        };
        let collectors: Vec<Box<dyn prometheus::core::Collector>> = vec![
            Box::new(m.github_requests.clone()),
            Box::new(m.github_rate_limit_remaining.clone()),
            Box::new(m.github_rate_limit_limit.clone()),
            Box::new(m.github_rate_limit_reset.clone()),
            Box::new(m.github_rate_limited.clone()),
            Box::new(m.github_request_duration.clone()),
            Box::new(m.poll_loop_duration.clone()),
            Box::new(m.project_poll_errors.clone()),
            Box::new(m.project_last_poll_success.clone()),
            Box::new(m.deployments.clone()),
            Box::new(m.deployment_duration.clone()),
            Box::new(m.deployment_step_failures.clone()),
            Box::new(m.deployment_lag.clone()),
            Box::new(m.project_last_deployment.clone()),
            Box::new(m.project_pending.clone()),
            Box::new(m.project_paused.clone()),
            Box::new(m.build_info.clone()),
            Box::new(m.projects_configured.clone()),
            Box::new(m.notifications.clone()),
        ];
        for collector in collectors {
            m.registry.register(collector).unwrap();
        }
        m.build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);
        m
    }

    /// Renders all metrics in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut buffer = vec![];
        prometheus::TextEncoder::new()
            .encode(&self.registry.gather(), &mut buffer)
            .unwrap();
        String::from_utf8(buffer).unwrap()
    }
}

/// Sets a gauge to the given time as a Unix timestamp in seconds.
pub fn set_timestamp(gauge: &Gauge, time: chrono::DateTime<chrono::Utc>) {
    gauge.set(time.timestamp_millis() as f64 / 1000.0);
}
