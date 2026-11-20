# Adding a Mac

How a Mac becomes a fleet member: the operator turns on Remote Login on it, then
the master does the rest over SSH. At the end the Mac is a member that the master
reaches and controls over the tailnet. It is **awaiting provisioning**, so placement
gives it no work. Installing its tools is
[fleet tool management](fleet-tools.md), a separate feature.

A change to anything here is a product decision and updates this document first.

## What the operator does

1. **On the new Mac, once:** System Settings → General → Sharing → Remote Login on,
   with "Allow access for" including the account used below (all users, the
   default, does). Join the master's Wi-Fi in System Settings › Wi-Fi, or connect
   both Macs to the same local network by Ethernet. The Add Mac sheet illustrates
   both Remote Login and Wi-Fi.
2. **On the master:** Add Mac in the fleet window, or `elsecall fleet nearby` in the
   CLI. The master lists the Macs it finds on the local network. A Mac it does not
   find can be added by host name or address.
3. **Pick the Mac and give an administrator account's name and password on it.**
4. **Approve the Mac in Tailscale** when the Mac is not on the tailnet yet: the master
   opens Tailscale's login link in its browser, which is already signed in to the
   tailnet.

The CLI form, for agents, is `elsecall fleet add <host> --user <account>` with the
password on standard input. It prints one JSON document like every other command.
While it waits for the Tailscale approval it prints the login link on standard
error and opens it in the browser; past its deadline it fails with
`tailscale_login_pending`, the link and the command that continues.

## What the master does

Every step checks before it changes anything, so running Add Mac again on a Mac
that stopped halfway continues where it stopped, and running it on a member repairs
it without changing its readiness.

1. **Finds Macs.** Bonjour browses `_ssh._tcp`, which a Mac with Remote Login on
   advertises, and `_companion-link._tcp`, which most Macs advertise while
   Continuity is on, and reads each host's model from `_device-info._tcp` to keep
   only Macs. A Mac seen only through `_companion-link._tcp` is listed with Remote
   Login off and what to turn on. Listing is best effort; a typed host name works
   without it. Discovery omits the current Mac. The Add Mac picker also hides
   virtual Macs and confirmed fleet members. Enter an address manually to add
   a virtual Mac or repair a fleet member.
   Discovery is a port; the production adapter uses the system's DNS-SD.
2. **Connects over SSH** to the Mac by its host name or address, with the account and
   password, and checks the Mac before changing anything. It refuses a Mac it cannot run the master's build
   on (the build is arm64 for macOS 14 or later), an account that is not an
   administrator, and a password sudo does not accept. It reports the Mac's model,
   serial number and SSH host key fingerprint, which the operator may compare with
   About This Mac. It also refuses a firewall that blocks all incoming connections
   and a Mac that belongs to another fleet. Refusals that need only the fleet (not
   the master, a member unreachable or on another build) come before connecting.
3. **Puts the Mac on the tailnet.**
   - A Mac already on the master's tailnet is used as it is.
   - A Mac on another tailnet is refused, never switched, also when Tailscale there
     is logged out but remembers that tailnet.
   - Add Mac's own daemon that does not answer is installed again; another
     `tailscaled` that does not answer, such as Homebrew's, is refused with how to
     start it.
   - A Mac with the Tailscale app installed but not logged in is refused with what
     to open: the app's login needs a person on that Mac. Tailscale's variants
     cannot run side by side, so the master never installs a second one.
   - A Mac without Tailscale gets Tailscale's open-source `tailscaled` as a root
     LaunchDaemon (`tailscaled install-system-daemon`, with both commands in
     `/usr/local/bin`, where Elsecall looks for them). The binaries come from
     Homebrew's bottle for the newest macOS the Mac runs: each bottle's `tailscaled`
     needs its own macOS or later, and macOS 14 has none, so such a Mac is refused
     with a pointer to Tailscale's own download. The Tailscale version and each bottle's SHA-256
     are pinned in the Elsecall build, so the download is checked against what the
     build was reviewed with. Homebrew itself is not installed. This variant needs no
     system extension or VPN approval on the Mac. Features only the Tailscale app
     has, such as Taildrop, are not available on it.
   - `tailscale up` prints a login link; a daemon that never logged in takes the
     Mac's Bonjour name, one that did keeps its settings. The master opens the link
     and waits for the approval, then checks that the Mac's tailnet is the
     master's. A tailnet that approves devices gets its admin console opened
     instead. Only links on `login.tailscale.com` are opened.
