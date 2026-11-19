# Elsecall

Elsecall helps maintain and work across a fleet of machines: sync tool versions, environment variables, and secrets; dispatch commands; and expose machine state, including sync status and CPU and memory usage.

Agents using the CLI are the primary consumers. Humans using the web interface are secondary.

## Architecture

- Use hexagonal architecture.
- A capability is a good candidate for a port when at least one of the following applies:
  - At least two adapters are known from the start.
  - The operator's request indicates that another adapter may be needed in the future.
  - The capability is inherently coupled to a provider strongly enough to justify a port upfront.
- Define ports from the perspective of the system and its business logic. Never adjust business logic to fit an adapter.
- Validate untrusted data at system boundaries and trust established internal invariants. Model and handle expected domain and I/O failures without adding defensive checks for hypothetical internal states.
- Keep secret values out of logs, diagnostics, and persisted artifacts, except in storage explicitly intended for secrets. Enforce authorization at command-dispatch and machine-access boundaries.

## Modularization

- Design for modularity: minimal interfaces, rich implementations, tightly scoped responsibilities, and minimal coupling between modules.
- Small size alone is never a reason to avoid extracting a distinct responsibility. Extract modules when they isolate a responsibility or reduce coupling.
- Each module should own one tightly scoped responsibility. For example, a transport module should contain only transport details; caching, authentication, and other responsibilities belong in separate modules.
- Catch-all modules such as `core`, `utils`, `helpers`, and `design-system` are forbidden. Organize modules by responsibility.
- Give each module a clear, tightly scoped interface. Modules may depend only on other modules' interfaces, never on their implementation details.
- Model the domain through types and data structures instead of scattered conditions. Make impossible states unrepresentable to reduce branching.
- `scripts/module_shape.py` holds the shape mechanically, in `mise run check` and on every pull request:
  - Rust files stay under 800 lines not counting `#[cfg(test)]` code, and 1,500 is a hard cap.
  - A folder holds at most 60 source files directly.
  - Catch-all module names fail.
  - Older violations are in `scripts/module-shape-exceptions.txt` at the size they stood at, and ceilings only go down. Never add or raise an entry: split the module instead.
  - After splitting or moving a listed file, run `scripts/module_shape.py --shrink` to lower or move its entry.
  - A file moved with `git mv` keeps its ceiling, so a move need not also be a split. Keep moves and splits in separate changes so each reviews on its own.
  - Warnings show only for files the change touched; `--all` lists the whole backlog.
- The code is a workspace of crates: each is the package `elsecall-<name>` in `crates/elsecall-<name>/`, and the `elsecall` package holds only the binary, the CLI's commands and the service's composition. `scripts/crate_graph.py` holds the graph, in `mise run check` and `check:affected`:
  - `scripts/crate-graph.toml` puts every crate in a band: foundation, core, feature, lab, adapter, entry. A new crate gets its entry in the same change. Modules are the unit of responsibility, crates the unit of compilation: an edit reruns rustc for its crate and every crate above it, so crates stay coarse. A new responsibility is a module in the crate it belongs to; a new crate is for one of several thousand lines that few edits share with the rest.
  - Dependencies point down the bands, never up, and crates in one band form no cycle. Test code follows the same rules: a crate shares fixtures as an ordinary `pub mod fixtures`, built in every build. Never gate them behind a feature that tests turn on: `cargo build` and `cargo test` would then each build that crate and every crate above it. Foundation crates stay under their line budget and free of frameworks such as axum and rusqlite; core stays free of axum and clap.
  - Fix a violation by moving the shared item down to a crate both sides can use, or by declaring a port (a trait) in the lower crate and implementing it above. `--why FROM TO` lists every reference behind an edge; `--cycles` suggests which edges to cut.
  - Manifests inherit the workspace (`version`, `edition`, `rust-version`, `license`, `[lints]`), say `publish = false`, and name every dependency as `name.workspace = true`, declared once in `[workspace.dependencies]`.
  - `pub` means another crate uses the item (`unreachable_pub` is denied); inside a crate, write `pub(crate)`.
  - Code written against the single library crate: rebase it, and Git follows the moved files; `modules` in `scripts/crate-graph.toml` says where each library module went.
  - The files in `tests/` are modules of one test binary, `tests/main.rs`: a new test file needs its `mod` line there.

## Working

- Always prioritize usability, security, speed, and delight over code size or complexity. Code is cheap; user satisfaction is priceless.
- Before shipping work, spawn a code review subagent to review architectural and behavioral correctness, as well as security.
- When an operator's request spans multiple vertical slices, split the work into one deliverable per slice. Each slice must be independently verifiable and small enough for the operator to review comfortably.
- Verify each base slice before starting work on a slice that depends on it. Building on an unverified base makes problems harder to review and fix.
- Cargo runs through mbx, which shares compiled work between worktrees (README, "Build and install"). Keep its scheduler off: Elsecall admits builds.
- Always verify your work with the smallest check that would fail if the intended behavior broke:
  - Pure functions: unit tests.
  - Business logic: integration tests that prove behavior without stubbing modules or units internal to the codebase.
  - Business logic involving external adapters: integration tests with fakes at the system boundary.
  - Rendering logic: golden tests covering relevant rendering states and known boundary cases.
  - UI flows: end-to-end UI tests covering crucial paths.
  - Rendering optimizations: end-to-end performance tests with a baseline and evidence of the claimed improvement, stubbing only parts that do not affect the measured behavior.
