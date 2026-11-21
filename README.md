# Remora

[简体中文](README.md) | [English](README.en.md)

Remora 是用 Rust 编写的网易 UU 远程（UU Remote）协议生态非官方自托管重实现。核心包含可自托管的云端服务 `remora`（`remora-cloud`）、无 I/O 协议库 `remora-proto`，以及基于 `str0m` 的端点 SDK `remora-endpoint` 与一致性测试实验室 `remora-lab`。兼容 UU 协议的被控端（`uuremote-server`）与控制端（`uuremote-viewer`）将服务地址指向 Remora 后，通过它完成扫码登录、设备发现、房间创建与 WebRTC 信令交换；信令协商完成后，桌面音视频、输入控制、剪贴板、终端和文件传输流量直接在被控端与控制端之间通过 WebRTC 点对点传输，或经由 `coturn` 中继。

本项目独立开发，与网易公司无关联。

```text
uuremote-viewer ──REST + Socket.IO──► remora ◄──REST + Socket.IO── uuremote-server
       └─────────────── WebRTC (P2P，或经由 coturn 中继) ───────────────┘
```

## 现状

项目处于积极开发阶段，已支持 `uu-remote-linux` 的被控端与控制端完整接入，并补齐了 macOS / Windows 客户端控制面兼容接口与内置客户端画像（`macos-4.48.1`、`macos-4.48.2`、`windows-4.48.2`）。客户端展示登录二维码时，扫码或打开其中的链接会进入 Remora 内嵌的 Web 控制台；用户通过 OpenID Connect 提供商或管理员（Owner）密码登录控制台后，即可核对设备信息并确认客户端登录。同一控制台也用于查看已绑定设备及其桌面壁纸、解绑被控端、通过同一局域网内的在线设备网络唤醒（Wake-on-LAN）离线设备，以及吊销客户端或浏览器会话。

云端服务支持单进程全功能运行，也可按角色拆分（`--roles api,gateway,room,relay`）并通过本地 Unix 域套接字总线（`--bus-listen` / `--bus-connect`）多进程协同。工作区还包含 `remora-endpoint`（端点控制面、Socket.IO 与 `str0m` WebRTC 会话实现）与 `remora-lab`（YAML 场景回归、P2P/TURN 中继及跨实现互通矩阵测试工具）。

## 快速运行

```sh
remora --config remora.toml
```

[`contrib/remora.toml`](contrib/remora.toml) 给出了所有配置项的示例，[`contrib/turnserver.conf`](contrib/turnserver.conf) 是配套的 `coturn` 配置。生产环境应在 Remora 前置支持 WebSocket 升级的 TLS 反向代理，并在 `remora.toml` 中至少启用一种登录确认方式（`login.password_file`、`[oidc]` 或内网专用的 `login.auto_confirm = true`）。公网部署时还应配置 `sign_key_file`，让服务端校验客户端请求的 `x-param-sign` HMAC-SHA256 签名。

各发行版提供以下安装与接入方式：

- **Debian 与 Ubuntu**：可通过 [`pkgs.moraxyc.com`](https://pkgs.moraxyc.com) APT 仓库安装 `remora` 并随 `apt upgrade` 自动更新，或从 Release 下载 `amd64` 与 `arm64` 的 `.deb` 包。软件包提供沙箱化的 `remora.service` 与按需激活的 `remora.socket`，读取 `/etc/remora/remora.toml` 并将 SQLite 数据库保存在 `/var/lib/remora/remora.db`。
- **NixOS**：本仓库 Flake 导出 `nixosModules.default`，可同时配置 `services.remora` 与本机 `services.remora.coturn`，并通过 systemd `LoadCredential` 注入密钥文件。
- **客户端（`uu-remote-linux`）**：上游仓库提供了独立 Flake（`github:VallierDev/uu-remote-linux`，包含 `packages.uu-remote-linux` 与 `nixosModules.default`）。`0.2.62` 及以上版本已原生支持通过环境变量 `UU_BASE_URL` / `UU_SIGN_KEY` 或客户端配置文件中的 `"base_url"` / `"sign_key"` 指向你的 Remora 实例。

## 文档

- [部署与运维指南](docs/zh/deployment.md)：反向代理与 `trusted_proxies`、单进程与按角色拆分（`--roles` + `--bus-listen` / `--bus-connect`）、`.deb` / NixOS / 二进制安装、Owner 密码与 OIDC 认证、`coturn` 中继配置、客户端接入与故障排查。
- [系统架构与安全设计](docs/zh/architecture.md)：工作区 Crate 边界与依赖策略、无 I/O 状态机（`remora-proto`）、角色总线与每房间 Actor 模型（`remora-cloud`）、端点 SDK（`remora-endpoint`）与测试实验室（`remora-lab`）、HTTP 路由与 CSP、JWT 与浏览器会话存储、GCRA 限流与日志脱敏。
- [线协议与接口参考](docs/zh/protocol.md)：`x-param-*` 签名算法、`{code, msg, data}` 响应信封与错误码、REST 接口清单（含桌面端兼容与更新存根接口）、Engine.IO v4 / Socket.IO v5 信令帧、会话数据通道与 TURN 凭据派生。
- [开发与构建指南](docs/zh/development.md)：`nix develop` 开发环境、内嵌 SvelteKit 前端热更新联调、`cargo-deny` 边界检查、`remora-lab` 兼容场景与矩阵测试，以及 NixOS 虚拟机端到端测试。
- [外部端点驱动协议](compat/driver-protocol.md)：`remora-lab` 驱动外部端点进程使用的 JSON-RPC 2.0 协议规范。

## 致谢

Remora 对 UU Remote 协议行为的理解主要参考了 VallierDev 及贡献者开发的 Linux 客户端与被控端实现 [uu-remote-linux](https://github.com/VallierDev/uu-remote-linux)（GPL-3.0-or-later）。Remora 为独立编写的实现。

## 许可证

| 路径 | 许可证 |
|---|---|
| `crates/remora-proto`、`crates/remora-endpoint`、`crates/remora-lab` | MIT OR Apache-2.0 |
| `crates/remora-cloud` | GPL-3.0-or-later |
| 其余文件（含 `web/`、`compat/`） | MIT |

各目录的完整许可证文本见对应 `LICENSE*` 文件。
