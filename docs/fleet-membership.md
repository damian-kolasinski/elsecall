# Fleet membership (S2)

Step S2 of [fleet-scheduling.md](fleet-scheduling.md): a tailnet listener, enrolment
and identity, the master role and moving it, fleet-wide machine state, a service that
runs without a desktop login, and installation on the second member.

## Decisions subsequent slices can build on

- **Identity is an Ed25519 key per member.** The service creates it on first start
  in `member.key` (owner-only, opened without following links, checked for owner and
  mode on every load). Only the service reads it. The CLI asks the service, over the
  same-user local socket, to perform fleet operations, so no CLI process holds the key.
  Key material never appears in `Debug`, logs or JSON; seeds are zeroized.
- **Every member request is signed; every response is signed.** A request signs the
  sender key, recipient key, timestamp, random nonce, method, path and body. A response
  signs responder, requester, the request's nonce, status and body. Transcripts are
  domain-separated and length-prefixed. Verification uses strict Ed25519 and refuses
  weak keys. Binding the recipient stops relaying a request to another member; binding
  the nonce stops replaying a response onto another request. Requests outside a
  60-second window, or with a nonce seen inside it, are refused (`peer_auth`).
- **Tailscale gives addresses, a node name and encryption, never identity.** Discovery
  is a port (`tailnet::Tailnet`). The production adapter runs the Tailscale CLI at a
  fixed path with `TAILSCALE_BE_CLI=1`, which the app bundle needs outside a terminal.
  It keeps only addresses inside the tailnet ranges. The listener binds each tailnet
  address on port 47821, never a wildcard, and retries discovery every 30 seconds.
  `ELSECALL_TAILNET_NAME`/`ELSECALL_TAILNET_ADDRESSES` replace discovery. Only then may
  loopback stand in for the tailnet, in listening and in rosters.
- **Unauthenticated requests get an empty 401 on every route.** The listener refuses a
  request before reading its body unless its headers name an enrolled key. Bodies are
  capped at 64 KiB and must arrive within 5 seconds. Headers get 5 seconds,
  connections 30 seconds, and the listener holds at most 64 connections.
- **The roster is a validated value.** A `Roster` lists unique names and keys, one to
  eight member endpoints each, a master that is a member, and a bounded epoch. It is
  only constructed through validation, including when it arrives from a member or from
  `fleet.json`. A member adopts an offered roster only if it is newer, lists both sender
  and receiver, and does not jump implausibly far ahead.
- **Changes are operator commands with a step on each machine.** Enrolling is
  `enroll <code>` on a member, then `join <code>` on the new one. Join pulls the
  roster from the sponsor and verifies it by the key in the code. Removal is `remove`
  on a member, then `leave` on the removed one. Member codes carry only public keys and
  endpoints. `enroll`, `remove` and `move-master` refuse while any member is
  unreachable unless forced. `sync` repairs members that missed a forced change, or
  that changed the fleet independently, by re-issuing the syncing member's roster above
  every epoch seen.
- **Placement authority is a type.** `PlacementAuthority` can only be obtained from
  `Fleet::authorize_placement`, which checks the sender against the recorded master's
  key. Its axum extractor answers everyone else with a signed `not_master`. S3 and S4
  placement handlers take it as an argument, as the `placement/probe` route does now.
- **Any enrolled member may change the roster and move the master role.** The contract
  makes moving the role an operator command that must work while the master is
  unreachable, so the master cannot be the only member allowed to change rosters.
  Membership is therefore trust between the operator's machines. Only placement
  authority is restricted to the master.
- **`elsecall fleet` asks every member, itself included, over the tailnet.** It shows
  each member's role and reachability, the member's own status, and whether that member
  records exactly the same roster (`in_sync`). Machine state is relayed as each member
  reports it, a JSON object, so fields S1 adds (utilization, pressure, thermal state,
  core counts) pass through without this slice depending on them.
- **Two LaunchAgent sessions.** `desktop` (`gui/<uid>`) is the existing installation and
  is updated in place. `background` (`user/<uid>`, `LimitLoadToSessionType`
  `Background`) runs without a desktop login, like the devbox's other agents. Both keep
  the service alive and restart it after a crash. New installations choose by whether
  a desktop session exists; updates keep their session. The same ownership, conflict
  and repair rules apply to both.
- **Broken fleet state degrades rather than stops the service.** If the key or roster
  cannot be opened, the local API still serves `status` and `machine`. The member
  listener stays closed, and fleet commands return the error with its repair.
- **The firewall is reported, not changed.** The macOS Application Firewall drops
  inbound tailnet connections to an executable it has no rule for, and adding one needs
  root. `install` and `doctor` report `member_listener_firewall` with the exact commands.

## Automated evidence

`mise run check` (formatting, Clippy with warnings denied, the locked test suite)
covers, with real service processes in separate temporary homes and loopback standing
in for the tailnet:

- Enrolment and joining with a step on each machine, including a member enrolled from a
  non-master member, idempotent re-enrolment, name and key conflicts, restart
  persistence, removal and leaving (`tests/fleet_membership.rs`).
- Rejection of unenrolled keys, fresh keys, unsigned requests, unknown routes, bodies
  without a proof, forged sender keys, replayed requests, requests signed for another
  member, removed members' keys, and impostor responses not signed by the enrolled key.
