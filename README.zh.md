# picapica

[English](README.md)

Docker / Ubuntu / Fedora 的按请求软件源代理。客户端把本机进程当源：没有就回源落下，有就本地服务。不做整库镜像。

一个仓库可以挂一组上游。测速后用最快的，失败再切。每个上游声明 `proxy: direct`（直连）或 `proxy: default`（走可选的 `proxy_url` 出站）。

## 快速开始

需要 Docker。复制示例配置后启动。第一次会在镜像里编译（宿主机不必装 Rust）。之后也可以直接拉 `ghcr.io/rayz2099/picapica:latest`。

```bash
git clone https://github.com/rayz2099/picapica.git
cd picapica
cp config.example.yaml config.yaml
docker compose up -d --build
```

打开 http://127.0.0.1:8080/ 。示例控制面令牌是 `picapica`。对公网暴露前先改 `api_keys`。

### Docker 客户端

HTTP 仓库必须加入 insecure。在 Docker 宿主机：

```json
{
  "insecure-registries": ["127.0.0.1:8080"],
  "registry-mirrors": ["http://127.0.0.1:8080"]
}
```

然后：

```bash
docker pull 127.0.0.1:8080/docker/library/busybox
```

示例给 docker 仓库配了 `localhost` / `127.0.0.1` 别名，所以 `registry-mirrors` 会把 Hub 的拉取打到 picapica 的 `/v2/...`。

### Ubuntu (apt)

```text
deb http://127.0.0.1:8080/ubuntu jammy main
```

### Fedora (dnf)

`/fedora/` 后面的路径跟上游目录树一致，例如：

```text
baseurl=http://127.0.0.1:8080/fedora/releases/$releasever/Everything/$basearch/os/
```

## 二进制

打 `vX.Y.Z` tag 会发布 linux / macOS / Windows，amd64 与 arm64。Linux 是 musl 静态链接。

```bash
tar -xzf picapica-linux-amd64.tar.gz
./picapica serve --config config.yaml
```

镜像：

```bash
docker pull ghcr.io/rayz2099/picapica:0.1.0
```

GitHub Release 和 GHCR 只保留最近 3 个版本。`latest` 指向最新 tag。

## 配置

`config.example.yaml` 是模板。第一次启动若没有 `config.yaml` 会从它复制。YAML 是事实源（热加载）。SQLite 只记引用、进度、统计；制品按哈希落盘。

| 字段 | 含义 |
| --- | --- |
| `listen` | HTTP 监听 |
| `data_dir` | 制品 + SQLite |
| `proxy_url` | 可选，HTTP 或 SOCKS5 出站 |
| `api_keys` | Web UI / HTTP API / CLI 的 Bearer |
| `repos[].name` | URL 前缀 `/{name}/...` |
| `repos[].type` | `docker` / `ubuntu` / `fedora` |
| `repos[].aliases` | 别名（Docker 还匹配 `Host`） |
| `repos[].upstreams` | 回源 URL，各带 `proxy: direct` 或 `default` |

数据面（docker / apt / dnf）匿名。控制面要令牌。私有上游鉴权不做。

CLI（同一个二进制，打正在跑的 `serve`）：

```bash
picapica serve
picapica probe
picapica stats
picapica cache ls
picapica cache rm <repo> <namespace>
```

## 从源码构建

```bash
just                 # 列出配方
just compile
just serve           # cargo run -- serve --config config.yaml
just check           # fmt + clippy -D warnings + tests
```

需要 Rust。`just dev` 在改动后增量重编（`cargo-watch`）。

## 许可

[Apache License 2.0](LICENSE) © 2026 rayz2099
