#!/usr/bin/env bash
# why: GitHub Release 只留最近 N 个版本；git tag 不删，方便对旧提交 checkout。
set -euo pipefail

keep="${1:-3}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"

if ! [[ "${keep}" =~ ^[0-9]+$ ]] || [[ "${keep}" -lt 1 ]]; then
  echo "keep must be a positive integer" >&2
  exit 1
fi

tags="$(
  gh release list --repo "${repo}" --limit 100 --json tagName,createdAt \
    --jq "sort_by(.createdAt) | reverse | .[${keep}:] | .[].tagName"
)"

if [[ -z "${tags}" ]]; then
  echo "[retain] nothing to delete (keep=${keep})"
  exit 0
fi

while IFS= read -r tag; do
  [[ -z "${tag}" ]] && continue
  printf '[retain] delete release %s (git tag kept)\n' "${tag}"
  gh release delete "${tag}" --repo "${repo}" --yes
done <<< "${tags}"
