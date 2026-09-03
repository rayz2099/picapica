#!/usr/bin/env bash
# why: 改 rs/html/svg 要增量重编并重启 serve；YAML 由进程内 watch 处理，不在这里重启。
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
test -f config.yaml || cp config.example.yaml config.yaml

if ! command -v cargo-watch >/dev/null 2>&1; then
  echo "just dev 需要 cargo-watch：cargo install --locked cargo-watch" >&2
  exit 1
fi

export CARGO_INCREMENTAL=1
export CARGO_TERM_COLOR=always
cfg="${PICAPICA_CONFIG:-config.yaml}"
extra=""
if [[ $# -gt 0 ]]; then
  extra="$(printf ' %q' "$@")"
fi
run="cargo run -p picapica -- serve --config $(printf '%q' "$cfg")${extra}"

exec cargo watch \
  --use-shell=bash \
  -w crates \
  -w Cargo.toml \
  -w Cargo.lock \
  -i data \
  -s "${run}"
