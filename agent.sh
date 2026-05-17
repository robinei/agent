#!/usr/bin/env bash
set -euo pipefail

# ── Customize this ────────────────────────────────────────────────────────────
AGENT_CMD=(pi)

# Additional dirs outside the project the agent may write to.
WRITABLE_DIRS=(
    "$HOME/.pi"
)

# Credential/auth dirs to hide from the agent.
# Non-existent paths are skipped automatically.
CREDENTIAL_DIRS=(
    "$HOME/.ssh"
    "$HOME/.git-credentials"
    "$HOME/.config/gh"
    "$HOME/.aws"
    "$HOME/.azure"
    "$HOME/.config/gcloud"
    "$HOME/.config/heroku"
    "$HOME/.kube"
)
# ─────────────────────────────────────────────────────────────────────────────

if ! command -v bwrap &>/dev/null; then
    echo "error: bubblewrap (bwrap) not found. Install it with your package manager." >&2
    echo "  Arch/CachyOS:  sudo pacman -S bubblewrap" >&2
    echo "  Debian/Ubuntu: sudo apt install bubblewrap" >&2
    echo "  Fedora:        sudo dnf install bubblewrap" >&2
    exit 1
fi

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WRITABLE_ARGS=()
for dir in "${WRITABLE_DIRS[@]}"; do
    mkdir -p "$dir"
    WRITABLE_ARGS+=(--bind "$dir" "$dir")
done

CRED_ARGS=()
for dir in "${CREDENTIAL_DIRS[@]}"; do
    if [[ -e "$dir" ]]; then
        CRED_ARGS+=(--tmpfs "$dir")
    fi
done

echo "Sandboxing agent to: $PROJECT_DIR"

cd "$PROJECT_DIR"

exec bwrap \
    --ro-bind / / \
    --bind "$PROJECT_DIR" "$PROJECT_DIR" \
    "${WRITABLE_ARGS[@]}" \
    "${CRED_ARGS[@]}" \
    --dev /dev \
    --proc /proc \
    --tmpfs /tmp \
    --unshare-all \
    --share-net \
    --new-session \
    --die-with-parent \
    -- "${AGENT_CMD[@]}"
