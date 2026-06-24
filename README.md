# mortis-code-server

把多个 **Git / SVN 代码仓库**封装成一个服务：同时对外提供 **REST/JSON API** 和 **HTTP Streamable MCP**（Model Context Protocol），面向人和 AI Agent。提供代码搜索、范围读取、blame、提交历史，以及**会话级 copy-on-write 写入层**（写入/删除不触碰原始仓库，可生成 status/diff/patch）。

纯 Rust（Git 后端零 C 工具链），**同时兼容 Windows 与 Linux**，单进程、单二进制。

---

## 目录

- [特性](#特性)
- [架构总览](#架构总览)
- [构建与安装](#构建与安装)
- [配置](#配置)
- [运行](#运行)
- [部署（Ubuntu）](#部署ubuntu)
- [REST API 参考](#rest-api-参考)
- [MCP 接入](#mcp-接入)
- [会话语义（CoW）](#会话语义cow)
- [内嵌 SVN 二进制](#内嵌-svn-二进制)
- [跨平台说明](#跨平台说明)
- [故障排查](#故障排查)
- [限制与 Roadmap](#限制与-roadmap)

---

## 特性

- **双协议等价**：REST 与 MCP 调用同一套服务层，能力一一对应、零重复逻辑。
- **多仓库自动拉取 + 定时更新**：支持 Git 与 SVN；cron 或间隔（`"15m"`）调度。
- **白名单筛选拉取**：每仓库 `include`/`exclude` glob，仅物化匹配的文件夹/文件。
- **代码搜索**：内嵌 ripgrep 库（`grep-*`，无需外部 `rg`），支持正则/字面量、大小写模式、子树/glob 作用域、上下文行、结果上限。
- **范围读取**：按行（或字节）范围或整文件读取，带总行数/截断/二进制标记。
- **blame 与提交历史**：基于原始仓库（Git 走对象库，SVN 走 URL）。
- **会话级 CoW 写入层**：写/删落到会话 upper 层，提供 `status`/`diff`/`export_patch`（git 风格、可 `git apply`），**绝不修改原始仓库**；会话持久化、按 owner 隔离、TTL 自动回收。
- **鉴权**：Bearer Token；一个 principal 可拥有多个会话，会话私有。
- **自包含**：Git 纯 Rust（rustls 传输）；SVN 通过可内嵌的 `svn` 二进制，缺失时回退系统 `svn`。

---

## 架构总览

分层 Cargo workspace，用 crate 边界强制依赖方向（领域核心不依赖任何框架）。

```
                      ┌─────────────────────────────────────────────┐
   HTTP client ─────► │  mortis-server  (axum)                       │
   (REST / MCP)       │  ┌─────────────┐   ┌──────────────────────┐  │
                      │  │ rest (REST) │   │ mcp (Streamable MCP)  │  │  ← 薄适配器
                      │  └──────┬──────┘   └───────────┬──────────┘  │
                      │     Bearer auth middleware (tower)           │
                      └─────────┼───────────────────────┼───────────┘
                                ▼                        ▼
                      ┌─────────────────────────────────────────────┐
                      │  mortis-app   ·  Services（Facade + DI）      │
                      │  RepoRegistry  +  Arc<dyn 端口>               │
                      └───┬──────────────┬──────────────┬───────────┘
                          ▼              ▼              ▼
                ┌──────────────┐ ┌──────────────┐ ┌──────────────────┐
                │ mortis-vcs   │ │ mortis-search│ │ mortis-session   │
                │ Git(gix)/SVN │ │ grep 引擎    │ │ CoW overlay 存储 │
                └──────┬───────┘ └──────┬───────┘ └────────┬─────────┘
                       │                ▼                  │
                       │         ┌──────────────┐          │
                       └────────►│  mortis-fs   │◄─────────┘
                                 │ FileView 实现 │
                                 └──────┬───────┘
                                        ▼
                                ┌──────────────┐   ┌──────────────┐
                                │ mortis-core  │   │ mortis-embed │
                                │ 端口/类型/错误│   │ 内嵌 svn 释放 │
                                └──────────────┘   └──────────────┘
```

### crate 职责

| crate | 职责 |
|---|---|
| `mortis-core` | 领域契约：trait（`VcsBackend`/`SearchEngine`/`SessionStore`/`FileView`）、值类型、统一 `CoreError`。无框架依赖。 |
| `mortis-fs` | 具体 `FileView`：`PhysicalFileView`（只读目录）、`OverlayFileView`（CoW 联合视图）。逻辑路径统一正斜杠。 |
| `mortis-vcs` | VCS 后端：`GixBackend`（纯 Rust Git）、`SvnCliBackend`（驱动 svn CLI）。白名单物化。 |
| `mortis-embed` | 内嵌并释放各平台 `svn` 二进制；解析顺序：override → 内嵌 → 系统 PATH。 |
| `mortis-search` | 内嵌 ripgrep 搜索引擎，作用于任意 `FileView`。 |
| `mortis-session` | 磁盘 CoW 会话存储：write/delete、status、`similar` diff/patch、持久化、TTL 回收。 |
| `mortis-app` | 应用服务层 `Services`（Facade）：编排端口、`RepoRegistry`、owner 鉴权。 |
| `mortis-server` | 表现层 + 装配：axum REST + Streamable MCP、Bearer 鉴权、调度器、`main`。 |

### 设计模式

- **Strategy**：`VcsBackend` 抽象，`GixBackend`/`SvnCliBackend` 可换；搜索/会话同理。
- **Adapter**：gix / svn-cli / grep / similar 被包装到领域 trait 后。
- **Facade**：`Services` 是表现层唯一入口。
- **Repository**：`SessionStore` / `RepoRegistry`。
- **Dependency Injection**：服务持有 `Arc<dyn Trait>`，由 `mortis-server` 装配；`mortis-app` 不依赖任何具体后端。
- **Ports & Adapters（六边形）**：core 定义端口，infra crate 实现，server 注入。

### 数据流示例（会话内搜索）

```
Client ──Bearer──► axum ──auth mw（注入 Principal）──► rest/mcp adapter
   └─► Services.search_session(principal, session, query)
        ├─ 校验 owner，构造 OverlayFileView（base ⊕ upper ⊖ whiteout）
        └─ GrepSearchEngine.search(view, query)   // spawn_blocking
   ◄── JSON 结果
```

---

## 构建与安装

### 前置

- **Rust** ≥ 1.85（edition 2024）。`rustup` 安装即可。
- **Git 后端**：无需系统 git、无需 C 工具链（纯 Rust + rustls）。
  - 注：默认 TLS 走 `reqwest` + `rustls`，其加密后端 `aws-lc-rs` 在构建时需要 C 编译器（多数平台有预编译，Windows 需要 MSVC 构建工具）。如需完全免 C，可改用 ring/纯 Rust provider（见 [限制](#限制与-roadmap)）。
- **SVN 后端**：可选。运行期需要一个 `svn` 可执行文件——内嵌（见[内嵌 SVN](#内嵌-svn-二进制)）或系统安装（Linux `apt install subversion`；Windows 安装 SlikSVN/TortoiseSVN 命令行）。无 SVN 仓库则无需 svn。
- **测试**：集成测试需要系统 `git`（构造夹具）；SVN 测试需要 `svn`/`svnadmin`（缺失则自动跳过）。

### 构建

```bash
cargo build --release          # 产物: target/release/mortis-code-server
cargo test  --workspace        # 全部单测 + 集成测试
```

Windows（PowerShell）相同命令即可。

---

## 配置

启动参数为配置文件路径（默认 `config.toml`）。支持 `MORTIS_` 前缀的环境变量覆盖，嵌套用 `__`，例如 `MORTIS_SERVER__BIND=0.0.0.0:9000`。完整示例见 [`config.example.toml`](config.example.toml)。

```toml
[server]
bind = "127.0.0.1:8080"        # 监听地址；对外服务用 0.0.0.0:8080
data_dir = "./data"            # 物化仓库 / 会话 / 缓存根目录
# svn_bin = "/usr/bin/svn"     # 可选：强制指定 svn 可执行文件

[auth]
# 每个请求都需 Authorization: Bearer <token>；token 映射到 principal。
tokens = [
  { token = "change-me", principal = "alice" },
]

[session]
ttl = "24h"                    # 空闲会话存活时间
reap_interval = "10m"          # 回收器运行间隔

[[repo]]
id = "proj-a"                  # 唯一 id，同时是磁盘目录名
kind = "git"                   # git | svn
url = "https://example.com/a.git"
rev = "main"                   # Git: 分支/标签/提交；SVN: 修订号/HEAD；省略则用默认 head
schedule = "15m"              # 6 段 cron 或人类间隔（"15m"/"1h"）；省略则不自动更新
include = ["src/**", "*.md"]  # 白名单 glob；空 = 物化全部
exclude = ["**/*.bin"]        # 在 include 之后应用
# username / password         # 认证仓库可选
```

### 字段语义要点

- **白名单 glob**：基于仓库相对路径（正斜杠），如 `src/**`、`**/*.rs`。`include` 为空表示全部物化；`exclude` 在其后过滤。
- **schedule**：能被解析为时长（`humantime`）即按间隔重复，否则按 6 段 cron。
- **数据目录布局**：`data/repos/<id>/work`（只读物化工作树）、`data/repos/<id>/vcs`（后端内部存储）、`data/sessions/<sid>/`（会话 upper + meta.json）、`data/cache/`（释放的 svn 二进制）。

---

## 运行

```bash
cp config.example.toml config.toml   # 编辑 tokens / repos
RUST_LOG=info ./target/release/mortis-code-server config.toml
# mortis-code-server listening on http://127.0.0.1:8080 (REST: /api/v1, MCP: /mcp)
```

启动时会后台触发一次全量同步，并按各仓库 `schedule` 注册定时同步与会话 TTL 回收。

健康检查（无需鉴权）：

```bash
curl http://127.0.0.1:8080/health      # -> ok
```

> 以下示例假设 `TOKEN=change-me`、服务在 `127.0.0.1:8080`。

---

## 部署（Ubuntu）

`scripts/deploy-ubuntu.sh` 把服务一键部署为 **supervisord 守护进程**（仅适配 Ubuntu，其它系统会提前报错退出）。它会依次：apt 安装依赖（`subversion` 用于 SVN 后端、`python3`+`venv` 供 supervisor、`build-essential`/`cmake` 构建链，以及 `lldb`/`llvm`/`binutils` 汇编调试工具）、**从源码 `cargo build --release`**（缺 `cargo` 时自动装 rustup；**运行目录若不是 git 仓库，自动从 [`DarcJC/mortis-coding-server`](https://github.com/DarcJC/mortis-coding-server) 克隆源码**再编译）、创建专用系统用户 `mortis` 与 FHS 目录、生成 `config.toml`、用 pip+venv 安装 supervisor 并配置开机自启。脚本可**重复运行（幂等）**：覆盖配置并 `reread/update/restart`。

### 一键部署（curl | bash）

无需事先克隆仓库——脚本检测到当前目录非 git 仓库时会自动拉取源码：

```bash
# 交互式（在终端逐项询问监听地址 / 端口 / principal / token）
curl -fsSL https://raw.githubusercontent.com/DarcJC/mortis-coding-server/main/scripts/deploy-ubuntu.sh | sudo bash

# 静默：默认值 + 自动随机 token
curl -fsSL https://raw.githubusercontent.com/DarcJC/mortis-coding-server/main/scripts/deploy-ubuntu.sh | sudo bash -s -- --no-prompt

# 静默 + 指定参数
curl -fsSL https://raw.githubusercontent.com/DarcJC/mortis-coding-server/main/scripts/deploy-ubuntu.sh \
  | sudo bash -s -- --no-prompt --bind 0.0.0.0:9000 --principal alice --token s3cr3t
```

> 用管道把脚本喂给 `sudo bash` 前，建议先过目脚本内容。此方式下源码默认克隆到构建用户家目录的 `mortis-code-server-src/`，可用 `--repo-url` 指定其它源。

### 从已克隆的仓库部署

```bash
# 交互式：逐项询问监听地址 / 端口 / principal / token（默认随机）
sudo ./scripts/deploy-ubuntu.sh

# 静默：用默认值 + 自动随机 token
sudo ./scripts/deploy-ubuntu.sh --no-prompt

# 静默 + 指定参数
sudo ./scripts/deploy-ubuntu.sh --no-prompt --bind 0.0.0.0:9000 --principal alice --token s3cr3t
```

### 命令行参数

| 参数 | 说明（默认值） |
|---|---|
| `--no-prompt` | 静默安装，不交互；无 `--token` 时自动随机 |
| `--bind <addr:port>` | 监听地址（`0.0.0.0:8080`），等价于同时给 `--host`+`--port` |
| `--host` / `--port` | 分别指定监听地址 / 端口 |
| `--token <值\|random>` | 认证 token；填 `random` 或留空表示随机生成 |
| `--principal <name>` | token 归属（`admin`） |
| `--data-dir <path>` | 数据目录（`/var/lib/mortis-code-server`） |
| `--install-dir <path>` | 安装前缀（`/opt/mortis-code-server`） |
| `--config <path>` | 配置路径（`/etc/mortis-code-server/config.toml`） |
| `--user <name>` | 运行服务的系统用户（`mortis`） |
| `--skip-build` | 复用已有 `target/release` 二进制，跳过编译 |
| `--repo-url <url>` | 源码非 git 仓库时的克隆地址（默认 `…/DarcJC/mortis-coding-server.git`） |
| `-h, --help` | 显示帮助 |

### 安装布局

| 路径 | 内容 |
|---|---|
| `/opt/mortis-code-server/bin/mortis-code-server` | 二进制 |
| `/etc/mortis-code-server/config.toml` | 配置（`0640`，属 `mortis`） |
| `/var/lib/mortis-code-server` | 数据目录（`data_dir`） |
| `/var/log/mortis-code-server/{stdout,stderr}.log` | 运行日志 |
| `/etc/supervisor/conf.d/mortis-code-server.conf` | supervisor 程序配置 |

### 开机自启（兼容无 systemd）

按运行环境三级回退：有 **systemd** → 写 `supervisord.service` 并 `enable`；否则有 **cron** → 写 root 的 `@reboot` 启动项；都没有（纯容器）→ 立即启动并提示把 `supervisord -c /etc/supervisor/supervisord.conf` 加进容器 entrypoint。

### 运维命令

```bash
supervisorctl status                          # 查看状态
supervisorctl restart mortis-code-server      # 改完 config.toml 后重启
supervisorctl tail -f mortis-code-server stderr
```

> `lldb` / `llvm`（`llvm-objdump` 等）/ `binutils`（`objdump`/`readelf`）由脚本一并预装，供下一阶段「命令执行沙箱」调试二进制汇编。

---

## REST API 参考

所有 `/api/v1/*` 端点都需 `Authorization: Bearer <token>`。错误返回 `{"error":"<code>","message":"..."}`，HTTP 状态码映射：`not_found→404`、`invalid_input→400`、`unauthorized→401`、`forbidden→403`、`conflict→409`、其余 `500`。

| 能力 | 方法与路径 |
|---|---|
| 列仓库/状态 | `GET /api/v1/repos` |
| 触发同步 | `POST /api/v1/repos/{id}/sync` |
| 代码搜索 | `POST /api/v1/search` |
| 读文件（范围） | `GET /api/v1/repos/{id}/file?path=&start=&end=&rev=` |
| blame | `GET /api/v1/repos/{id}/blame?path=&rev=` |
| 提交历史 | `GET /api/v1/repos/{id}/history?path=&limit=&skip=` |
| 建会话 | `POST /api/v1/sessions`  body `{"repo":"<id>"}` |
| 列/查/删会话 | `GET /api/v1/sessions` · `GET/DELETE /api/v1/sessions/{sid}` |
| 会话写文件 | `PUT /api/v1/sessions/{sid}/file?path=`（请求体=文件字节） |
| 会话删文件 | `DELETE /api/v1/sessions/{sid}/file?path=` |
| 会话读文件 | `GET /api/v1/sessions/{sid}/file?path=&start=&end=` |
| 会话 status | `GET /api/v1/sessions/{sid}/status` |
| 会话 diff（text/plain） | `GET /api/v1/sessions/{sid}/diff?path=` |
| 导出 patch（text/plain） | `GET /api/v1/sessions/{sid}/patch` |

### 示例

```bash
# 列仓库
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/repos

# 搜索（不带 repo 则搜全部；带 session 则搜该会话 overlay）
curl -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"pattern":"fn ","regex":false,"repo":"proj-a","max_results":50}' \
  http://127.0.0.1:8080/api/v1/search
# -> [{"repo":"proj-a","path":"src/lib.rs","line_no":1,"line":"pub fn greet() {}","submatches":[[7,9]]}]

# 读文件第 1 行
curl -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:8080/api/v1/repos/proj-a/file?path=src/lib.rs&start=1&end=1"

# blame / history
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:8080/api/v1/repos/proj-a/blame?path=src/lib.rs"
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:8080/api/v1/repos/proj-a/history?limit=10"

# 会话：建 -> 写 -> status -> diff -> patch
SID=$(curl -s -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"repo":"proj-a"}' http://127.0.0.1:8080/api/v1/sessions | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
curl -X PUT -H "Authorization: Bearer $TOKEN" --data-binary @new.rs \
  "http://127.0.0.1:8080/api/v1/sessions/$SID/file?path=src/lib.rs"
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:8080/api/v1/sessions/$SID/status"
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:8080/api/v1/sessions/$SID/patch"   # 可 git apply
```

搜索请求体即 `SearchQuery` 字段：`pattern`(必填)、`regex`、`case`(`smart|sensitive|insensitive`)、`max_results`、`context_before`、`context_after`、`subtree`、`globs`，外加可选 `repo` / `session`。

---

## MCP 接入

MCP 端点：`POST /mcp`，**Streamable HTTP（无状态 JSON 模式）**——每个请求直接返回 `application/json` 结果，无需 MCP 会话握手或 SSE 通道。客户端必须：

- 发送 `Authorization: Bearer <token>`；
- `Accept: application/json, text/event-stream`（规范要求二者皆列）；
- 标准 HTTP 客户端会自带 `Host` 头（rmcp 会校验）。

> 应用级"会话"（CoW）不是 MCP 协议会话——它通过工具参数 `session_id` 显式传递，符合 MCP 的 explicit-handles 模式。`principal` 从 Bearer Token 推导（无需作为参数）。

### 工具清单（与 REST 对应）

`list_repos` · `sync_repo` · `search_code` · `read_file` · `blame_file` · `get_history` · `create_session` · `list_sessions` · `delete_session` · `write_file` · `delete_file` · `session_status` · `session_diff` · `export_patch`

每个工具返回 JSON 文本内容（`result.content[0].text` 为序列化后的 JSON 字符串）。

### 交互示例（curl）

```bash
H=(-H "Authorization: Bearer $TOKEN" -H "Accept: application/json, text/event-stream" -H "Content-Type: application/json")

# initialize
curl -s "${H[@]}" -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}' http://127.0.0.1:8080/mcp

# tools/list
curl -s "${H[@]}" -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' http://127.0.0.1:8080/mcp

# tools/call: 在某仓库搜索
curl -s "${H[@]}" -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_code","arguments":{"pattern":"fn ","repo":"proj-a"}}}' http://127.0.0.1:8080/mcp
```

### 接入 MCP 客户端 / Inspector

任意支持 **Streamable HTTP** 传输的 MCP 客户端，配置 URL `http://<host>:<port>/mcp` 并附带 `Authorization: Bearer <token>` 头即可。例如 MCP Inspector：

```bash
npx @modelcontextprotocol/inspector
# Transport: Streamable HTTP；URL: http://127.0.0.1:8080/mcp；
# Header: Authorization = Bearer <token>
```

---

## 会话语义（CoW）

会话是某仓库只读工作树之上的、按 principal 私有的 copy-on-write overlay：

- **写**（`write_file`）：内容写入 `data/sessions/<sid>/upper/<path>`；若该路径此前被删除则取消删除。
- **删**（`delete_file`）：从 upper 移除该文件；若 base 存在则记为 whiteout。**base 始终只读，绝不修改。**
- **读 / 搜索**：通过 `OverlayFileView` 解析：whiteout→不存在；upper 命中→upper；否则 base。
- **status**：相对 base 的变更集（`added`/`modified`/`deleted`，仿 `git status`；逐字节比较，跳过无效写）。
- **diff / export_patch**：用 `similar` 生成统一 diff，带 `diff --git` 与 `/dev/null`（新增/删除），**可被 `git apply` 应用**。
- **持久化**：upper + `meta.json` 落盘，进程重启后会话仍在。
- **TTL 回收**：后台回收器按 `last_accessed + ttl` 删除空闲会话。
- **隔离**：会话归属创建它的 principal；他人列举不可见、访问返回 `403`。
- **路径安全**：拒绝绝对路径与 `..`，写入不会逃逸 upper 目录。

---

## 内嵌 SVN 二进制

为让 SVN 支持自包含，可把各平台 `svn` 发行集放到 `crates/mortis-embed/assets/svn/<os>-<arch>/`，它们会在编译期被嵌入二进制（`rust-embed`），首次使用时释放到 `data/cache/svn-<tag>/` 并运行。

**解析顺序**：`[server].svn_bin` 覆盖 → 内嵌（当前平台）→ 系统 `PATH`。三者皆无且存在 SVN 仓库时，启动会以清晰的配置错误失败。

源码仅提交占位文件（不含大体积二进制），因此默认行为是**回退系统 `svn`**，构建不被大文件阻塞。要做到完全自包含，用脚本填充资产后重建：

```bash
# Windows：从 SlikSVN zip 提取，或打包系统 svn
pwsh scripts/fetch-svn.ps1 -Url https://.../sliksvn.zip
pwsh scripts/fetch-svn.ps1 -FromSystem

# Linux：打包系统 svn 及其 ldd 依赖（best-effort 可重定位）
bash scripts/fetch-svn.sh
```

布局要求：Windows 为 `windows-x86_64/svn.exe` + 同目录 DLL（运行时该目录加入 `PATH`）；Linux 为 `linux-x86_64/bin/svn` + `linux-x86_64/lib/*.so`（运行时 `lib` 加入 `LD_LIBRARY_PATH`）。

**许可**：Apache Subversion 以 Apache-2.0 分发；再分发其二进制请随附相应 NOTICE/LICENSE。

---

## 跨平台说明

- 逻辑路径在 `mortis-fs` 与各后端统一为正斜杠，REST/MCP 响应在两平台一致。
- 路径用 `camino::Utf8Path`，缓存/数据目录用 `directories`。
- Git 走 gix 纯 Rust，两平台一致，无需系统 git。
- 内嵌 svn 释放时按平台设置可执行位与动态库搜索路径。
- 读取/diff 处理 CRLF/LF 与非 UTF-8（`grep-searcher` 编码探测；非 UTF-8 以有损方式解码）。

---

## 故障排查

| 现象 | 处理 |
|---|---|
| 所有请求 401 | 缺少/错误 Bearer Token；检查 `[auth].tokens`。 |
| MCP 报 "Not Acceptable" | `Accept` 必须同时包含 `application/json` 与 `text/event-stream`。 |
| MCP 报 "Invalid Host header" | 使用标准 HTTP 客户端（自带 `Host`）；勿发送空 Host。 |
| SVN 仓库报 config 错误 | 未找到 svn：安装系统 svn、设置 `[server].svn_bin`，或填充内嵌资产。 |
| 同步失败 | 看日志（`RUST_LOG=info`/`debug`）；确认 URL、`rev`、凭据与网络。 |
| 默认分支解析不到 | 显式设置 `[[repo]].rev`（如 `"main"`/`"master"`）。 |

---

## 限制与 Roadmap

- **纯 Rust SVN**：2026 年仍不可用，故 SVN 走 CLI 包装（内嵌或系统）。
- **gix blame**：功能可用、读侧可接受，性能略逊于 C git；必要时可引入 `git2` 兜底。
- **TLS 加密后端**：当前 `aws-lc-rs` 构建期需 C 编译器；Roadmap 提供 `ring`/纯 Rust provider 的可选 feature 以彻底免 C 工具链。
- **Linux 内嵌 svn 可重定位性**：依赖 APR/serf/openssl，best-effort 打包；脆弱时建议回退系统 `svn`。
- **写入内容**：MCP `write_file` 目前接受 UTF-8 文本；二进制写入可经 REST `PUT`（原始字节）。
- Roadmap：每仓库 SSH 凭据/已知主机配置、搜索结果流式分页、可选 `git2` blame、Prometheus 指标。

---

## 许可

MIT OR Apache-2.0（见各 crate `Cargo.toml`）。再分发内嵌的 Subversion 二进制须遵循 Apache-2.0 并随附其 NOTICE。
