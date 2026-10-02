# Rollouts agent

This is an agent that runs on my VMs and implements continuous deployment for my projects.

The agent watches a number of GitHub repositories.
Whenever there is a new successful GitHub actions run on mainline, 
    the agent executes a specified command on the VM. 
All of my GitHub actions build Docker images and push them to Docker hub;
    the command executed by the agent pulls the latest image and recreates the container.
But for simplicity the agent is designed to be "Docker agnostic"
    and can run any command when a new successful build on mainline completes.

The repositories to watch and the VM commands to execute are specified using a config file.
An example of this config file is `config.yml` and the full spec with documentation
    is at `src/config.rs`.

Two optional shorthands cover the common Docker case:
    `compose` adds `docker compose pull` and `docker compose up -d` steps,
    and `check` adds a curl step (with retries) for each URL.
Projects can also live in their own files, listed under `include` in the main config;
    an included project's working directory defaults to the directory containing its file.

To run the agent in the repository root, simply run `cargo run -- $PATH_TO_CONFIG_FILE`.

To persist the state of the agent across runs,
    including the history of all previous rollouts,
    pass the `--db=some/file.txt` flag.
State will be saved in that flag.


## Deploying the agent

As with all my projects, the agent is deployed using Docker.
This introduces some challenges as the vanilla Dockerized deployment of the agent is sandboxed by default.
The agent generally needs access to many system resources in order to redeploy the configured projects
E.g., it needs to use system commands like `docker-compose`
    and it needs read various `compose.yaml` files on the filesystem.
The solution is to use appropriate Docker file system mounts.

The agent can be used to update itself.
The tricks is to run a _pair_ of agents, which update each other.
One of the pair is configured to redeploy after a delay
    (using the `wait_minutes` config field)
    to avoid concurrent rollouts.

## License

MIT
