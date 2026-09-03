# picapica 本地开发与镜像入口
set shell := ["bash", "-cu"]

# 列出命令
default:
    @just --list

# 调试编译
compile:
    cargo build -p picapica

# 发布构建
build:
    cargo build --release -p picapica

# 本地打 picapica:local
build-image:
    bash scripts/build-image.sh

# fmt + clippy -D warnings + test
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# 跑测试
test:
    cargo test --workspace

# 格式化
fmt:
    cargo fmt --all

# 生成并安装 fish 补全
completions:
    mkdir -p "${HOME}/.config/fish/completions"
    cargo run -q -p picapica -- completions fish > completions/picapica.fish
    cp completions/picapica.fish "${HOME}/.config/fish/completions/picapica.fish"

# 启动 serve
serve *args:
    cargo run -p picapica -- serve --config ${PICAPICA_CONFIG:-config.yaml} {{args}}

# 调试：增量编译，源码变更后重启 serve
dev *args:
    bash scripts/dev.sh {{args}}

# 对所有仓库上游测速
probe *args:
    cargo run -p picapica -- probe --config ${PICAPICA_CONFIG:-config.yaml} {{args}}

# 核心 HTTP 集成测试（需先 just dev）
it-core:
    bun test src/it/core.test.ts

# Docker 客户端端到端测试（需先 just dev，并已启动 Docker）
it-docker:
    bun test src/it/docker-pull.test.ts

# 完整集成测试
it:
    just it-core
    just it-docker

# 集成测试（需先 just dev）
verify:
    just it

docker-up:
    docker compose up -d --build

docker-down:
    docker compose down
