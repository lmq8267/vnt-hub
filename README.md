# vnt-hub

`vnt-hub` 是 VNT 组网体系的配置管理控制台。它面向多用户、多房间、多设备场景，负责集中管理客户端设备、维护分组配置、向在线设备下发组网参数、接收设备状态与事件，并提供整库迁移能力。

它不是 `vnts` 服务端的替代品，也不是组网数据转发节点。`vnt-hub` 的职责是“控制面”：配置、鉴权、状态、审计、备份。真正的组网连接仍由 `vnt` 客户端和 `vnts` 服务端完成。

## 项目关系

| 组件 | 作用 |
|------|------|
| `vnt` | 客户端，负责本机虚拟网卡、P2P/中继连接、流量转发 |
| `vnts` | 组网服务端，负责注册、发现、辅助打洞和中继 |
| `vnt-hub` | 配置管理控制台，负责房间、分组、设备接入、配置下发和迁移 |

典型流程：

1. 管理员在 `vnt-hub` 创建房间和分组。
2. 客户端使用 `room_id` 接入 `vnt-hub`。
3. 控制台生成设备 ID 与设备 token。
4. 管理员把设备加入分组，并下发组网配置。
5. 客户端应用配置后连接 `vnts`，并持续上报状态、事件和流量。

## 主要功能

### 多用户与权限

- 启动时自动创建 `admin` 管理员账号。
- `admin` 可管理全部用户和全部房间。
- 普通用户只能管理自己创建的房间、分组和设备。
- 可通过 `--disable-register` 禁止开放注册。
- 登录接口带失败锁定：连续失败 3 次后锁定 10 分钟。

### 房间与设备

- 房间 ID 使用 16 位小写十六进制字符串，全局唯一。
- 设备 ID 使用 UUID v4。
- 设备首次接入时由控制台生成 `device_id` 和 `device_token`。
- 后续接入必须携带 `room_id + device_id + device_token`。
- 设备状态支持 `pending`、`online`、`offline`、`kicked`。
- 设备状态页显示累计上行流量、累计下行流量和更新时间。

### 分组配置

- 房间内可创建多个分组。
- 分组配置通过表单填写，不要求用户手写 JSON。
- 支持 token、服务器地址、协议、虚拟 IP、DNS、STUN、端口映射、加密算法、打洞模式、传输模式、Hook 等参数。
- `token`、服务器地址、组网密码分别加密存储。
- 可对单个设备下发配置，也可对整个分组批量下发。

### Web 与客户端接入

- Web 控制台监听地址默认 `0.0.0.0:29876`。
- 客户端接入监听地址默认 `0.0.0.0:29878`。
- 同一端口支持 HTTP/HTTPS 自动识别。
- WebSocket 随页面协议使用 `ws://` 或 `wss://`。
- 首次启动会自动生成自签名 TLS 证书并保存到数据库。

### 事件与流量

- 客户端可上报事件：连接控制台、断开服务端、重连、配置应用、被踢出等。
- 客户端可上报 `TrafficStats`，控制台保存累计上下行流量。
- 兼容通过 `EventReport` 上报的 `traffic_stats` / `traffic` / `status` / `client_status` payload。

### 备份与迁移

- `admin` 可导出整库迁移文件。
- 普通用户可导出个人备份文件。
- 整库迁移文件可用于 Rust 版与 Cloudflare KV 版互迁。
- 导入支持 `merge` 和 `overwrite`。
- 导入完成后返回逐表统计，便于确认迁移结果。

## 快速启动

```bash
./vnt-hub \
  --listen 0.0.0.0:29876 \
  --console-listen 0.0.0.0:29878 \
  --db ./vnt-hub.db \
  --log-path console
```

打开控制台：

```text
http://服务器IP:29876
```

默认账号：

```text
用户名：admin
密码：admin
```

首次登录后应立即修改 admin 密码。

## 启动参数

```text
-a, --listen <ADDR>              Web 控制台 HTTP/HTTPS 复用监听地址，默认 0.0.0.0:29876
-c, --console-listen <ADDR>      客户端接入 HTTP/HTTPS/WS/WSS 复用监听地址，默认 0.0.0.0:29878
-d, --db <PATH>                  SQLite 数据库路径，默认 ./vnt-hub.db
-R, --disable-register           禁止新用户注册
-l, --log-path <LOG_PATH>        log 路径，console 输出到控制台，/dev/null 不输出
-h, --help                       打印帮助
-V, --version                    打印版本
```

## 环境变量

| 变量 | 说明 |
|------|------|
| `VNT_HUB_JWT_SECRET` | Web JWT 签名密钥，生产环境必须设置 |
| `CONSOLE_MASTER_KEY` | 控制台配置加密主密钥，生产环境必须设置 |

`CONSOLE_MASTER_KEY` 不会写入数据库。迁移数据库时不会包含该主密钥。若目标环境主密钥不同，历史加密配置可能无法解密，需要重新填写并下发配置。

## 本地构建

项目限制 Rust 版本不高于 1.77：

```bash
rustup install 1.77
rustup default 1.77
cargo build --release
```

生成文件：

```text
target/release/vnt-hub
```

## musl 静态编译

Linux x86_64 静态包：

```bash
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C target-feature=+crt-static -C strip=symbols" \
  cargo build --release --target x86_64-unknown-linux-musl
```