- Install, update or uninstall Elsecall only in a macOS VM (`docs/e2e-vm.md`) or another test account, never in the operator's own account, whose installation other projects use; the VM is also the place for a fresh Mac and for demos of those flows. Verify everything else on the host: tests, the CLI and UI scenes are faster there, and the host runs at most two VMs, often both taken by other sessions.
- Test the update path (update check, install, service restart, window reopen, rollback) against local builds: `scripts/local-release.sh build` and `serve` on the host, then the printed install in a VM. Never publish a release just to test an update.

## Public material

This repository is public, and so are its commit messages and pull request descriptions. Evidence gathered on a real fleet must not carry private details into them.

- Write evidence, examples and test data with neutral names: members `mac-mini` and `macbook-pro`; accounts `me` and `devbox`, with homes under `/Users/me` and `/Users/devbox`; a consumer project `acme` (`acme-ios`), with labels such as `acme test Checkout`; the tailnet `example.ts.net`, with addresses in `100.64.0.0/24` and `fd7a:115c:a1e0::/112`; email addresses at `example.com`.
- Translate what a live fleet prints before writing it down. Never copy real host or member names, account names, home paths, tailnet names or addresses, employer or client names, other projects' module, label, script or branch names, or private repository URLs. Keep real timings and counts; they are the evidence.
- Private details must be stopped before they leave the machine: once pushed, a pull request keeps them even after a force push. Three guards run `scripts/privacy_scan.py`, which checks generic patterns plus private terms: the names that must never appear, listed one per line in `~/.config/elsecall/private-terms`, outside the repository, because the list itself names them.
  - `scripts/install-hooks.sh` installs pre-commit, commit-msg and pre-push hooks for the clone and all its worktrees. The pre-push hook scans ref names, tag messages and every commit not yet on any remote, however it was made. With private terms listed at install, the hooks refuse to run without them. The hooks run from a copy under `.git`, never from the checked-out branch; run the script again after `.githooks` or the scanner change.
  - Run `mg submit` and `mg reply` as `scripts/mg_guard.py submit …` and `scripts/mg_guard.py reply …`. Before a submit it scans every commit not yet on any remote, whether or not `mg` runs git hooks when it pushes, and it scans the title, guide and reply body, which become the pull request's description and comments and the commit message on `main`, and runs `mg` only when they are clean. A guide that names what a change removed leaks it again; describe it by kind ("the client's module names") instead.
  - CI is the backstop for what got past them, such as a clone without the hooks or a fork. It reads the terms from the `PRIVACY_SCAN_TERMS` secret.
- Fix a finding by translating it, never by widening the scanner's allowances. Assemble deliberately private-looking samples at run time, as `scripts/test_privacy_scan.py` does.

## Code review

When `mg` is installed and connected to the operator's hub, use the mergegate workflow below. Otherwise, use the normal GitHub pull request workflow.

- Publish work through the operator's mergegate hub with `mg submit`; never `git push` or `gh pr create`, even when asked to open a PR. The hub opens the GitHub PR and squash-merges it into `main` after approval.
- Follow the installed `mergegate` skill for submission, review feedback, and cleanup. If it is not loaded, read `~/.mergegate/skill/mergegate/SKILL.md` and its relevant references; do not vendor a copy into this repository.
- Run `mg init --project elsecall` once in every new clone or worktree, and `scripts/install-hooks.sh` once in every new clone. Fetch `mergegate` and start work on a branch from `mergegate/main`, since the clone's local `main` may be stale.
- Iterate with `mise run check:affected`, which runs only the tests the branch's changes can affect; `mise run check` must pass before submitting. It is the gate: pull requests get no full CI run, which runs only after merge and on release tags.
- An open issue labelled `main-red` means a workflow, named in its title, is failing on `main` (`gh issue list -l main-red`). Don't fix it alongside other work. Take it on only in its own change, after commenting on the issue that you are, and keep your own branch's failures apart from it.
- Commit scoped changes after relevant verification, then submit through `scripts/mg_guard.py` with a short review guide explaining the purpose, validation, and which files to read first.
- Answer each review comment with `mg reply` through `scripts/mg_guard.py`, make supported changes, and submit the next revision. In Codex, leave the wake command described by the mergegate skill and end the turn instead of polling for feedback.
- Reach the hub only through `mg`; its UI and HTTP API are reserved for the operator.
- Commits and PRs use only the operator's authorship: no AI co-author trailers or identities, and no mention of models, agents, or AI tools in commit messages or PR descriptions.
