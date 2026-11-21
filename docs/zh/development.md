# 开发与构建指南

[简体中文](development.md) | [English](../en/development.md) | [返回总览](../../README.md)

## 1. 开发环境

仓库通过 Nix Flake 提供统一的开发环境（包含 Rust 工具链、`cargo-deny`、`rust-analyzer`、`nodejs` 与 `treefmt`）：

```sh
nix develop
```

仅开发使用的输入（包括 `treefmt-nix`、`moraxyc-repo` 以及用于集成测试的上游 `uu-remote-linux`）放在 `dev/flake.nix` 中，不会进入下游用户的 `flake.lock`。

## 2. 构建与前后端联调

`crates/remora-proto/build.rs` 通过纯 Rust 的 `protox` 与 `prost-build` 编译 `proto/gv_pb.proto`（无需系统安装 `protoc`），并在编译期校验、合并 `profiles/*.toml` 为内嵌 JSON。

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

## 3. 代码检查、边界校验与测试

提交代码前运行以下命令完成格式化检查、Clippy 静态分析、单元测试与依赖边界检查：

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny --config deny.toml check bans licenses sources
cargo deny --manifest-path crates/remora-proto/Cargo.toml --config crates/remora-proto/deny.toml check bans
cargo deny --manifest-path crates/remora-endpoint/Cargo.toml --config crates/remora-endpoint/deny.toml check bans licenses sources
cargo deny --manifest-path crates/remora-lab/Cargo.toml --config crates/remora-lab/deny.toml check bans licenses sources
nix fmt
```

### 使用 `remora-lab` 运行兼容场景与矩阵测试

`compat/` 目录包含云端与会话兼容场景（`compat/scenarios/`）、线协议测试向量（`compat/vectors/`）、Trace JSON Schema（`compat/schema/`）以及外部端点驱动协议（[`compat/driver-protocol.md`](../../compat/driver-protocol.md)）。构建工作区后可直接用 `remora-lab` 运行本地验证：

```sh
# 运行所有云端与会话场景（自动拉起单进程 remora 与测试 TURN）
cargo run --package remora-lab -- run compat/scenarios --remora-bin target/debug/remora

# 对比单进程（standalone）与多进程角色拆分（split）的归一化线迹差异
cargo run --package remora-lab -- matrix compat/scenarios --mode cloud --remora-bin target/debug/remora

# 运行 P2P 与强制 TURN 中继会话矩阵
cargo run --package remora-lab -- matrix compat/scenarios/session --mode session --remora-bin target/debug/remora
```

### NixOS 虚拟机端到端测试

在 Linux 主机上执行 `nix flake check` 会构建所有 Flake 输出包，运行 `fmt`、`clippy`、`test`、`deny-workspace` 与 `deny-proto`，并执行三组 NixOS 虚拟机端到端测试：

- [`nix/tests/remora.nix`](../../nix/tests/remora.nix)（`nixos`）：运行 `services.remora` 与 `services.remora.coturn`，由 Python 测试客户端验证 `remora.socket` 按需启动、扫码登录、房间创建与加入、`be-controlled` / `soac` 信令转发、`turnutils_uclient` 对临时 TURN 凭据的实测校验、服务重启持久化，以及 `journalctl` 脱敏审计。
- [`nix/tests/compat.nix`](../../nix/tests/compat.nix)（`compat`）：使用 `remora-lab` 针对 NixOS `remora` + `coturn` 服务运行全部云端与会话 YAML 场景，校验 `standalone` 与 `split` 角色拆分兼容矩阵输出完全一致且无密钥泄露。
- [`nix/tests/interop-linux.nix`](../../nix/tests/interop-linux.nix)（`interop-linux`）：同时拉起上游 `uu-remote-linux`（被控端 + 控制端）与 `remora-lab`，验证通过 Remora 的文件传输、`lab-lab-p2p` / `lab-lab-relay` / `lab-uu-remote-linux` 三列会话矩阵，以及 `uuremote-viewer` 与 `remora-lab serve-host` 之间的输入与视频帧互通。

## 4. Flake 构建产物

| Flake Output | 说明 |
|---|---|
| `packages.<sys>.remora` | 默认软件包，动态链接系统 `aws-lc` 与 `sqlite`，附带 systemd 服务与 socket 单元。 |
| `packages.<sys>.remora-lab` | `remora-lab` 一致性与场景测试工具二进制。 |
| `packages.<sys>.remora-web` | SvelteKit 前端静态构建产物。 |
| `packages.<sys>.remora-static` | 基于 musl 静态链接并剥离符号的独立 Linux 二进制，用于制作 Release 压缩包。 |
| `packages.<sys>.remora-deb` | Debian / Ubuntu `.deb` 安装包，CI 中通过 `lintian` 与 systemd 容器脚本测试安装、升级、卸载与 purge。 |
| `nixosModules.default` | NixOS 服务模块（支持同时配置 `remora` 与本机 `coturn`）。 |

构建指定输出包：

```sh
nix build .#remora-static
nix build .#remora-deb
```
