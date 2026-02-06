use crate::config;
use crate::database;
use crate::email;
use crate::github;
use std::process::Command;
use std::sync;
use std::sync::mpsc;
use std::thread;

pub struct Manager<'a> {
    agent_url: String,
    projects: sync::Mutex<Vec<Project>>,
    db: &'a dyn database::DB,
    notifier: &'a dyn email::Notifier,
    github_client: &'a github::Client<'a>,
    poll_interval: chrono::Duration,
}

impl<'a> Manager<'a> {
    pub fn new(
        db: &'a dyn database::DB,
        notifier: &'a dyn email::Notifier,
        github_client: &'a github::Client,
        agent_url: String,
        projects: Vec<config::ProjectConfig>,
        poll_interval: chrono::Duration,
    ) -> Self {
        let mut projects: Vec<Project> = projects
            .into_iter()
            .map(|project_config| {
                match database::get_typed::<Project>(
                    db,
                    &format!["project_manager/projects/{}", project_config.name],
                )
                .unwrap()
                {
                    None => Project::new(project_config),
                    Some(mut project) => {
                        project.config = project_config;
                        project
                    }
                }
            })
            .collect();
        projects.sort_by_key(|p| p.config.name.clone());
        let projects = sync::Mutex::new(projects);
        Self {
            agent_url,
            db,
            github_client,
            notifier,
            projects,
            poll_interval,
        }
    }
    pub fn start<'scope>(&'a self, scope: &'scope thread::Scope<'scope, 'a>) -> Stopper {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            self.run(rx);
        });
        Stopper { tx }
    }
    pub fn projects(&self) -> Vec<Project> {
        (*self.projects.lock().unwrap()).clone()
    }
}

pub struct Stopper {
    tx: mpsc::Sender<()>,
}

impl Stopper {
    pub fn stop(self) {
        eprintln!("[project_manager] shutdown signal received");
        self.tx.send(()).unwrap();
        eprintln!("[project_manager] signalled to work thread; waiting to stop");
    }
}

