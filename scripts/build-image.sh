#!/usr/bin/env bash
# why: 本地只打 picapica:local；公开镜像由打 tag 的 GitHub Action 推 GHCR。
set -euo pipefail

readonly ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly LOCAL_IMAGE="picapica:local"

main() {
  printf '[build-image] local_image=%s\n' "${LOCAL_IMAGE}"
  cd "${ROOT_DIR}"
  docker build -t "${LOCAL_IMAGE}" .
}

main "$@"
