# picapica

[中文](README.zh.md)

Pull-through proxy for **Docker**, **Ubuntu**, and **Fedora**. Clients talk to this process as a local software source. On a miss it fetches from upstream and stores the object; on a hit it serves locally. It does not mirror entire registries.

A repository can list several upstreams. picapica probes them, uses the fastest, and fails over. Each upstream is either `proxy: direct` or `proxy: default` (the optional `proxy_url` egress).

## Quick start

You need Docker. Copy the example config, then start the server. The first run builds the image locally (Rust is compiled inside Docker). Later you can pull `ghcr.io/rayz2099/picapica:latest` instead.

```bash
git clone https://github.com/rayz2099/picapica.git
cd picapica
cp config.example.yaml config.yaml
docker compose up -d --build
```

Open http://127.0.0.1:8080/ — the control-plane token in the example is `picapica`. Change `api_keys` before you expose the port.

### Docker client

HTTP registries must be allowed. On the Docker host:

```json
{
  "insecure-registries": ["127.0.0.1:8080"],
  "registry-mirrors": ["http://127.0.0.1:8080"]
}
```

Then:

```bash
docker pull 127.0.0.1:8080/docker/library/busybox
```

With the `localhost` / `127.0.0.1` aliases in the example, `registry-mirrors` sends Hub pulls through picapica on `/v2/...`.

### Ubuntu (apt)

```text
deb http://127.0.0.1:8080/ubuntu jammy main
```

### Fedora (dnf)

Paths after `/fedora/` match the upstream tree, for example:

```text
baseurl=http://127.0.0.1:8080/fedora/releases/$releasever/Everything/$basearch/os/
```

## Binary install

Tagged releases (`vX.Y.Z`) publish archives for linux / macOS / Windows, amd64 and arm64. Linux binaries are musl-static.

```bash
# example: linux amd64
tar -xzf picapica-linux-amd64.tar.gz
./picapica serve --config config.yaml
```

Image:

```bash
docker pull ghcr.io/rayz2099/picapica:0.1.0
```

Only the latest three GitHub Releases and GHCR versions are kept. `latest` always points at the newest tag.

## Configure

`config.example.yaml` is the template. On first start, missing `config.yaml` is copied from it. YAML is the source of truth (hot-reloaded). SQLite only stores refs, progress, and stats; blobs are content-addressed on disk.

| Field | Meaning |
| --- | --- |
| `listen` | HTTP bind address |
| `data_dir` | Blobs + SQLite |
| `proxy_url` | Optional HTTP or SOCKS5 egress |
| `api_keys` | Bearer tokens for the Web UI / HTTP API / CLI |
| `repos[].name` | URL prefix: `/{name}/...` |
| `repos[].type` | `docker` / `ubuntu` / `fedora` |
| `repos[].aliases` | Extra names (Docker also matches `Host`) |
| `repos[].upstreams` | Fetch URLs, each with `proxy: direct` or `default` |

Data plane (docker / apt / dnf) is anonymous. Control plane requires a token. Private upstream auth is out of scope.

CLI (same binary; talks to a running `serve`):

```bash
picapica serve
picapica probe
picapica stats
picapica cache ls
picapica cache rm <repo> <namespace>
```

## Build from source

```bash
just                 # list recipes
just compile
just serve           # cargo run -- serve --config config.yaml
just check           # fmt + clippy -D warnings + tests
```

Requires a Rust toolchain. `just dev` rebuilds on change (`cargo-watch`).

## License

[Apache License 2.0](LICENSE) © 2026 rayz2099