impl<'a> Manager<'a> {
    fn run(&self, rx: mpsc::Receiver<()>) {
        eprintln!("[project_manager] work thread started");
        loop {
            let start = chrono::Utc::now();

            let mut projects = (*self.projects.lock().unwrap()).clone();
            for project in &mut projects {
                if rx.try_recv().is_ok() {
                    eprintln!(
                        "[project_manager] running project {} interrupted because of shut down signal",
                        project.config.name,
                    );
                    return;
                }
                if let Err(err) = project.run(self.github_client, self.notifier, &self.agent_url) {
                    eprintln!(
                        "Failed to run one iteration for project {}: {err}",
                        project.config.name
                    );
                }
                database::set_typed(
                    self.db,
                    format!("project_manager/projects/{}", project.config.name),
                    project,
                )
                .unwrap();
            }
            *self.projects.lock().unwrap() = projects;
            let loop_duration = chrono::Utc::now() - start;
            match self.poll_interval.checked_sub(&loop_duration) {
                Some(remaining) => {
                    if rx.recv_timeout(remaining.to_std().unwrap_or_default()).is_ok() {
                        eprintln!(
                            "[project_manager] sleep interrupted because of shut down signal"
                        );
                        return;
                    }
                }
                None => {
                    eprintln!("[project_manager] time to poll all projects ({loop_duration:?}) was longer than the poll interval ({:?}). Will poll again immediately", self.poll_interval);
                }
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Project {
    config: config::ProjectConfig,
    last_workflow_run: Option<github::WorkflowRun>,
    pending: Option<Pending>,
    run_results: Vec<RunResult>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Pending {
    workflow_run: github::WorkflowRun,
    first_seen: chrono::DateTime<chrono::Utc>,
    run_time: chrono::DateTime<chrono::Utc>,
}

impl Project {
    pub fn new(config: config::ProjectConfig) -> Self {
        Self {
            config,
            last_workflow_run: None,
            pending: None,
            run_results: Default::default(),
        }
    }

    pub fn run(
        &mut self,
        github_client: &github::Client,
        notifier: &dyn email::Notifier,
        agent_url: &str,
    ) -> Result<(), String> {
        let now = chrono::offset::Utc::now();
        if self.config.paused {
            self.pending = None;
            return Ok(());
        }
        let old_workflow_run = &self.last_workflow_run;
        let new_workflow_run_or = github_client.get_latest_successful_workflow_run(
            &self.config.repo,
            &self.config.branch,
            &self.config.auth_token,
        )?;
        let Some(new_workflow_run) = new_workflow_run_or else {
            return Ok(());
        };
        // If there is no new workflow run, exit early.
        if let Some(old_workflow_run) = old_workflow_run {
            if old_workflow_run.id == new_workflow_run.id {
                self.pending = None;
                return Ok(());
            }
        }

        // There is a new workflow run.
        let first_seen = match self.pending.as_ref() {
            None => None,
            Some(existing) => {
                if existing.workflow_run.id != new_workflow_run.id {
                    eprintln!(
                        "[{}] Abandoning workflow run {:#?} in favor of new workflow run.",
                        existing.workflow_run.id, self.config.name,
                    );
                    None
                } else {
                    Some(existing.first_seen.clone())
                }
            }
        };
        let run_time =
            first_seen.unwrap_or(now) + chrono::Duration::minutes(self.config.wait_minutes);
        // If it's not time to run the workflow yet, log and continue.
        if run_time > now {
            if first_seen.is_none() {
                eprintln!(
                "[{}] New successful workflow run found: {new_workflow_run:#?}; waiting until {} to deploy.",
                self.config.name,
                run_time,
            );
            }
            self.pending = Some(Pending {
                workflow_run: new_workflow_run,
                first_seen: first_seen.unwrap_or(now),
                run_time,
            });
            return Ok(());
        }
        eprintln!(
            "[{}] New successful workflow run found: {new_workflow_run:#?}; redeploying",
            self.config.name
        );
        self.last_workflow_run = Some(new_workflow_run.clone());
        self.pending = None;

        let json_config = serde_json::to_value(&self.config).unwrap();
        let mut result = RunResult {
            config: json_config,
            started: now,
            finished: now,
            success: true,
            workflow_run: new_workflow_run,
            steps: vec![],
        };
        for step in &self.config.steps {
            let pieces = match shlex::split(&step.run) {
                None => return Err(format!("invalid run command {}", step.run)),
                Some(pieces) => pieces,
            };
            let program = match pieces.first() {
                None => return Err("empty run command".into()),
                Some(command) => command,
            };
            eprintln!("Running program {program} with args {:?}", &pieces[1..]);
            let mut command = Command::new(program);
            command.args(&pieces[1..]);
            if let Some(working_directory) = &self.config.working_directory {
                command.current_dir(working_directory);
            }
            let step_result = match command.output() {
                Ok(output) => StepResult::new(step, &output),
                Err(err) => {
                    let json_config = serde_json::to_value(step).unwrap();
                    StepResult{
                        config: json_config,
                        success:false,
                        stderr: format!("Failed to start command.\nThis is probably an error in the project configuration.\nError: {err}"),
                    }
                }
            };
            let success = step_result.success;
            result.steps.push(step_result);
            if !success {
                result.success = false;
                eprintln!("failed to run command: {:?}", result);
                break;
            }
        }
        if !result.success {
            notifier.notify(
                &format!["Failed rollout for {}", self.config.name],
                &format!["Debug this failure: https://{agent_url}",],
            );
        }

        result.finished = chrono::offset::Utc::now();
        self.run_results.insert(0, result);
        while self.run_results.len() >= self.config.retention {
            self.run_results.pop();
        }
        Ok(())
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RunResult {
    // The project config at the time of the workflow run.
    // We store this as a JSON value so that the config file format
    // can be changed without worrying about serializing this field.
    config: serde_json::Value,
    #[serde(default)]
    started: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    finished: chrono::DateTime<chrono::Utc>,
    success: bool,
    workflow_run: github::WorkflowRun,
    steps: Vec<StepResult>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StepResult {
    // The step config at the time of the step execution.
    // We store this as a JSON value so that the config file format
    // can be changed without worrying about serializing this field.
    config: serde_json::Value,
    success: bool,
    stderr: String,
}

impl StepResult {
    fn new(step: &config::Step, output: &std::process::Output) -> Self {
        let json_config = serde_json::to_value(step).unwrap();
        Self {
            config: json_config,
            success: output.status.success(),
            stderr: vec_to_string(&output.stderr),
        }
    }
}

fn vec_to_string(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.into(),
        Err(_) => v
            .iter()
            .map(|b| if b.is_ascii() { *b as char } else { '#' })
            .collect(),
    }
}
