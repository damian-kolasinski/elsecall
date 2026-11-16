# Onboarding v1: trust and local service boundary

This slice implements the same-user operation API and independently runnable
invitation/claim primitives. It does **not** install a plain Mac yet. Production
prepare always reports `blocked`, with no transferable artifact. Nothing starts a
LAN listener, issues production invitations, changes Tailscale, enrols a machine,
or grants installation/control authority in this slice.

## Native UI contract

All routes use the existing authenticated same-user Unix socket, never the member
TCP listener. Responses use `{ "schema_version": 1, "data": ... }`; request errors
use the existing error envelope. `POST` bodies must be `{}` (limit 1 KiB).

| Route | Result |
| --- | --- |
| `POST /v1/onboarding/prepare` | New operation, even when installer unavailable |
| `GET /v1/onboarding/{id}` | Persisted operation, including after restart |
| `POST /v1/onboarding/{id}/cancel` | Idempotent terminal cancellation |

The exact operation DTO is:

```json
{
  "version": 1,
  "id": "0123456789abcdef0123456789abcdef",
  "expires_at": 1800000900,
  "stage": "blocked",
  "artifact": null,
  "error": {
    "code": "onboarding_installer_unavailable",
    "message": "This release has no verified plain-Mac installer.",
    "hint": "A signed and notarized installer with invitation support must be published before Add Mac can send it."
  }
}
```

IDs are 128 random bits encoded as 32 lowercase hex digits. Lifetime is 900
seconds, with Unix seconds for expiry. V1 currently emits only `blocked`,
`cancelled`, and `expired`. At `now >= expires_at`, a blocked operation becomes
expired. Terminal states cannot be revived, including by cancellation or a clock
rollback after expiry was observed. Cancel clears the blocker. Unknown valid IDs
return 404; invalid IDs/requests return 400. Oversized requests return 413 with
`onboarding_request_too_large` in the JSON error envelope. The CLI equivalents are `elsecall
onboarding prepare`, `status ID`, and `cancel ID`; blocked/expired exit 1 while
still printing the operation data.

The UI should map known safe codes to actionable text and treat unsupported
versions/stages as unavailable. There is no transfer-acknowledgment endpoint.
AirDrop completion is only a local transfer observation, never evidence that an
installer opened, a machine joined, or remote control works.

## Distribution handoff

The next artifact-producing slice must preserve this packaging contract:

* A single `Elsecall Onboarding.zip`, declared format `elsecall-onboarding-v1`.
  Its two top-level entries are `Elsecall Installer.app` and
  `invitation.elsecall-invitation`.
* The app is a generic, independently signed/notarized receiver. Its bytes and
  sealed resources remain unchanged after signing. The invitation is a sibling
  document outside its signature. No configuration is injected into a signed app.
* The service must acquire a published, pinned release, verify publisher identity,
  notarization and supported receiver protocol, then copy unchanged app bytes and
  create the sidecar under private operation storage. Only after verification may
  prepare return `awaiting_transfer` with
  `artifact: { "path": "<absolute local ZIP path>", "format": "elsecall-onboarding-v1" }`.
  There is no supported override accepting an arbitrary unsigned installer.
* The receiver reads the adjacent invitation as bounded untrusted input. It must
  show the intended fleet identity and the permissions needed before installation.
  The accepted AirDrop transfer is the operator's source of the initial pin.
  A self-signed invitation cannot establish a fleet's identity independently of
  that trusted delivery; discovery must never replace the accepted pin.

Current distribution builds only an arm64 macOS tar archive with Elsecall and its
native UI, authenticated through packslip for mise. It has neither a plain-Mac
receiver app nor an installer acquisition path. The current signing context has
zero Developer ID Application and zero Developer ID Installer identities. No
release URL or credential is assumed. This is why production prepare is blocked;
the current archive is not sent as an installer.

## Invitation and one-member claim

`Invitation` is strict JSON with `payload` and `signature`, both unpadded URL-safe
base64. The payload is bounded to 2048 encoded bytes and contains `version: 1`,
`operation`, `issuer`, `fleet_master`, `issued_at`, and `expires_at`. The master
signs domain-separated exact UTF-8 JSON bytes using the existing strict Ed25519
member identity. V1 requires issuer = pinned master; a regular member cannot
self-assert delegated authority. The service orchestration must obtain invitations
from the current master in a subsequent slice. Signatures cover every field; time
validation requires exactly 900 seconds, rejects not-yet-valid invitations and
expires at the boundary. Discovery contains only correlation IDs and reachability
hints; none of it authorizes a claim.

The candidate generates an identity and fresh random challenge. Its signed claim
binds the invitation digest, candidate key and live challenge. The issuer checks
the invitation, expected master, candidate possession and its own signing key,
then atomically consumes the invitation in SQLite before signing a receipt. The
receipt binds the same context and verifies only against the bootstrap's retained
request. Expired, wrong-fleet, tampered and replayed claims are refused. The
ledger persists only a digest and public candidate key in owner-only storage.

If a response is lost after consumption, the invitation stays spent across restart.
Retry uses a **fresh invitation**, never a second claim on the old one. The current
primitive intentionally does not return a cached receipt or recover an installer
session. It grants no shell, root command, install, fleet enrolment or remote
control capability. Production issuance, revocation on cancel/role changes, LAN
discovery and authenticated transport are still required before connecting these
primitives to a receiver. The operation API does not issue or cancel test-library
invitations.

No bearer credential is present in an invitation or receipt. Private signing keys
remain in the existing member-key store; no Tailscale auth key, secret, or invitation
payload enters status, diagnostics, or general persisted operation records.

## Remaining installer lifecycle

Later stages are `awaiting_transfer`, `waiting_for_installer`, `claimable`,
`claiming`, `connecting`, `joined`, and `provisioning_pending`. They are reserved
design vocabulary, not supported responses in this slice. They require versioned
receiver integration and tests before enabling artifact transfer. A bootstrap must
offer narrow privileged actions for verified package installation, never a remote
root shell. It must preserve a compatible existing Tailscale setup and refuse a
different tailnet rather than silently switch it. macOS network/system-extension,
administrator and Tailscale account approval prompts remain explicit user steps.

Reuse existing Elsecall per-user installation ownership/conflict checks and member
identities. Preserve recognized installations and never replace unrelated files.
Current desktop/background launchd sessions are user sessions, not system boot
daemons; an installer cannot promise availability after reboot before login without
an independently designed persistence mechanism. Completion must verify persistent
connectivity and authenticated remote Elsecall control, then close the temporary
listener. Only then hand off `provisioning_pending` to later tool provisioning.
Inventory/desired-tool state already exists; tools apply, Xcode and secret/keychain
migration are outside this slice. Never report a fully provisioned developer Mac.

## Run the isolated demo and verification

```sh
cargo run --locked --example onboarding_trust_demo
cargo test --locked -p elsecall --lib onboarding
cargo test --locked -p elsecall --test onboarding_api --test onboarding_trust --test onboarding_service
```

The demo runs real Ed25519 signing/verification and SQLite replay refusal in a
temporary directory and deletes it on exit. It prints safe stage/error observations,
never keys or network details. Tests also start a real Elsecall service and CLI in
a temporary HOME, with external machine observations isolated by the existing test
harness. They use a neutral loopback tailnet boundary, prove restart/cancellation, preserve
existing installation bytes, and refuse unsafe state.
They do not install on another Mac. Native Add Mac/AirDrop presentation and its
actual-window demo belong to the separate native UI slice. There is no installer
window to demonstrate in this slice. No authorized clean Mac target is configured;
real end-to-end onboarding remains unverified.
