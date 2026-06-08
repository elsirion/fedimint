#!/usr/bin/env bash
set -euo pipefail

require_env() {
  local name="$1"
  if [ -z "${!name:-}" ]; then
    echo "Missing required environment variable: ${name}" >&2
    exit 1
  fi
}

require_env BACKPORT_TOKEN
require_env GITHUB_ACTOR
require_env GITHUB_EVENT_NAME
require_env GITHUB_EVENT_PATH
require_env GITHUB_REPOSITORY
require_env GITHUB_WORKSPACE
require_env PPQ_KEY
require_env RUNNER_TEMP

PPQ_MODEL="${PPQ_MODEL:-openai/gpt-5.5}"

sandbox_root="${RUNNER_TEMP}/codex-agent"
sandbox_home="${sandbox_root}/home"
codex_home="${sandbox_root}/codex-home"
context_dir="${sandbox_root}/context"

rm -rf "${sandbox_root}"
mkdir -p \
  "${sandbox_home}" \
  "${codex_home}" \
  "${context_dir}"

cat > "${codex_home}/config.toml" <<EOF
model = "${PPQ_MODEL}"
model_provider = "ppq"
sandbox_mode = "danger-full-access"
approval_policy = "never"

[model_providers.ppq]
name = "PPQ"
base_url = "https://api.ppq.ai/v1"
env_key = "PPQ_KEY"
wire_api = "responses"
EOF

jq '{
  action,
  repository: env.GITHUB_REPOSITORY,
  event_name: env.GITHUB_EVENT_NAME,
  actor: env.GITHUB_ACTOR,
  comment: .comment,
  issue: .issue,
  pull_request: .pull_request,
  review: .review
}' "${GITHUB_EVENT_PATH}" > "${context_dir}/event.json"

cat > "${context_dir}/prompt.md" <<'EOF'
You are fedimint-bot, an automation agent for the Fedimint GitHub repository.
An organization member mentioned @fedimint-bot in a GitHub issue, pull request,
or pull request review thread. Decide what action is useful from the context.

You may:
- answer the question directly in the relevant GitHub thread;
- inspect the repository, PR diff, CI state, linked issues, or previous comments;
- do preliminary research and post findings;
- implement a small, low-risk fix, push a branch, and open a draft PR;
- open a follow-up issue when that is the most appropriate outcome.

Use judgment. Prefer a concise answer when the user is asking a question. Only
change code for simple, well-scoped fixes. Do not make broad refactors. Do not
push to contributor PR branches. If you implement a fix, create a new branch
named like `fedimint-bot/<short-topic>-<run-id>` and open a draft PR.

Available tools:
- `gh`, authenticated as fedimint-bot via `GH_TOKEN`;
- `git`;
- normal shell tools including `bash`, `jq`, `rg`, `sed`, and `awk`;
- Codex can run commands and edit files freely inside this bubblewrap sandbox.

Repository conventions:
- Follow AGENTS.md and any nested repository instructions.
- For Rust code, avoid `unwrap()` in non-test code; use `expect()` with an
  invariant message or propagate errors.
- Use structured logging where relevant.
- Run `just format` after code changes if the toolchain is available.
- Run focused checks when practical and report what was or was not verified.

Operational rules:
- The GitHub event payload is at `/agent/context/event.json`.
- The checked out repository is at `/workspace`.
- First inspect `/agent/context/event.json` to understand whether this is an
  issue comment, PR comment, or inline PR review comment.
- Use `gh` to fetch additional context as needed.
- For inline PR review comments, `gh api
  repos/:owner/:repo/pulls/comments/:comment_id/replies -f body=...` can reply
  directly to the review thread.
- If responding to a pull request review comment, prefer replying to that
  review comment thread when possible. Otherwise comment on the issue or PR.
- Before finishing, post a GitHub comment/reply, open a GitHub issue, or open a
  draft PR, unless the safest action is explicitly to do nothing.
- If you cannot complete the requested action, post a short comment explaining
  the blocker.
- Avoid mentioning secrets or environment variable values in comments, logs,
  commits, branches, or PR descriptions.
- Do not wait for human input; make a reasonable decision and act.
EOF

bwrap_args=(
  --unshare-all
  --share-net
  --die-with-parent
  --new-session
  --proc /proc
  --dev /dev
  --tmpfs /tmp
  --ro-bind /nix/store /nix/store
  --ro-bind /etc /etc
  --bind "${GITHUB_WORKSPACE}" /workspace
  --bind "${sandbox_root}" /agent
  --bind "${sandbox_home}" /home/codex
  --bind "${codex_home}" /codex-home
  --chdir /workspace
  --clearenv
  --setenv HOME /home/codex
  --setenv CODEX_HOME /codex-home
  --setenv GITHUB_ACTOR "${GITHUB_ACTOR}"
  --setenv GITHUB_EVENT_NAME "${GITHUB_EVENT_NAME}"
  --setenv GITHUB_REPOSITORY "${GITHUB_REPOSITORY}"
  --setenv GITHUB_RUN_ID "${GITHUB_RUN_ID:-}"
  --setenv GITHUB_SERVER_URL "${GITHUB_SERVER_URL:-https://github.com}"
  --setenv GITHUB_WORKSPACE /workspace
  --setenv GH_TOKEN "${BACKPORT_TOKEN}"
  --setenv PPQ_KEY "${PPQ_KEY}"
  --setenv PATH "${PATH}"
)

for path in /bin /usr /lib /lib64 /run/current-system/sw; do
  if [ -e "${path}" ]; then
    bwrap_args+=(--ro-bind "${path}" "${path}")
  fi
done

if [ -e /nix/var/nix/daemon-socket ]; then
  bwrap_args+=(--bind /nix/var/nix/daemon-socket /nix/var/nix/daemon-socket)
fi

if [ "$(id -G | wc -w)" -gt 1 ]; then
  echo "Warning: unable to clear supplementary groups in this runner context" >&2
fi

# shellcheck disable=SC2016
exec bwrap "${bwrap_args[@]}" bash -euo pipefail -c '
  git config --global --add safe.directory /workspace
  git config --global user.name fedimint-bot
  git config --global user.email fedimint-bot@users.noreply.github.com
  gh auth status >/dev/null
  gh auth setup-git
  codex exec \
    --ephemeral \
    --sandbox danger-full-access \
    --ask-for-approval never \
    "$(cat /agent/context/prompt.md)"
'