4. **Installs Elsecall** by copying the master's own executable, so the new member
   runs the master's exact build, and running its `install`. The session follows
   the existing rule: desktop when the account has a desktop login, otherwise
   background.
5. **Opens the Application Firewall** for Elsecall, and for Add Mac's `tailscaled` so
   peers connect directly rather than through Tailscale's relays, when it is on,
   with sudo. This changes the rule that Elsecall only reports firewall rules
   ([fleet membership](fleet-membership.md)): during Add Mac the operator has
   granted administrator access for exactly this.

   It also installs a root LaunchDaemon, `dev.elsecall.firewall-guard`, whether the
   firewall is on or off. The firewall can ask whoever sits at the Mac's console
   "Accept incoming network connections?" even for a program an administrator
   allowed, and a console user who leaves that unanswered and logs out denies it: the
   rule turns to Block, every member's connection is accepted and closed unanswered,
   and only an administrator can lift it. Every 30 seconds the guard allows again any
   of those programs whose rule blocks. It cannot see a prompt that is still waiting
   for an answer, while connections wait too, and it adds no rule the firewall does
   not have. Running Add Mac again on a member installs or updates the guard. To
   remove it on the Mac:
   `sudo launchctl bootout system/dev.elsecall.firewall-guard` and
   `sudo rm /Library/LaunchDaemons/dev.elsecall.firewall-guard.plist`.
6. **Authorizes the master's SSH key.** The operator's public key on the master
   (`~/.ssh/id_ed25519.pub`, or the first of the other default keys) is added to the
   account's `authorized_keys`, limited to connections from the tailnet ranges and
   marked as Elsecall's. The master can then reach the Mac over the tailnet when
   Elsecall itself is broken. `fleet leave` and `uninstall` on the Mac remove the
   marked lines. A master without a key is told how to create one; the step is
   skipped, not failed. Remote Login stays on, including password logins on the
   local network, as the operator turned it on.
7. **Enrolls the Mac** with the code its `fleet identity` prints, marked awaiting
   provisioning, then has it `fleet join`.
8. **Verifies** that the Mac answers over the tailnet as a member, in sync with the
   master's roster, and reports when its Tailscale node key expires.

## Decisions

- **Root runs only system programs it fetched itself.** Root steps run under
  `/usr/bin/sudo` with an empty environment and only system directories on `PATH`,
  and every account command gets the same `PATH`, since the account's shell setup
  could put its own `mkdir` or `shasum` first. Tailscale is downloaded, checked
  and installed by root in a directory root created, never from a path the
  account could change.
- **A root guard acts on the account's executable, as data only.** The firewall
  guard is the one root component that keeps running, and it acts on a path the
  account can change. It runs only `socketfilterfw` and `logger`, by absolute path
  and in launchd's empty environment, with the program paths as arguments, and never
  runs the programs. A firewall rule already follows the program's path from build to
  build, so allowing it again grants nothing the account could not get by replacing
  the file. It skips anything but a regular file reached without a symbolic link, and
  acts only on the path's own rule when that rule blocks; `socketfilterfw` changes no
  file, and a path without a rule reads as permitted, so it is left alone. The
  firewall records a rule with the code signature of the file at the path, so an
  account that puts another program's bytes there, and gets its rule to Block, has
  the guard allow that signature for the path; that program's own rules elsewhere
  stay as they were. Nothing runs as root but `socketfilterfw`. Verified in a macOS
  VM: a rule that a denied prompt turned to Block, and one set to Block by hand, were
  allowed again on the guard's next run, a linked path was left alone, and an
  unsigned file was unchanged by `--add`.
