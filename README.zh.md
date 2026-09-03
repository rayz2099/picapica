# picapica

[English](README.md)

内网软件源：Docker / Ubuntu / Fedora。客户端打本机，没有就回源落下，有就本地服务。

![picapica](picapica.png)

## 启动

```bash
git clone https://github.com/rayz2099/picapica.git
cd picapica
mkdir -p config
cp config.example.yaml config/config.yaml
docker compose up -d
```

从旧版 Compose 升级时，先执行 `mkdir -p config && mv config.yaml config/config.yaml`，再重新创建容器。新版容器带健康检查与 `unless-stopped` 自动重启。

打开 http://127.0.0.1:8080/ ，令牌 `picapica`。对公网暴露前改 `api_keys`。

## 当源用

**Docker**（宿主机）：

```bash
cat > /etc/docker/daemon.json <<'EOF'
{
  "insecure-registries": ["127.0.0.1:8080"],
  "registry-mirrors": ["http://127.0.0.1:8080"]
}
EOF
docker pull 127.0.0.1:8080/docker/library/busybox
```

**apt**

```bash
cat > /etc/apt/sources.list.d/ubuntu.list <<'EOF'
deb http://127.0.0.1:8080/ubuntu jammy main
EOF
apt-get update
```

**dnf**

```bash
cat > /etc/yum.repos.d/fedora.repo <<'EOF'
[fedora]
name=fedora
baseurl=http://127.0.0.1:8080/fedora/releases/$releasever/Everything/$basearch/os/
enabled=1
gpgcheck=0
EOF
dnf makecache
```

仓库页里点「教程」可复制当前实例的同样脚本。

## 配置

配置文件是事实源（Compose 使用 `config/config.yaml`，直接运行二进制默认使用 `config.yaml`）。仓库、出站、令牌等热加载；`listen`、`data_dir`、`probe_interval` 修改后需重启。WebUI 设置页可改 JSON。SQLite 只记引用和统计，制品按哈希落盘。

| 字段 | 含义 |
| --- | --- |
| `listen` | HTTP 监听；修改后需重启 |
| `data_dir` | 制品 + SQLite；修改后需重启 |
| `proxy_url` | HTTP / HTTPS / SOCKS5 出站；上游 `proxy: default` 才走它 |
| `probe_interval` | 周期测速间隔：`30s`、`10m`、`1h`；修改后需重启 |
| `cache` | `true` 落盘；`false` 只转发 |
| `cache_max_bytes` | 可选容量上限；清理时优先回收无引用、过期及最久未使用制品 |
| `cache_ttl` | 可选缓存 TTL，格式同 `probe_interval` |
| `public_url` | 可选对外地址，用于 WebUI 客户端样例和 Docker Host 别名 |
| `api_keys` | 控制面 Bearer 令牌；至少一个 |
| `repos[].name` / `type` | URL 前缀 `/{name}/...`；类型为 `docker`、`ubuntu`、`fedora` |
| `repos[].aliases` | 同一仓库的可选 Host/路径别名 |
| `repos[].username/password` | 可选上游基础认证 |
| `repos[].upstreams[].url` | HTTP(S) 回源地址 |
| `repos[].upstreams[].proxy` | `direct`、`default`、`none` 或该上游专用代理 URL |

数据面匿名。控制面要令牌。

## 运维

本机命令从 `--config` 读取监听地址和第一枚令牌。管理远端时必须显式给 URL 和令牌；所有管理请求默认总超时 30 秒，可用 `--timeout` 调整。

```bash
picapica health
picapica status
picapica transfers --limit 100
picapica probe run
picapica probe results
picapica config validate config.yaml
picapica config show
picapica config apply config.yaml
picapica cache ls [query]
picapica cache tree <repo> [prefix] --page 1 --per-page 50
picapica cache rm <repo> <namespace>
picapica cache prefix rm <repo> <prefix>
picapica cache prune --dry-run
picapica cache prune

picapica --url https://pica.example.com --token '<token>' status
```

WebUI 总览显示健康状态、缓存命中/未命中、上游失败和活跃传输，并可预览或执行清理。`health` 无需令牌，适合容器编排探针；其余控制面命令需要令牌。

## 二进制

[Releases](https://github.com/rayz2099/picapica/releases) 提供 linux / macOS / Windows（amd64、arm64）。Linux 为 musl 静态链接。每次发布附 `SHA256SUMS`；用 `sha256sum <压缩包>` 与其中对应行比对。当前不引入外部签名密钥，因此不提供制品签名。只保留最近 3 个版本。

```bash
./picapica serve --config config.yaml
```

或 `docker pull ghcr.io/rayz2099/picapica:latest`。

源码：`just compile` / `just serve` / `just check`（需要 Rust）。

## 许可

[Apache License 2.0](LICENSE) © 2026 rayz2099
