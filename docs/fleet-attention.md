# Fleet attention

What in the fleet needs an agent or a person, as one typed list. `elsecall fleet`
and the local route it reads, `GET /v1/fleet`, carry it as `data.attention`. Agents
read it to find work Elsecall cannot finish by itself; the window will show it, and a
hand-off will start an agent session for an item. Fleet provisioning adds its own
categories to it.

## An item

```json
{
  "id": "awaiting_provisioning:macbook-pro",
  "member": "macbook-pro",
  "category": "awaiting_provisioning",
  "subject": null,
  "code": "member_awaiting_provisioning",
  "message": "macbook-pro is awaiting provisioning: placement gives it no work until it is marked ready. Work required on it by name still runs there.",
  "hint": "Install the tools the fleet's work pins on macbook-pro, then run elsecall fleet ready macbook-pro on mac-mini.",
  "since": null,
  "actions": [{
    "summary": "Mark macbook-pro ready once its tools are installed, so placement may give it work.",
    "command": ["elsecall", "fleet", "ready", "macbook-pro"],
    "run_on": "mac-mini",
    "person_only": false
  }]
}
```

- **`id`** is `<category>:<member>`, with `:<subject>` added when the item has a
  `subject`: what on the member it is about, for a category that can hold several
  items per member (such as one per missing tool). Read the subject from `subject`,
  never from the `id`. It is the same on
  every read while the condition lasts, and the same again if the condition returns,
  so a reader can tell an item it already handled from a new one.
- **`member`** is the member the condition is about, which is not always where its
  action runs.
- **`category`** is the kind of attention (below). **`code`** names the condition in
  the vocabulary of error codes, so a refusal and the item behind it share a code
  (`version_skew`, `member_awaiting_provisioning`).
- **`message`** and **`hint`** say what is wrong and what resolves it, as a failure
  line does.
- **`since`** is when the condition began when the member records it; otherwise null.
  Of today's conditions only a missing tool records its start: the end of the run
  that hit it.
- **`actions`**, the first one first, each carry a ready-to-run `elsecall` command as
  its arguments (`command`, run without a shell, so a subject needs no quoting), the
  member to run it on (`run_on`), and `person_only`: whether only a person can
  complete it, because it asks for a password, shows a prompt on a member's screen, or
  is a decision an agent must not make alone. An agent may still run such a command
  to start it, then hand over to the person.

## Categories

A category is what an operator trusts an agent with. The hand-off will let an
operator accept a category's future items automatically, so every item in a category
has the same kind of cause and is resolved by the same kind of action. A new kind of
condition gets its own category rather than widening one, and a reader shows a
category it does not know but never accepts it automatically (this build reads one as
`unknown`). Nor is a `person_only` action ever run automatically.

| Category | Code | When | Action |
|---|---|---|---|
| `awaiting_provisioning` | `member_awaiting_provisioning` | The member was enrolled before its tools were installed, as Add Mac does; placement leaves it out. | `elsecall fleet ready <member>` on the master, once its tools are installed. |
| `version_skew` | `version_skew` | The member runs another build than the master; work between them is refused. | `elsecall fleet upgrade --member <member>` on the master. When the member runs a later version than the master, this installs an older build on it, so the action is `person_only` and the hint offers updating the master first. |
| `missing_tool_pin` | `missing_tool_pin` | A run this member placed failed on `member` because a tool it called (the `subject`) is missing there, and nothing pins it. One item per member and tool, from the latest such run; `since` is when that run ended. | Pin the tool in the file the hint names: `elsecall tools pins <workspace>` on the member that placed the run shows the pin. Second, `person_only` and not recommended: install it on that member alone. |
| `secret_consent` | `secret_consent` | A change to files in a synced path (the `subject`) that look like secrets waits on the master, `member`, until a person confirms it. One item per path; the message names the files, never their content. | `elsecall sync consent <path> --confirm <id>` on the master syncs this change; the second action adds `--always`, so later changes to those files sync unasked. Both are `person_only`, and the CLI refuses `--confirm` without a terminal. |
| `provisioning_step_failed` | the failure's code, such as `tool_install_failed` | A step of `member`'s tool plan failed to install there. One item per member and step, its `subject` `<manager>:<tool>@<version>`; the message names the stage that failed, never what a command printed. `since` is when it failed. It stands until the member's next apply. | `elsecall tools apply --member <member>` on the master retries it. A step that needs Elsecall's Xcode privileges first adds `elsecall tools privileges install` on the member, `person_only`. [Fleet provisioning](fleet-provisioning.md), "Delivery 4", lists the codes. |
| `xcode_apple_id_required` | `xcode_not_on_master` | A member's plan wants an Xcode the master, `member`, does not have, so no member can copy it. One item per version (the `subject`); `since` is the earliest such failure. | A person downloads that Xcode onto the master with an Apple ID; then `elsecall tools apply` on the master, `person_only`. |


A `missing_tool_pin` item is derived from the runs this member placed, so it shows in
that member's fleet view, which is the master's for placed work. It clears when the
tool is pinned, when a later run of the same job on that member exits 0, or after 7
days. [Fleet provisioning](fleet-provisioning.md), "When a job needs something
unpinned", says how the tool is recognised without keeping any output.

Left out on purpose:

- **Unreachable members.** A sleeping laptop is unreachable for hours and needs
  nothing; `members[].error` already says why.
- **Outside pressure.** A member whose gate outside processes hold closed reopens it
  by itself, and the only repair is quitting the operator's own apps, which no agent
  should decide. `members[].blocked` and `outside_warnings` show it.
- **The firewall.** A member whose firewall drops other members' connections cannot
  tell the master; it is simply unreachable. Its own `elsecall doctor` reports the
  commands to allow it.

## Derived, not stored

Items are derived from the fleet view each time it is read, so one disappears the
moment its condition ends, and no stored list can drift from the fleet. Storing items
would only add their history and a `since` for conditions that do not record their
start. Neither is needed yet: a condition that needs a start time records it with the
condition itself, as a failed provisioning step will.
