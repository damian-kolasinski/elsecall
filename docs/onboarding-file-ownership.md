# Onboarding file ownership and reversal

This independently verified slice supplies the macOS per-user filesystem part of
onboarding and teardown. It does not activate production prepare, install a
receiver, grant privileges, change Tailscale, or implement the native Detach
action. The complete onboarding milestone and its real-device acceptance run
remain unfinished. The ZIP/signature contract in [onboarding-v1](onboarding-v1.md)
is unchanged; the existing release updater is unchanged.

## The ownership boundary

`provisioning_files` provides `Creation`, `Session`, and `Finalization`. These are
local interfaces for the intended user's trusted coordinator. Every entry point
refuses root. Serialized tickets and receipts belong in private coordinator
storage; they must never become request bodies accepted from the LAN or a
general privileged filesystem interface.

The scope is an existing user-owned directory, with normalized relative paths
inside it. Ancestors are opened through directory descriptors without following
symlinks. New symlinks must reference a regular file through safe ancestors inside
the scope. Modes are explicit ordinary private/public file or executable modes.
Traversal, conflicting file types, multiply-linked regular files and files over
256 MiB are refused. Hash reads are bounded even if a file grows while being read.

Each file mutation chooses ownership deliberately:

| Choice | Forward action | Reverse action |
| --- | --- | --- |
| Introduced | Publish only into an absent destination | Capture and verify the introduced inode, then remove it |
| Reused | Leave the existing file or directory untouched | Leave it untouched, including subsequent user edits |
| Restorable | Atomically swap a regular file with the new file | Swap back the retained original inode before removing the new one |

The retained original preserves contents, owner/group, mode, modification time,
ACLs and extended attributes. There is no lossy reconstruction from a content
backup. Exact identities include device, inode, birth time, ownership, permissions,
flags, content or link target, and ACL/extended-attribute digests. A replaced inode,
edited file, or modified symlink is preserved and reported as pending. Directory
identity excludes child-list changes, but removal is always empty-only: user work
cannot be recursively deleted.

Restoration storage may contain secret original files. Its directory and parent
must be owner-only with no extended ACL grants; POSIX modes alone are insufficient
on macOS. New storage explicitly clears its ACL, and subsequent operations and
resume validate privacy. Receipt metadata is mode 0600. Status reports contain
only a clean flag, receipt-local entry indices and safe error codes/messages;
paths, contents and tickets never enter public status or logs.

## Durable handoff

The future lifecycle coordinator must follow this protocol:

1. Allocate a new receipt slot in its private namespace on the same volume as
   the target scope. `Creation::prepare` makes no filesystem changes. Durably save
   that creation ticket outside the receipt before calling `start`.
2. `start` can resume partial directory/lock/initial receipt creation. The ticket
   reserves only that exact private slot. Unknown contents prevent recovery;
   an existing complete receipt must carry the same creation identity.
3. Provision each directory, regular file or symlink through the session. The
   destination's prior identity and installed identity are committed before
   atomic publication. Incomplete writes remain in an explicitly reserved private
   scratch slot. The session lock excludes concurrent lifecycle writers.
4. `cleanup` durably disables further provisioning and reverses entries in order
   from last to first. Independent failures do not prevent other entries from
   being attempted. An owned destination is renamed into private quarantine before
   deletion, then verified again. A concurrent pathname replacement is preserved.
   Capture is recorded before private-slot deletion, so a process restart can
   distinguish an authorized deletion from an unexplained disappearance.
5. A nonempty quarantined directory is returned to its original free pathname,
   including after restart. If that path is occupied, preserve both objects and
   keep cleanup pending. Unknown receipt contents also prevent a clean result.
6. After tracked files are restored, obtain `Finalization` and durably retain it
   outside the receipt. Its pinned directory/metadata identities allow removal of
   the remaining receipt and lock, including retry after partial metadata removal.
   Persist the mutated ticket after **every** attempt. The ticket records removal
   only after successful `rmdir`; repeated attempts with committed removal evidence
   are idempotent. A missing receipt without that evidence remains unverified.

If a process stops after final `rmdir` but before its external completion evidence
is saved, automatic cleanup cannot infer what happened from absence alone. Retain
the coordinator's pending state and require independent baseline verification.
Similarly, externally moving a managed object never counts as its deletion. No
filename search or broad directory cleanup attempts to locate or erase user work.

`Cleanup.clean` refers to the tracked filesystem entries and private scratch, with
the receipt still retained for finalization. It is not evidence that services,
fleet membership, Tailscale credentials, helpers or a whole device have been
removed. The coordinator must aggregate those separate effects and retain
connectivity until the authenticated teardown handoff is durable. It must retain
pending/failed status for offline members or missing completion evidence.

## Verification and remaining work

Run the pure path/hash boundary tests and real-filesystem integration tests with:

```sh
cargo test --locked -p elsecall --lib provisioning_files
cargo test --locked -p elsecall --test provisioning_files
cargo clippy --locked -p elsecall --lib --test provisioning_files -- -D warnings
```

The integration suite installs and reverses real temporary files twice; restores
the original inode and metadata; preserves reused components, user edits and
unknown files; refuses unsafe paths/ACLs; exercises interrupted staging,
publication, quarantine, creation and finalization states; and races cleanup
against concurrent pathname swaps without deleting unrelated inodes. Restart
states are reconstructed at the external private-filesystem boundary. They are
not a native UI demo or a clean-Mac installer test. No test grants privileges or
changes a live fleet.

The next slices must connect this ownership handoff to verified receiver packaging,
production invitation issuance/revocation, authenticated LAN transport, service
ownership, and narrow privileged install/teardown actions. Dynamic service state,
transferred ZIP/extracted app ownership, temporary network listeners, managed
files, tailnet authorization and helper self-removal each need explicit receipt
effects and forward/reverse verification before production enablement.

Headless `tailscaled` remains a candidate requiring actual clean-Mac validation.
The official [macOS background](https://github.com/tailscale/tailscale/wiki/Tailscaled-on-macOS)
documents utun networking, launchd persistence, limited automatic DNS integration
and less testing. Reusing an existing installation must preserve its tailnet and
configuration; this filesystem module does not establish that compatibility.

The final acceptance evidence must come from the actual host window and an
operator-authorized test Mac: baseline, real AirDrop, measured system prompts,
authenticated join/control, UI detach, inspected restoration, then a second
onboard/detach cycle. That run has not occurred. Production receiver signing and
notarization remain external prerequisites; no unsigned production override is
provided.