- **SSH from the master, not an installer on the Mac.** A plain Mac exposes nothing
  else the master can use, and an installer delivered to it would need a Developer ID
  signature, notarization and several approvals on the Mac. Turning on Remote Login
  is one switch and is part of macOS.
- **The add runs in the process the operator started.** macOS Local Network privacy
  denies a LaunchAgent, such as the Elsecall service, connections to local network
  addresses: a probe run under launchd got "No route to host" from a Mac that the
  same probe reached from a shell. A command run from Terminal or over SSH is
  allowed, and an app asks once. So discovery and the SSH session run in the CLI
  process (`elsecall fleet nearby`, `elsecall fleet add`), and the window runs that
  CLI as its child, so the permission is asked for as Elsecall's. The CLI uses the
  local service only for what it already does: the master's identity and enrolling.
- **The password lives only in the add command's memory.** It never appears in
  arguments, the environment, a file, a log, a diagnostic or the output. OpenSSH
  gets it from an askpass helper that reads it once from a socket in a private
  directory the command creates and removes; sudo gets it on standard input. SSH to
  the Mac ignores the operator's SSH configuration, agent and keys. Running Add Mac
  again asks for the password again. Later work, such as provisioning, asks for it
  again too, or uses the master's key and the narrowly scoped sudo rules that work
  installs.
- **The SSH host key is trusted on first use on the local network.** The operator
  picked the Mac by name from the local network, and its host key must not change
  for the rest of the command. A device on the same network that answers in its
  place learns the password, and the master would install Elsecall on it and enroll
  it. An enrolled member can change the roster and move the master role, so this is
  more than a leaked password. Comparing fingerprints by hand would break the
  one-switch flow, so the risk is accepted; the operator may compare the reported
  model, serial number and fingerprint. Holding back synced files from a new member
  would not contain it, since an enrolled member can mark itself ready.
- **Awaiting provisioning is roster state.** A member enrolled by Add Mac is listed
  in the roster's `awaiting_provisioning`, so every member and every future master
  agrees on it. Placement leaves it out and says why in the run's reason; work that
  names it with `--member` still runs there, which is how the master controls it.
  Configuration sync and upgrades include it as any member. It cannot become master.
  `elsecall fleet ready <name>`,
  and later provisioning, marks it ready with a roster change, which, like every
  roster change, is refused while a member is unreachable unless forced. A roster
  with no member awaiting provisioning serializes exactly as before. A member on a
  build that predates this drops the list when it stores the roster, and a roster
  change it makes marks every waiting member ready. So Add Mac refuses while a
  reachable member runs another build than the master, with `fleet upgrade` as the
  fix.
- **Only the master adds Macs.** This is a product choice, not a security boundary:
  any member can still enroll by hand. The master's build is the one copied, and the
  master is where the fleet is looked after.
- **A member in a desktop session needs a login after a reboot.** `tailscaled` runs
  at boot, but Elsecall's desktop session starts at the account's login, as for any
  member. Members without a desktop login run in the background session.
- **Tailscale node keys expire.** A node approved through a login link expires after
  the tailnet's key expiry, 180 days by default, and then drops off the tailnet.
  Disabling expiry needs the Tailscale admin console, so Add Mac reports the expiry
  and links to the console instead of changing it.

## Slices

1. This document; the earlier AirDrop installer and invitation work is removed.
2. Members awaiting provisioning take no placed work; `fleet ready` marks one
   ready.
3. `elsecall fleet nearby`: Macs on the local network.
4. `elsecall fleet add` over SSH for a Mac already on the master's tailnet.
5. Tailscale for a Mac that is not on the tailnet.
6. Add Mac in the fleet window.

End-to-end evidence uses a Tart VM ([e2e-vm.md](e2e-vm.md)) as the new Mac. Its
image already has Remote Login on and Homebrew installed; neither changes the flow,
which needs Remote Login and does not use Homebrew.
