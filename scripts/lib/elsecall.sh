# The consumer-kit guard from README: PATH, standard install link, then direct.
EC=$(command -v elsecall || echo "$HOME/.local/bin/elsecall")
ec() { if [ -x "$EC" ]; then "$EC" "$@"; else while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done; [ $# -gt 0 ] && shift; [ $# -eq 0 ] || ELSECALL_ADMISSION=missing "$@"; fi; }
