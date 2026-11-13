# Elsecall

Elsecall makes a personal fleet of development machines ready for work, observable,
and controllable. Agents are the primary users; humans use it to understand and
direct the fleet. Prioritize usability, security, performance, and delight.

## Product surfaces

- **CLI:** the primary agent interface, with structured output and noninteractive commands.
- **Local web UI:** fleet overview, machine details, jobs, and maintenance controls.
- **Background service on each machine:** observe state, execute durable jobs,
  synchronize configuration, and manage local resources.
- **Shared API:** serve the CLI and web UI, with a future native iOS client in mind.

Machines communicate over Tailscale. An authorized machine can control the others;
accepted durable jobs continue if the initiating machine disconnects. Attached runs
(see [Fleet scheduling](docs/fleet-scheduling.md)) stop with their caller. No separate hosted
backend is required initially.

## Tech stack

- **Rust** for the CLI and background service; **Clap** for commands and
  **Tokio + Axum** for asynchronous execution and APIs.
- **Solid + TypeScript + Vite** for the web UI.
- **SQLite** for local operational state and job history.
- **Tailscale** for fleet connectivity.

## High-level scope

- **Machine preparation:** bootstrap tools, versions, repositories, and development configuration.
- **Environment and secrets:** keep configuration and credentials synchronized.
  Storage, distribution, and refresh behavior for running sessions remain undecided.
- **Git synchronization:** keep registered repositories' default branches current
  using safe fast-forward updates, preserving local changes and active worktrees.
  Report divergence rather than overwrite it.
- **Remote execution:** dispatch commands, follow progress, cancel jobs, reconnect,
  and retrieve results and artifacts.
- **Fleet visibility:** expose connection and sync status, CPU and memory usage,
  thermal state, capabilities, resource availability, and running work.
- **Housekeeping:** safely reclaim stale worktrees, processes, and owned resources.
- **State-based execution:** check readiness and select suitable machines for work.
  [Fleet scheduling](docs/fleet-scheduling.md) defines how work is admitted, placed and run.

Start with macOS and the existing fleet, keeping platform boundaries ready for
Linux. Moving unfinished work between machines and a native iOS app are later features.
