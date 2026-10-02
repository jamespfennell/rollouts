//! Configuration for the agent.

use crate::{email, github};

/// Configuration for the agent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Hostname for the agent is running on; e.g. rollouts.example.com.
    ///
    /// This is used when sending emails and on the status page.
    pub hostname: String,

    /// List of projects to run the agent for.
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,

    /// Paths to files that each contain a single project config.
    ///
    /// Paths are relative to the directory containing this config file.
    /// Projects loaded from these files are added to `projects`.
    /// If such a project does not specify a working directory,
    ///     it defaults to the directory containing the project file.
    #[serde(default)]
    pub include: Vec<String>,

    pub email_config: Option<email::Config>,
}

/// A project to run the agent for.
///
/// Each project corresponds to a distinct deployment and generally a distinct GitHub repository.
/// Whenever there is a new successful CI run on the specified GitHub repository branch,
///     the agent will run the specified command.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectConfig {
    /// Name of the project. Used for debugging.
    pub name: String,

    /// If the project is paused; defaults to false.
    #[serde(default)]
    pub paused: bool,

    /// GitHub repository to watch.
    ///
    /// This field as the form `github.com/$USER/$NAME`.
    pub repo: github::Repo,

    /// Branch of repo to watch.
    pub branch: String,

    /// Auth token to use for making GitHub API requests.
    ///
    /// The auth token can be empty, in which case GitHub will use per-IP-address rate limiting.
    /// This allows up to 60 non-cached requests an hour.
    /// Using an auth token increases the rate limit to 5000 non-cached requests an hour.
    /// In general a non-cached request is only made when there is a new successful CI run.
    ///
    /// If provided, the auth token must have GitHub actions read permission
    ///     on the repository.
    #[serde(default)]
    pub auth_token: String,

    /// Working directory in which to run the redeployment steps.
    ///
    /// Defaults to the working directory in which the agent was started.
    pub working_directory: Option<String>,

    /// Shorthand for redeploying the project using Docker Compose.
    ///
    /// If set, the redeployment starts with the steps `docker compose pull`
    ///     and `docker compose up -d --remove-orphans <services>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<ComposeConfig>,

    /// Steps to perform during a redeployment.
    ///
    /// These run after the `compose` steps and before the `check` steps.
    #[serde(default)]
    pub steps: Vec<Step>,

    /// URLs to check after redeploying.
    ///
    /// Each URL is requested using curl, retrying for about 30 seconds,
    ///     and the redeployment fails if the URL does not return a successful response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check: Vec<String>,

    /// Number of prior deployments to retain in the internal database and show on
    /// the HTML status page.
    #[serde(default = "ten")]
    pub retention: usize,

    /// Minutes to wait after a successful CI run before performing the redeployment.
    ///
    /// This can be used to perform staggered redeployments.
    /// It can also be used to update the agent itself by having a second agent and performing
    /// staggered redeployments of the pair.
    #[serde(default)]
    pub wait_minutes: i64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Step {
    /// Name of the step.
    pub name: String,

    /// Command to run.
    pub run: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeConfig {
    /// Services to redeploy. If empty, all services are redeployed.
    #[serde(default)]
    pub services: Vec<String>,
}

fn ten() -> usize {
    10
}

/// Loads the config at the given path, including any projects in `include` files,
///     and expands the `compose` and `check` shorthands into steps.
pub fn load(path: &std::path::Path) -> Result<Config, String> {
    let mut config: Config = read_yaml(path)?;
    let config_dir = path.parent().unwrap_or(std::path::Path::new("."));
    for include in std::mem::take(&mut config.include) {
        let include_path = config_dir.join(&include);
        let mut project: ProjectConfig = read_yaml(&include_path)?;
        let project_dir = match std::fs::canonicalize(&include_path) {
            Ok(path) => path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
            Err(err) => {
                return Err(format!(
                    "failed to resolve path {}: {err}",
                    include_path.display()
                ))
            }
        };
        let working_directory = match &project.working_directory {
            None => project_dir,
            Some(working_directory) => project_dir.join(working_directory),
        };
        project.working_directory = Some(working_directory.display().to_string());
        config.projects.push(project);
    }
    let mut names = std::collections::HashSet::new();
    for project in &mut config.projects {
        if !names.insert(project.name.clone()) {
            return Err(format!("duplicate project name {:?}", project.name));
        }
        project.expand_shorthands();
    }
    Ok(config)
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            return Err(format!(
                "failed to read configuration file {}: {err}",
                path.display()
            ))
        }
    };
    match serde_yaml::from_str(&raw) {
        Ok(t) => Ok(t),
        Err(err) => Err(format!(
            "failed to parse YAML configuration file {}: {err}",
            path.display()
        )),
    }
}

impl ProjectConfig {
    fn expand_shorthands(&mut self) {
        let mut steps = vec![];
        if let Some(compose) = &self.compose {
            steps.push(Step {
                name: "Pull".into(),
                run: "docker compose pull".into(),
            });
            let mut run = "docker compose up -d --remove-orphans".to_string();
            for service in &compose.services {
                run.push(' ');
                run.push_str(&shlex::quote(service));
            }
            steps.push(Step {
                name: "Redeploy".into(),
                run,
            });
        }
        steps.append(&mut self.steps);
        for url in &self.check {
            steps.push(Step {
                name: format!("Check {url}"),
                run: format!(
                    "curl --fail-with-body -v --retry 10 --retry-delay 3 --retry-all-errors --output /dev/null {}",
                    shlex::quote(url)
                ),
            });
        }
        self.steps = steps;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &std::path::Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn include_and_shorthands() {
        let dir = test_dir("rollouts_config_include");
        write(
            &dir.join("agent/config.yml"),
            "
hostname: example.com
include:
- ../service/rollouts.yml
projects:
- name: inline
  repo: github.com/user/inline
  branch: main
  steps:
  - name: Hello
    run: echo hello
",
        );
        write(
            &dir.join("service/rollouts.yml"),
            "
name: service
repo: github.com/user/service
branch: main
compose:
  services: [web, docs]
steps:
- name: Middle
  run: echo middle
check:
- https://example.com/a
",
        );

        let config = load(&dir.join("agent/config.yml")).unwrap();

        assert_eq!(config.projects.len(), 2);
        let inline = &config.projects[0];
        assert_eq!(inline.working_directory, None);
        assert_eq!(inline.steps.len(), 1);
        let service = &config.projects[1];
        assert_eq!(
            service.working_directory,
            Some(dir.join("service").display().to_string())
        );
        let runs: Vec<&str> = service.steps.iter().map(|s| s.run.as_str()).collect();
        assert_eq!(
            runs,
            vec![
                "docker compose pull",
                "docker compose up -d --remove-orphans web docs",
                "echo middle",
                "curl --fail-with-body -v --retry 10 --retry-delay 3 --retry-all-errors --output /dev/null https://example.com/a",
            ]
        );
    }

    #[test]
    fn duplicate_names() {
        let dir = test_dir("rollouts_config_duplicate");
        write(
            &dir.join("config.yml"),
            "
hostname: example.com
include:
- service.yml
projects:
- name: service
  repo: github.com/user/service
  branch: main
",
        );
        write(
            &dir.join("service.yml"),
            "
name: service
repo: github.com/user/service
branch: main
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(err.contains("duplicate project name"), "{err}");
    }

    #[test]
    fn unknown_top_level_field() {
        let dir = test_dir("rollouts_config_unknown");
        write(
            &dir.join("config.yml"),
            "
hostname: example.com
inclde:
- service.yml
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(err.contains("inclde"), "{err}");
    }
}