- Refusal of changes while a member is down; forced changes; incomplete changes
  exiting 1; `fleet` showing a down member as `member_unavailable`; sync completing a
  forced removal so the removed member loses trust everywhere; sync resolving a
  same-epoch conflict; degraded start after the key changed under an enrolled roster.
- Master-only placement authority, a non-member's `member_rejected`, the master refusing
  to leave, moving the role, refusal with an unreachable member, forcing (the
  unreachable member keeps trusting the old master), repair, and moving the role from a
  non-master member (`tests/fleet_master.rs`).
- Session selection, in-place desktop update, the switch to background, refusal of a
  desktop session without a desktop login before anything stops, and uninstall refusing
  to discard an enrolled key (`tests/installation.rs`).
- Unit tests for signing transcripts, replay windows, key files, roster invariants and
  offers, epoch bounds, member codes, tailnet ranges, Tailscale status parsing,
  LaunchAgent configuration and firewall rule parsing.

## Installed-system evidence

Verified on 2026-11-02 with release builds that contain main's S1 work (admission,
leases, records, `run`). The two members:

- `mac-mini`: Mac mini M4 Pro, macOS 26.4, account `me`, with a desktop
  login.
- `macbook-pro`: MacBook Pro M1 Max, macOS 26.6.2, account `devbox` (uid 501). The
  account has no desktop login: `launchctl print gui/501` exits 125, and its sessions
  are `Background`.

Neither machine has a Rust toolchain installed for this. The devbox got a copy of the
release binary over ssh, which ran its own `install`.

- **Update in place on the Mac.** `install` updated the existing desktop installation:
  `session: desktop`, the same LaunchAgent link, a new PID. S1's `runs` kept working on
  both machines.
- **Install on the devbox.** `install` chose `session: background` by itself.
  `launchctl print user/501/dev.elsecall.agent` showed it running in the `user/501`
  domain. The account's other LaunchAgents kept their PIDs throughout, including
  `com.devbox.codex-app-server` (PID 64606 before and after).
- **Firewall.** Both machines run the Application Firewall. The first installs
  reported `member_listener_firewall: blocked` with the commands, and doctor was
  unhealthy once the machines were enrolled. The operator ran the commands on each
  machine; both then reported `allowed`. Unauthenticated requests to either tailnet
  address got an empty 401. The rule kept working after an update that replaced the
  executable with a different build at the same path.
- **Listener.** On the Mac, `lsof` showed the service listening only on
  `100.64.0.10:47821` and `[fd7a:115c:a1e0::a]:47821`, never on a wildcard
  or LAN address. `lsof` on the devbox showed the same for `100.64.0.20` and
  `fd7a:115c:a1e0::b`.
- **Enrolment.** The Mac enrolled the devbox's code; `fleet join` on the devbox
  completed it. From then on, `elsecall fleet` on each side showed both members
  reachable, in sync at the same epoch, with `mac-mini` as master. Each
  member's machine state came from that member, including S1's CPU utilization, memory
  pressure, thermal state and core counts by type (14 = 10P+4E and 10 = 8P+2E).
- **Placement authority.** With the Mac as master, `check-authority macbook-pro` from
  the Mac was accepted, and the same request from the devbox got `not_master`.
- **Moving the master role.** `move-master macbook-pro`, run on the Mac, completed on
  both members (epoch 2). Both views showed the devbox as master, and authority
  reversed. `move-master mac-mini`, run on the devbox, moved it back
  (epoch 3). The pair was repeated after the update (epochs 4 and 5).
- **Running without a desktop login.** On the devbox the service kept the same PID
  across many separate ssh sessions over more than 40 minutes. After `kill -9` of the
  service, launchd restarted it within the same second (`runs = 2`), and `fleet` from
  the Mac reached it again at once. This was checked twice, on two builds.
- **A bug found and fixed.** The Mac had enrolled the devbox while the firewall still
  blocked the join, so enrolling again was refused as `member_unavailable` until the
  devbox joined. The fix lets enrolment be repeated before the member joins (item
  `Fleet: repeat enrolment before the member joins`).

## Deliberate limits

- Membership is operator-level trust: any enrolled member's key can change the roster
  or the master. A compromised member can therefore disrupt the fleet. It still cannot
  impersonate another member, and it holds placement authority only if it is master.
- Clocks must agree within 60 seconds. Only one service per machine and account
  listens on port 47821.
- Endpoints are recorded at enrolment. If a member's tailnet addresses change, enrol it
  again. Newly appearing addresses are bound, but disappeared ones are not released
  until restart.
- There is no per-address connection limit. A tailnet device can hold the 64 connection
  slots for up to 10 seconds per connection (5 for headers, 5 for a body after headers
  that name an enrolled public key, which member codes reveal) before it is refused.
- The macOS firewall needs an administrator to allow Elsecall's executable once per
  machine. The rule survived an update in testing, but nothing guarantees that for
  unsigned executables, so check `doctor` after updates.
- The background session's behaviour after a reboot with nobody logged in has not
  been tested; rebooting the devbox would disturb its agent workloads. Background agents
  load when the account's background session starts, as the devbox's existing agents
  do.
