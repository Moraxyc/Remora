# 开发与构建指南

[简体中文](development.md) | [English](../en/development.md) | [返回总览](../../README.md)

## 1. 开发环境

仓库通过 Nix Flake 提供统一的开发环境（包含 Rust 工具链、`rust-analyzer`、`nodejs`、`treefmt` 与 `fetchrix`）：

```sh
nix develop
```

## 2. 构建与前后端联调

`crates/remora-cloud/build.rs` 在编译期通过 `rust-embed` 将前端静态构建产物嵌入 `remora` 二进制。构建脚本默认读取仓库内的 `web/build/` 目录（要求其中存在 `app.html`），也可通过环境变量 `REMORA_WEB_DIR=/path/to/build` 指定外部产物路径（Nix 构建流程即通过该变量传入 `.#remora-web`）。首次在本地执行 `cargo build` 或 `cargo test` 前，先构建一次前端：

```sh
(cd web && npm ci && npm run build)
cargo build
```

调试 `web/` 下的 SvelteKit 单页应用时，可在两个终端分别启动后端与 Vite 开发服务器。Vite 提供热更新，并根据 `web/vite.config.ts` 将 `/api` 请求代理至 `REMORA_URL`（默认为 `http://[::1]:8680`）：

```sh
# 终端 1：使用本地开发配置启动后端
cargo run --package remora-cloud -- --config dev-remora.toml

# 终端 2：启动前端热更新服务器
cd web
REMORA_URL=http://[::1]:8680 npm run dev
```

在 `web/` 目录下运行 `npm run check` 可执行 Svelte 与 TypeScript 类型检查。

## 3. 代码检查与测试

提交代码前运行以下命令完成格式化检查、Clippy 静态分析与单元/集成测试：

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
nix fmt
```

在 Linux 主机上执行 `nix flake check` 会构建所有 Flake 输出包，运行 `clippy` 与 `treefmt`，并启动 [`nix/tests/remora.nix`](../../nix/tests/remora.nix) 中的双节点 NixOS 虚拟机端到端测试。该测试在虚拟机中运行 `services.remora` 与 `services.remora.coturn`，由 Python 测试客户端验证首次连接触发 `remora.socket` 按需启动、扫码登录、房间创建与加入、`be-controlled` / `soac` 信令转发、`turnutils_uclient` 对临时 TURN 凭据的实测校验、服务重启后的 SQLite 数据持久化，以及扫描 `journalctl` 验证密钥与令牌脱敏。

## 4. Flake 构建产物

| Flake Output | 说明 |
|---|---|
| `packages.<sys>.remora` | 默认软件包，动态链接系统 `aws-lc` 与 `sqlite`，附带 systemd 服务与 socket 单元。 |
| `packages.<sys>.remora-web` | SvelteKit 前端静态构建产物。 |
| `packages.<sys>.remora-static` | 基于 musl 静态链接并剥离符号的独立 Linux 二进制，用于制作 Release 压缩包。 |
| `packages.<sys>.remora-deb` | Debian / Ubuntu `.deb` 安装包，CI 中通过 `lintian` 与 systemd 容器脚本测试安装、升级、卸载与 purge。 |
| `packages.<sys>.uu-remote-linux` | 打入自定义 `BASE_URL` 与 `SIGN_KEY` 支持补丁的 Linux 客户端包。 |
| `nixosModules.default` | NixOS 服务模块（支持同时配置 `remora` 与本机 `coturn`）。 |

构建指定输出包：

```sh
nix build .#remora-static
nix build .#remora-deb
```
