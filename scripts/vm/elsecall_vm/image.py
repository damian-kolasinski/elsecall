"""What makes a guest quiet and ready for demos, as guest-side shell steps.

These are the settings an image keeps, so every clone starts without
dialogs or banners. Approving automation prompts needs a key press over VNC
and lives with the command that drives the screen.
"""

from __future__ import annotations

import re

# User agents whose banners cover demos: background-item notices ("App
# Background Activity"), Tips ("See what's new in macOS") and update offers.
# Disabling them changes only notifications: launch agents still load.
QUIET_AGENTS = (
    "com.apple.backgroundtaskmanagement.agent",
    "com.apple.tipsd",
    "com.apple.SoftwareUpdateNotificationManager",
)

# Apps a guest command may drive with Apple Events. Each asks once whether
# the Tart guest agent may control it; the image keeps the answer.
AUTOMATED_APPS = {
    "Terminal": 'tell application "Terminal" to count windows',
    "System Events": 'tell application "System Events" to count processes',
    "Finder": 'tell application "Finder" to count windows',
}

QUIET = "\n".join(
    [
        "set -eu",
        "uid=$(id -u)",
        f"for label in {' '.join(QUIET_AGENTS)}; do",
        '  launchctl disable "gui/$uid/$label"',
        '  launchctl bootout "gui/$uid/$label" 2>/dev/null || true',
        "done",
        "sudo -n softwareupdate --schedule off >/dev/null",
        # No window restoration: clones start with an empty desktop and no
        # shell history from whoever built the base image.
        "defaults write NSGlobalDomain NSQuitAlwaysKeepsWindows -bool false",
        "defaults write com.apple.Terminal NSQuitAlwaysKeepsWindows -bool false",
        "defaults write com.apple.loginwindow TALLogoutSavesState -bool false",
        "defaults write com.apple.loginwindow LoginwindowLaunchesRelaunchApps -bool false",
        # Desktop widgets show the weather for wherever the host is.
        "defaults write com.apple.WindowManager StandardHideWidgets -int 1",
        "defaults write com.apple.WindowManager StageManagerHideWidgets -int 1",
        "rm -f ~/.zsh_history",
        "rm -rf ~/.zsh_sessions",
    ]
)

# Readable text in recordings: the default profile at 15 pt, 100x30.
TERMINAL_PROFILE = """tell application "Terminal"
  set profile to default settings
  set font size of profile to 15
  set number of columns of profile to 100
  set number of rows of profile to 30
  set startup settings to profile
end tell"""

QUIT_TERMINAL = 'tell application "Terminal" to quit'

# Notifications posted before the notifiers were disabled wait in the
# notification database and show at the next login unless removed.
QUIET_SENDERS = ("com.apple.btmnotificationagent", "com.apple.tips", "com.apple.softwareupdatenotification")
_SENDERS = ", ".join(f"'{sender}'" for sender in QUIET_SENDERS)
_NOTIFICATIONS = '"$HOME/Library/Group Containers/group.com.apple.usernoted/db2/db"'
CLEAR_BANNERS = (
    f"sqlite3 {_NOTIFICATIONS} \"delete from record where app_id in "
    f"(select app_id from app where identifier in ({_SENDERS}));\""
)

# A graceful shutdown logs out first, so loginwindow saves no apps to reopen.
SHUT_DOWN = 'tell application "System Events" to shut down'

# What `check` expects of a clone that just booted.
CHECK = "\n".join(
    [
        "uid=$(id -u)",
        f"for label in {' '.join(QUIET_AGENTS)}; do",
        '  launchctl print-disabled "gui/$uid" | grep -q "\\"$label\\" => disabled" '
        '|| echo "notifier still enabled: $label"',
        "done",
        "pgrep -x BackgroundTaskManagementAgent >/dev/null && echo 'background-item notifier running'",
        "pgrep -x Terminal >/dev/null && echo 'Terminal reopened at login'",
        f"[ \"$(sqlite3 {_NOTIFICATIONS} \"select count(*) from record where app_id in "
        f"(select app_id from app where identifier in ({_SENDERS}));\")\" = 0 ] "
        "|| echo 'banners waiting to show'",
        "[ \"$(defaults read com.apple.WindowManager StandardHideWidgets 2>/dev/null)\" = 1 ] "
        "|| echo 'desktop widgets shown'",
        "true",
    ]
)

# A Mac that never had mise: drop the Homebrew formula, its data and the
# shell lines that activate it.
FRESH = "\n".join(
    [
        "set -eu",
        "/opt/homebrew/bin/brew uninstall --force mise >/dev/null 2>&1 || true",
        "rm -rf ~/.local/share/mise ~/.local/state/mise ~/.config/mise ~/.cache/mise ~/.local/bin/mise",
        "for f in ~/.zshrc ~/.zprofile ~/.bashrc ~/.bash_profile ~/.profile; do",
        # Some of these are symlinks; rewrite the file they point to.
        "  if [ -f \"$f\" ] && grep -q mise \"$f\"; then",
        "    kept=$(grep -v mise \"$f\" || true)",
        "    printf '%s\\n' \"$kept\" > \"$f\"",
        "  fi",
        "done",
        "if command -v mise >/dev/null 2>&1 || zsh -ilc 'command -v mise' >/dev/null 2>&1; then",
        "  echo 'mise is still reachable' >&2; exit 1",
        "fi",
    ]
)


def rename(hostname: str) -> str:
    """The guest script that names the Mac, as a fleet member would be named."""
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,62}", hostname):
        raise ValueError(f"hostname {hostname!r} must be lowercase letters, digits and dashes")
    return "\n".join(
        f"sudo -n scutil --set {key} {hostname}" for key in ("ComputerName", "LocalHostName", "HostName")
    )