生成文件：

```text
target/x86_64-unknown-linux-musl/release/vnt-hub
```

## GitHub Actions 打包

工作流文件：

```text
.github/workflows/rust.yml
```

触发条件：

- push 到 `main`
- push tag
- 手动 `workflow_dispatch`

流程：

1. 使用 Rust 1.77 执行 `cargo check --locked`。
2. 执行 `cargo test --locked`。
3. 按 matrix 编译 Linux musl、Windows 和 macOS 二进制。
4. Linux 目标使用 UPX 压缩二进制。
5. 打包 `vnt-hub` 和 `README.md`。
6. tag 构建时上传到 GitHub Releases。

当前打包架构：

| Target | 说明 |
|--------|------|
| `x86_64-unknown-linux-musl` | Linux x86_64 |
| `i686-unknown-linux-musl` | Linux x86 32 位 |
| `aarch64-unknown-linux-musl` | Linux ARM64 |
| `armv7-unknown-linux-musleabihf` | ARMv7 硬浮点 |
| `armv7-unknown-linux-musleabi` | ARMv7 软浮点 |
| `arm-unknown-linux-musleabihf` | ARM 32 位硬浮点 |
| `arm-unknown-linux-musleabi` | ARM 32 位软浮点 |
| `mipsel-unknown-linux-musl` | MIPS little-endian musl |
| `mips-unknown-linux-musl` | MIPS big-endian musl |
| `x86_64-apple-darwin` | macOS Intel |
| `aarch64-apple-darwin` | macOS Apple Silicon |
| `i686-pc-windows-msvc` | Windows x86 |
| `x86_64-pc-windows-msvc` | Windows x86_64 |

包名格式：

```text
vnt-hub-<target>-<tag-or-sha>.tar.gz
```

包内文件：

```text
vnt-hub
README.md
```

Windows 包内二进制文件名为 `vnt-hub.exe`。macOS 目标不是 musl 静态链接；Linux 目标使用 musl 静态链接。MIPS 目标使用与客户端类似的 musl 交叉工具链和 nightly `build-std` 构建流程。

## 数据库与迁移

默认数据库路径：

```text
./vnt-hub.db
```

数据库中包含：

- 用户和密码哈希
- 房间、分组、设备
- 设备 token
- 分组加密配置
- 事件和配置下发记录
- 自签名 TLS 证书和私钥

### admin 整库迁移

整库迁移用于 Rust 版与 Cloudflare KV 版之间迁移全部数据。

Web 控制台：

```text
备份 -> 整库迁移（admin） -> 导出备份
```

API 导出：

```bash
curl -H "Authorization: Bearer <ADMIN_TOKEN>" \
  "http://127.0.0.1:29876/api/backup/export?scope=full" \
  -o vnt-hub-full-migration.json
```

整库导出包含：

- `users`
- `rooms`
- `groups`
- `devices`
- `events`
- `config_pushes`
- `system_config`

`system_config` 中包含自签名 TLS 证书和私钥，因此恢复后客户端无需重新信任新的证书。

API 导入：

```bash
curl -X POST \
  -H "Authorization: Bearer <ADMIN_TOKEN>" \
  -H "Content-Type: application/json" \
  --data-binary @vnt-hub-full-migration.json \
  "http://127.0.0.1:29876/api/backup/import?mode=merge"
```

导入模式：

- `merge`：按 ID 合并，已存在记录跳过。
- `overwrite`：清空现有数据后导入。

导入接口返回逐表统计：

```json
{
  "ok": true,
  "mode": "merge",
  "source_mode": "sqlite",
  "scope": "full",
  "tables": [
    { "table": "users", "attempted": 1, "imported": 1, "skipped": 0 }
  ]
}
```

### 普通用户个人备份

普通用户只能导出自己的数据：

```text
备份 -> 个人备份 -> 导出备份
```

API：

```bash
curl -H "Authorization: Bearer <USER_TOKEN>" \
  "http://127.0.0.1:29876/api/backup/export?scope=user" \
  -o vnt-hub-user-backup.json
```

个人备份不包含 `system_config`，不能作为整库迁移文件使用。

## 生产部署建议

1. 修改 admin 密码。
2. 设置 `VNT_HUB_JWT_SECRET`。
3. 设置 `CONSOLE_MASTER_KEY` 并妥善保管。
4. 使用固定数据库路径，例如 `/var/lib/vnt-hub/vnt-hub.db`。
5. 定期使用 admin 整库导出备份。
6. 迁移文件含敏感信息，应限制访问权限。

示例 systemd 服务：

```ini
[Unit]
Description=VNT Hub
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=vnt-hub
WorkingDirectory=/var/lib/vnt-hub
Environment=VNT_HUB_JWT_SECRET=change-this-secret
Environment=CONSOLE_MASTER_KEY=change-this-master-key
ExecStart=/usr/local/bin/vnt-hub --db /var/lib/vnt-hub/vnt-hub.db --log-path console
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

## 安全提示

- 默认 `admin/admin` 只用于首次启动。
- 不要把整库迁移文件公开上传。
- `CONSOLE_MASTER_KEY` 不在备份文件中，迁移前应确认目标环境配置一致。
- 自签名 TLS 证书保存在数据库内，整库迁移会同步该证书。
- 普通用户备份不应被用于覆盖整库。
