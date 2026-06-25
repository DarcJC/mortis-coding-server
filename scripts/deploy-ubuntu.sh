#!/usr/bin/env bash
# =============================================================================
# mortis-code-server 一键部署脚本（仅适配 Ubuntu）
#
# 功能：
#   1. 校验系统为 Ubuntu，否则提前报错退出。
#   2. apt 安装系统依赖：subversion(SVN 后端) / git(Git 同步后端 + 拉源码) /
#      python3+venv(supervisor) / build-essential、cmake 等(从源码构建 Rust，
#      aws-lc-rs 需要 C 编译器) / lldb、llvm、binutils(汇编/二进制调试工具链)。
#   3. 增量构建/部署：git 仓库先 `git pull`；仅当有新提交或二进制缺失时才
#      `cargo build --release` 并更新二进制，随后按需经 supervisorctl 重启服务
#      （缺 cargo 自动装 rustup；REPO_ROOT 非 git 仓库时，自动从 --repo-url 克隆）。
#   4. 创建专用系统用户 mortis 与 FHS 目录布局，安装二进制（setcap 授予绑定
#      <1024 特权端口能力，使非 root 的 mortis 可监听 80/443）+ 生成 config.toml
#      （配置已存在则默认保留不动，需重写请加 --reconfigure）。
#   5. 用 pip+venv 安装 supervisor（规避 PEP 668），写好 supervisord/程序配置。
#   6. 配置开机自启：systemd → cron @reboot → 手动 三级回退（兼容无 systemd）。
#
# 用法：
#   sudo ./scripts/deploy-ubuntu.sh                 # 交互式，逐项询问参数
#   sudo ./scripts/deploy-ubuntu.sh --no-prompt     # 静默，用默认值/随机 token
#   sudo ./scripts/deploy-ubuntu.sh --no-prompt --bind 0.0.0.0:9000 --token mysecret
#
# 详见 --help。
# =============================================================================
set -euo pipefail

# ---- 路径定位（兼容 curl | bash 管道执行：此时 BASH_SOURCE 可能为空） --------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# ---- 默认值（可被命令行标志覆盖） -------------------------------------------
BIND_HOST_DEFAULT="0.0.0.0"
BIND_PORT_DEFAULT="8080"
PRINCIPAL_DEFAULT="admin"
SVC_USER_DEFAULT="mortis"
INSTALL_DIR_DEFAULT="/opt/mortis-code-server"
CONFIG_PATH_DEFAULT="/etc/mortis-code-server/config.toml"
DATA_DIR_DEFAULT="/var/lib/mortis-code-server"
LOG_DIR="/var/log/mortis-code-server"
# REPO_ROOT 不是 git 仓库时，从此地址克隆源码再编译（--repo-url 可覆盖）。
REPO_URL_DEFAULT="https://github.com/DarcJC/mortis-coding-server.git"

# supervisor 相关固定路径
SUP_DIR="/etc/supervisor"
SUP_CONF="$SUP_DIR/supervisord.conf"
SUP_PROG_CONF="$SUP_DIR/conf.d/mortis-code-server.conf"
SUP_LOG_DIR="/var/log/supervisor"
SUPERVISORD="/usr/local/bin/supervisord"
SUPERVISORCTL="/usr/local/bin/supervisorctl"

# ---- 运行时变量（命令行解析后填充） -----------------------------------------
PROMPT=1
SKIP_BUILD=0
RECONFIGURE=0
BIND=""
HOST=""
PORT=""
TOKEN=""
TOKEN_RANDOM=0
PRINCIPAL=""
SVC_USER="$SVC_USER_DEFAULT"
INSTALL_DIR="$INSTALL_DIR_DEFAULT"
CONFIG_PATH="$CONFIG_PATH_DEFAULT"
DATA_DIR="$DATA_DIR_DEFAULT"
REPO_URL=""

# 部署过程中由各阶段设置，驱动“有更新才重编/重启”的增量逻辑
SOURCE_UPDATED=0    # git pull 拉到新提交（或全新克隆）时置 1 → 触发重新编译
BINARY_UPDATED=0    # 安装目录中的二进制内容发生变化时置 1 → 需要重启
CONFIG_CHANGED=0    # config.toml 内容发生变化时置 1 → 需要重启
CONFIG_PRESERVE=0   # 已存在 config.toml 且未加 --reconfigure 时置 1 → 跳过配置生成

# ---- 日志辅助 ---------------------------------------------------------------
c_reset=$'\033[0m'; c_blue=$'\033[1;34m'; c_green=$'\033[1;32m'
c_yellow=$'\033[1;33m'; c_red=$'\033[1;31m'
info()  { printf '%s[INFO]%s %s\n'  "$c_blue"   "$c_reset" "$*"; }
ok()    { printf '%s[ OK ]%s %s\n'  "$c_green"  "$c_reset" "$*"; }
warn()  { printf '%s[WARN]%s %s\n'  "$c_yellow" "$c_reset" "$*" >&2; }
die()   { printf '%s[FAIL]%s %s\n'  "$c_red"    "$c_reset" "$*" >&2; trap - EXIT; exit 1; }
trap 'rc=$?; [ $rc -ne 0 ] && printf "%s[FAIL]%s 部署中断 (exit %s)\n" "$c_red" "$c_reset" "$rc" >&2' EXIT

usage() {
  cat <<'USAGE'
mortis-code-server 部署脚本（仅 Ubuntu）

用法: sudo ./scripts/deploy-ubuntu.sh [选项]

模式:
  (默认)              交互式：逐项提示输入监听地址/端口/principal/token
  --no-prompt         静默安装：直接使用默认值与命令行标志，无 token 时自动随机

参数标志:
  --bind <addr:port>  监听地址，等价于同时给 --host 与 --port (默认 0.0.0.0:8080)
  --host <addr>       监听地址 (默认 0.0.0.0)
  --port <port>       监听端口 (默认 8080)
  --token <值|random> 认证 token；填 random 或留空表示随机生成
  --principal <name>  token 对应的 principal (默认 admin)
  --data-dir <path>   服务数据目录 (默认 /var/lib/mortis-code-server)
  --install-dir <path>安装前缀 (默认 /opt/mortis-code-server)
  --config <path>     配置文件路径 (默认 /etc/mortis-code-server/config.toml)
  --user <name>       运行服务的系统用户 (默认 mortis)
  --skip-build        若已存在 target/release 二进制则跳过编译（重复部署提速）
  --reconfigure       已有配置文件时强制重走配置流程并覆盖（默认保留现有配置不动）
  --repo-url <url>    源码非 git 仓库时的克隆地址
                      (默认 https://github.com/DarcJC/mortis-coding-server.git)
  -h, --help          显示本帮助

示例:
  sudo ./scripts/deploy-ubuntu.sh
  sudo ./scripts/deploy-ubuntu.sh --no-prompt
  sudo ./scripts/deploy-ubuntu.sh --no-prompt --bind 0.0.0.0:9000 --principal alice --token s3cr3t
USAGE
}

# ---- 命令行解析（支持 --k v 与 --k=v 两种写法） -----------------------------
parse_args() {
  while [ $# -gt 0 ]; do
    local arg="$1" val=""
    case "$arg" in --*=*) val="${arg#*=}"; arg="${arg%%=*}";; esac
    case "$arg" in
      --no-prompt)  PROMPT=0 ;;
      --skip-build) SKIP_BUILD=1 ;;
      --reconfigure) RECONFIGURE=1 ;;
      -h|--help)    usage; trap - EXIT; exit 0 ;;
      --bind|--host|--port|--token|--principal|--data-dir|--install-dir|--config|--user|--repo-url)
        if [ -z "$val" ]; then
          [ $# -ge 2 ] || die "$arg 需要一个参数值"
          val="$2"; shift
        fi
        case "$arg" in
          --bind)        BIND="$val" ;;
          --host)        HOST="$val" ;;
          --port)        PORT="$val" ;;
          --token)       TOKEN="$val" ;;
          --principal)   PRINCIPAL="$val" ;;
          --data-dir)    DATA_DIR="$val" ;;
          --install-dir) INSTALL_DIR="$val" ;;
          --config)      CONFIG_PATH="$val" ;;
          --user)        SVC_USER="$val" ;;
          --repo-url)    REPO_URL="$val" ;;
        esac ;;
      *) die "未知参数: $1 （--help 查看用法）" ;;
    esac
    shift
  done
}

# ---- 前置检查：仅 Ubuntu + root ---------------------------------------------
preflight() {
  if [ "$(uname -s)" != "Linux" ] || [ ! -r /etc/os-release ]; then
    die "本脚本目前仅支持 Ubuntu 系统。"
  fi
  # shellcheck disable=SC1091
  . /etc/os-release
  if [ "${ID:-}" != "ubuntu" ]; then
    die "检测到系统为 '${PRETTY_NAME:-${ID:-unknown}}'，本脚本目前仅支持 Ubuntu，已退出。"
  fi
  if [ "$(id -u)" -ne 0 ]; then
    die "请用 root 运行，例如：sudo $0 [选项]"
  fi
  info "系统检查通过：${PRETTY_NAME:-Ubuntu}"
}

# ---- apt 安装系统依赖 -------------------------------------------------------
install_system_deps() {
  info "安装系统依赖 (apt)…"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -y
  apt-get install -y --no-install-recommends \
    subversion \
    python3 python3-venv \
    build-essential cmake pkg-config \
    curl git ca-certificates \
    libcap2-bin \
    lldb llvm binutils
  # binutils → GNU objdump/readelf/nm/addr2line；llvm → llvm-objdump/llvm-readobj 等；
  # lldb → LLVM 调试器。供下一阶段的命令执行沙箱调试二进制汇编。
  # libcap2-bin → 提供 setcap，用于授予二进制绑定 <1024 特权端口（如 80/443）的能力。
  # git → Git 同步后端首选系统 git fetch（复用凭据助手/SSH/证书/代理），并用于拉取源码。
  ok "系统依赖安装完成（含 subversion、git、libcap2-bin、lldb/llvm/binutils 汇编调试工具链）"
}

# ---- 以构建用户身份执行命令（保持 target/ 归属正常） ------------------------
BUILD_USER=""
resolve_build_user() {
  BUILD_USER="${SUDO_USER:-}"
  if [ -z "$BUILD_USER" ] || [ "$BUILD_USER" = "root" ]; then
    BUILD_USER="root"
  fi
}
run_as_builder() {
  if [ "$BUILD_USER" = "root" ]; then
    bash -lc "$1"
  else
    sudo -u "$BUILD_USER" -H bash -lc "$1"
  fi
}
# 同上，但用非登录 shell（不加载 profile），便于干净地捕获命令输出（如 git rev-parse）
run_as_builder_q() {
  if [ "$BUILD_USER" = "root" ]; then
    bash -c "$1"
  else
    sudo -u "$BUILD_USER" -H bash -c "$1"
  fi
}

# ---- 拉取 git 更新：HEAD 变化（拉到新提交）则置 SOURCE_UPDATED=1 ------------
pull_repo() {
  local dir="$1" before after
  before="$(run_as_builder_q "git -C '$dir' rev-parse HEAD 2>/dev/null" || true)"
  info "检查 git 更新：$dir"
  if run_as_builder "git -C '$dir' pull --ff-only"; then
    after="$(run_as_builder_q "git -C '$dir' rev-parse HEAD 2>/dev/null" || true)"
    if [ -n "$after" ] && [ "$before" != "$after" ]; then
      SOURCE_UPDATED=1
      ok "拉取到更新：${before:0:12} → ${after:0:12}（将重新编译）"
    else
      info "源码已是最新，无需更新。"
    fi
  else
    warn "git pull 失败（可能有本地改动/网络问题/非快进），沿用现有源码。"
  fi
}

# ---- 源码就绪：git 仓库则拉取更新；非 git 仓库则克隆（可能重定向 REPO_ROOT） --
ensure_source() {
  if [ -e "$REPO_ROOT/.git" ]; then
    info "构建源码：$REPO_ROOT（git 仓库，就地构建）"
    pull_repo "$REPO_ROOT"
    return
  fi

  local url="${REPO_URL:-$REPO_URL_DEFAULT}"
  info "REPO_ROOT 不是 git 仓库（$REPO_ROOT）→ 从 $url 克隆源码"

  local target
  if [ ! -e "$REPO_ROOT" ] || [ -z "$(ls -A "$REPO_ROOT" 2>/dev/null)" ]; then
    # REPO_ROOT 为空/不存在：直接克隆进 REPO_ROOT
    target="$REPO_ROOT"
    install -d -o "$BUILD_USER" -g "$BUILD_USER" "$target"
  else
    # REPO_ROOT 非空（通常含本脚本）：克隆到构建用户家目录，避免覆盖正在执行的脚本
    local home; home="$(getent passwd "$BUILD_USER" | cut -d: -f6)" || home=""
    [ -n "$home" ] || home="/root"
    target="$home/mortis-code-server-src"
    warn "REPO_ROOT 非空且非 git 仓库，为避免覆盖正在运行的脚本，改克隆到：$target"
  fi

  if [ -e "$target/.git" ]; then
    info "复用已存在的克隆：$target"
    REPO_ROOT="$target"
    pull_repo "$target"
  else
    if [ "$target" != "$REPO_ROOT" ] && [ -e "$target" ] && [ -n "$(ls -A "$target" 2>/dev/null)" ]; then
      die "克隆目标已存在且非空：$target，请清理后重试（或用 --repo-url 指定其它源）。"
    fi
    run_as_builder "git clone --depth 1 '$url' '$target'"
    SOURCE_UPDATED=1                  # 全新克隆 → 视为有更新，需编译
    REPO_ROOT="$target"
  fi

  [ -f "$REPO_ROOT/Cargo.toml" ] || die "克隆完成但未找到 Cargo.toml：$REPO_ROOT（仓库地址/分支是否正确？）"
  ok "源码已就绪：$REPO_ROOT"
}

# ---- 从源码构建（仅在源码有更新或二进制缺失时编译） -------------------------
build_binary() {
  if [ "$SKIP_BUILD" -eq 1 ]; then
    local bin="$REPO_ROOT/target/release/mortis-code-server"
    [ -x "$bin" ] || die "--skip-build 已指定，但未找到 $bin，请去掉该标志以从源码构建。"
    info "已跳过构建，复用现有二进制：$bin"
    return
  fi

  resolve_build_user
  ensure_source                       # 拉取/克隆源码并探测更新；可能重定向 REPO_ROOT
  local bin="$REPO_ROOT/target/release/mortis-code-server"

  if [ "$SOURCE_UPDATED" -eq 0 ] && [ -x "$bin" ]; then
    info "源码无更新且已存在二进制，跳过编译：$bin"
    return
  fi
  if [ "$SOURCE_UPDATED" -eq 1 ]; then
    info "检测到源码更新 → 以用户 '$BUILD_USER' 重新编译"
  else
    info "未找到现有二进制 → 以用户 '$BUILD_USER' 首次编译"
  fi

  if ! run_as_builder '. "$HOME/.cargo/env" 2>/dev/null || true; command -v cargo >/dev/null'; then
    info "未检测到 cargo，正在为 '$BUILD_USER' 安装 rustup (stable)…"
    run_as_builder 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable'
    ok "rustup 安装完成"
  fi

  info "开始 cargo build --release（首次编译可能需要数分钟）…"
  run_as_builder '. "$HOME/.cargo/env" 2>/dev/null || true; cd '"'$REPO_ROOT'"' && cargo build --release'
  [ -x "$bin" ] || die "构建结束但未找到二进制：$bin"
  ok "构建完成：$bin"
}

# ---- 创建专用用户与目录 -----------------------------------------------------
setup_user_and_dirs() {
  if ! id "$SVC_USER" >/dev/null 2>&1; then
    info "创建系统用户 '$SVC_USER'…"
    useradd --system --create-home --home-dir "$DATA_DIR" \
            --shell /usr/sbin/nologin "$SVC_USER"
  else
    info "系统用户 '$SVC_USER' 已存在"
  fi

  install -d -m 0755 "$INSTALL_DIR/bin"
  install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$(dirname "$CONFIG_PATH")"
  install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$DATA_DIR"
  install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$LOG_DIR"
  install -d -m 0755 "$SUP_DIR/conf.d"
  install -d -m 0755 "$SUP_LOG_DIR"
  ok "目录布局就绪"
}

# ---- 安装二进制（仅当内容变化或目标缺失时覆盖，并标记需要重启） -------------
install_binary() {
  local src="$REPO_ROOT/target/release/mortis-code-server"
  local dst="$INSTALL_DIR/bin/mortis-code-server"
  if [ -x "$dst" ] && cmp -s "$src" "$dst"; then
    info "二进制无变化，跳过更新：$dst"
    return
  fi
  install -m 0755 "$src" "$dst"
  BINARY_UPDATED=1
  ok "已安装二进制到 $dst"
}

# ---- 授予绑定特权端口的能力（让非 root 的 mortis 也能监听 80/443 等 <1024 端口） ----
# 服务以系统用户 "$SVC_USER" 运行，默认无权绑定 <1024 端口。setcap 把
# CAP_NET_BIND_SERVICE 写入二进制文件的扩展属性，使其 exec 时即获得该能力。
# 注意：install 复制会生成新 inode、清除扩展属性，故必须在 install_binary 之后、
# 且每次重新部署都重新授予。
grant_net_bind_capability() {
  local bin="$INSTALL_DIR/bin/mortis-code-server"
  if ! command -v setcap >/dev/null 2>&1; then
    warn "未找到 setcap（请安装 libcap2-bin），无法授予绑定特权端口能力。"
    [ "$PORT" -lt 1024 ] && die "端口 $PORT < 1024，非 root 用户 '$SVC_USER' 需要 setcap 才能绑定；请安装 libcap2-bin 后重试。"
    return
  fi
  if setcap 'cap_net_bind_service=+ep' "$bin"; then
    ok "已授予 cap_net_bind_service：'$SVC_USER' 运行的二进制可绑定 <1024 特权端口（如 80/443）"
  else
    warn "setcap 失败：$bin（文件系统可能不支持扩展属性，如部分容器/网络盘）。"
    [ "$PORT" -lt 1024 ] && die "端口 $PORT < 1024 且 setcap 失败，服务将无法以非 root 用户绑定。"
  fi
}

# ---- token 生成 -------------------------------------------------------------
gen_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
  elif [ -r /dev/urandom ]; then
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
  else
    python3 -c 'import secrets;print(secrets.token_hex(32))'
  fi
}

# ---- 交互式提示辅助（提示走 /dev/tty，结果走 stdout） -----------------------
ask() {
  local q="$1" def="$2" ans
  printf '%s [%s]: ' "$q" "$def" >/dev/tty
  read -r ans </dev/tty || ans=""
  printf '%s' "${ans:-$def}"
}
ask_yes_no() {
  local q="$1" def="${2:-y}" ans hint="[Y/n]"
  [ "$def" = "n" ] && hint="[y/N]"
  printf '%s %s: ' "$q" "$hint" >/dev/tty
  read -r ans </dev/tty || ans=""
  ans="${ans:-$def}"
  case "$ans" in [Yy]*) return 0 ;; *) return 1 ;; esac
}
ask_secret() {
  local q="$1" ans
  printf '%s: ' "$q" >/dev/tty
  read -rs ans </dev/tty || ans=""
  printf '\n' >/dev/tty
  printf '%s' "$ans"
}

# ---- 从已有配置解析展示用参数（保留模式：不交互/不生成，仍在总结里显示真实值） ----
# 仅针对本脚本自己生成的 config.toml 格式做轻量解析；解析失败的字段留空，由调用方兜底。
parse_existing_config() {
  local f="$CONFIG_PATH" bind line t p
  [ -r "$f" ] || return 0

  # [server] 段的 bind = "host:port"
  bind="$(sed -n 's/^[[:space:]]*bind[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' "$f" | head -n1)"
  if [ -n "$bind" ]; then
    HOST="${bind%:*}"; PORT="${bind##*:}"
  fi

  # [auth] 段第一条 { token = "...", principal = "..." }
  line="$(grep -E '^[[:space:]]*\{[[:space:]]*token[[:space:]]*=' "$f" | head -n1)"
  if [ -n "$line" ]; then
    t="$(printf '%s' "$line" | sed -n 's/.*token[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*,[[:space:]]*principal.*/\1/p')"
    p="$(printf '%s' "$line" | sed -n 's/.*principal[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*}.*/\1/p')"
    # TOML 基本字符串反转义：先 \" → "，再 \\ → \
    t="${t//\\\"/\"}"; t="${t//\\\\/\\}"
    p="${p//\\\"/\"}"; p="${p//\\\\/\\}"
    [ -n "$t" ] && TOKEN="$t"
    [ -n "$p" ] && PRINCIPAL="$p"
  fi
}

# ---- 解析最终参数（交互或静默） ---------------------------------------------
resolve_params() {
  # 已存在配置且未要求重新配置 → 保留模式：不交互、不生成，改从现有配置解析展示值
  if [ -f "$CONFIG_PATH" ] && [ "$RECONFIGURE" -eq 0 ]; then
    CONFIG_PRESERVE=1
    info "检测到已有配置，保留不动：$CONFIG_PATH（如需重新配置并覆盖，请加 --reconfigure）"
    parse_existing_config
    # 解析失败时兜底，确保后续 setcap 端口校验与收尾总结不报错
    HOST="${HOST:-$BIND_HOST_DEFAULT}"
    PRINCIPAL="${PRINCIPAL:-$PRINCIPAL_DEFAULT}"
    case "$PORT" in
      *[!0-9]*|'') warn "未能从现有配置解析出有效端口，端口相关校验改用默认值 $BIND_PORT_DEFAULT。"
                   PORT="$BIND_PORT_DEFAULT" ;;
    esac
    return
  fi

  # --bind 优先拆分为 host/port
  if [ -n "$BIND" ]; then
    HOST="${BIND%:*}"; PORT="${BIND##*:}"
  fi

  if [ "$PROMPT" -eq 1 ] && [ ! -t 0 ] && [ ! -r /dev/tty ]; then
    warn "当前非交互式终端，自动切换为静默模式（--no-prompt）。"
    PROMPT=0
  fi

  if [ "$PROMPT" -eq 1 ]; then
    info "交互式配置（直接回车使用方括号内默认值）"
    HOST="$(ask '监听地址 (0.0.0.0=所有网卡, 127.0.0.1=仅本机)' "${HOST:-$BIND_HOST_DEFAULT}")"
    PORT="$(ask '监听端口' "${PORT:-$BIND_PORT_DEFAULT}")"
    PRINCIPAL="$(ask 'principal 名称（token 归属）' "${PRINCIPAL:-$PRINCIPAL_DEFAULT}")"
    if [ -z "$TOKEN" ] || [ "$TOKEN" = "random" ]; then
      if ask_yes_no '是否随机生成认证 token?' y; then
        TOKEN="$(gen_token)"; TOKEN_RANDOM=1
      else
        TOKEN="$(ask_secret '请输入认证 token')"
        if [ -z "$TOKEN" ]; then
          warn "未输入 token，改为随机生成。"
          TOKEN="$(gen_token)"; TOKEN_RANDOM=1
        fi
      fi
    fi
  else
    HOST="${HOST:-$BIND_HOST_DEFAULT}"
    PORT="${PORT:-$BIND_PORT_DEFAULT}"
    PRINCIPAL="${PRINCIPAL:-$PRINCIPAL_DEFAULT}"
    if [ -z "$TOKEN" ] || [ "$TOKEN" = "random" ]; then
      TOKEN="$(gen_token)"; TOKEN_RANDOM=1
    fi
  fi

  [ -n "$HOST" ] || die "监听地址不能为空"
  case "$PORT" in *[!0-9]*|'') die "监听端口必须为数字：'$PORT'" ;; esac
}

# ---- 生成 config.toml -------------------------------------------------------
write_config() {
  if [ "$CONFIG_PRESERVE" -eq 1 ]; then
    info "保留现有配置，跳过生成：$CONFIG_PATH"
    return
  fi
  # TOML 字符串转义：反斜杠与双引号
  local tok="$TOKEN" prin="$PRINCIPAL"
  tok="${tok//\\/\\\\}"; tok="${tok//\"/\\\"}"
  prin="${prin//\\/\\\\}"; prin="${prin//\"/\\\"}"

  cat > "$CONFIG_PATH.new" <<EOF
# mortis-code-server 配置（由 scripts/deploy-ubuntu.sh 生成）
# 环境变量覆盖使用 MORTIS_ 前缀、__ 表示嵌套，例如 MORTIS_SERVER__BIND=0.0.0.0:9000

[server]
bind = "$HOST:$PORT"
data_dir = "$DATA_DIR"
# svn_bin = "/usr/bin/svn"   # 默认走系统 svn（已 apt 安装 subversion）

[auth]
tokens = [
  { token = "$tok", principal = "$prin" },
]

[asm]                          # 汇编查询会话（二进制下载 + 反汇编）
allowed_hosts = []             # 下载主机白名单；默认拒绝（空 = 全部拒绝）
max_download_bytes = 2684354560 # 单次下载上限（2560 MiB）
download_timeout = "60s"       # 单次下载超时
session_ttl = "1h"             # 汇编会话空闲存活时间（按 reap_interval 回收）
max_sessions = 16              # 并发汇编会话上限

[session]
ttl = "24h"
reap_interval = "10m"

# ---- 仓库（按需取消注释并填写；可留空，服务也能正常运行） -------------------
# [[repo]]
# id = "mortis-code-server"
# kind = "git"
# url = "https://github.com/DarcJC/mortis-coding-server.git"
# rev = "master"
# schedule = "30m"
# include = ["**/*"]
# exclude = []
EOF

  chown "$SVC_USER:$SVC_USER" "$CONFIG_PATH.new"
  chmod 0640 "$CONFIG_PATH.new"

  if [ -f "$CONFIG_PATH" ] && cmp -s "$CONFIG_PATH.new" "$CONFIG_PATH"; then
    rm -f "$CONFIG_PATH.new"
    info "配置无变化：$CONFIG_PATH"
  else
    mv -f "$CONFIG_PATH.new" "$CONFIG_PATH"
    CONFIG_CHANGED=1
    ok "已生成配置：$CONFIG_PATH"
  fi
}

# ---- 安装 supervisor（pip + venv，规避 PEP 668） ----------------------------
install_supervisor() {
  local venv="$INSTALL_DIR/supervisor-venv"
  if [ ! -x "$venv/bin/supervisord" ]; then
    info "创建 supervisor 虚拟环境并安装…"
    python3 -m venv "$venv"
    "$venv/bin/pip" install --quiet --upgrade pip
    "$venv/bin/pip" install --quiet supervisor
  else
    info "supervisor 虚拟环境已存在，跳过安装"
  fi
  ln -sf "$venv/bin/supervisord"  "$SUPERVISORD"
  ln -sf "$venv/bin/supervisorctl" "$SUPERVISORCTL"
  ok "supervisor 安装完成：$("$SUPERVISORD" --version 2>/dev/null || echo '?')"
}

# 解析“服务实际使用的”data_dir：优先取（可能被 --preserve 保留的）config.toml 中的值，
# 回退到部署变量 $DATA_DIR。supervisor 的 directory=（进程 CWD）/HOME= 必须与之一致，
# 否则改了 config.toml 的 data_dir 后，进程会被 chdir 到一个已不存在的旧目录，
# svn 在启动 getcwd 处报 E125001、gix 在 is_git 的 getcwd 处报“not a git repository”。
effective_data_dir() {
  local v=""
  if [ -f "$CONFIG_PATH" ]; then
    v="$(grep -E '^[[:space:]]*data_dir[[:space:]]*=' "$CONFIG_PATH" 2>/dev/null \
          | head -n1 | sed -E 's/.*=[[:space:]]*"(.*)"[[:space:]]*$/\1/')"
  fi
  [ -n "$v" ] && printf '%s' "$v" || printf '%s' "$DATA_DIR"
}

# ---- 写 supervisord 主配置 + 程序配置 ---------------------------------------
write_supervisor_conf() {
  # supervisor 必须以服务真正使用的 data_dir 作为 directory=/HOME=（见 effective_data_dir）。
  local eff_data_dir; eff_data_dir="$(effective_data_dir)"
  if [ "$eff_data_dir" != "$DATA_DIR" ]; then
    warn "config.toml 的 data_dir（$eff_data_dir）与 --data-dir（$DATA_DIR）不一致；"
    warn "supervisor 的 directory=/HOME= 以 config.toml 为准（$eff_data_dir）。"
  fi
  # directory= 必须在进程启动前就存在，否则 supervisor chdir 失败、程序无法拉起。
  install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$eff_data_dir" || true
  cat > "$SUP_CONF" <<'EOF'
; supervisord 主配置（由 mortis-code-server 部署脚本生成）
[unix_http_server]
file=/run/supervisor.sock
chmod=0700

[supervisord]
logfile=/var/log/supervisor/supervisord.log
logfile_maxbytes=10MB
logfile_backups=5
pidfile=/run/supervisord.pid
childlogdir=/var/log/supervisor
nodaemon=false

[rpcinterface:supervisor]
supervisor.rpcinterface_factory = supervisor.rpcinterface:make_main_rpcinterface

[supervisorctl]
serverurl=unix:///run/supervisor.sock

[include]
files = /etc/supervisor/conf.d/*.conf
EOF

  cat > "$SUP_PROG_CONF" <<EOF
[program:mortis-code-server]
command=$INSTALL_DIR/bin/mortis-code-server $CONFIG_PATH
directory=$eff_data_dir
user=$SVC_USER
autostart=true
autorestart=true
startsecs=5
startretries=3
stopsignal=TERM
stopwaitsecs=15
; LANG/LC_ALL=C.UTF-8：强制进程树使用 glibc 内置的 UTF-8 locale（无需 locale-gen），
; 避免宿主机未生成的 locale（如 en_US.UTF-8）导致 svn 回退到 ASCII、在中文文件名上
; 报 E000022 而中断导出。svn 子进程在代码层也会再强制一次，此处为纵深防御并惠及 git。
environment=LANG="C.UTF-8",LC_ALL="C.UTF-8",RUST_LOG="info",HOME="$eff_data_dir",MORTIS_SERVER__LOG_FILE="$LOG_DIR/app.log",MORTIS_SERVER__LOG_LEVEL="info,mortis_vcs=debug"
stdout_logfile=$LOG_DIR/stdout.log
stderr_logfile=$LOG_DIR/stderr.log
stdout_logfile_maxbytes=10MB
stdout_logfile_backups=5
stderr_logfile_maxbytes=10MB
stderr_logfile_backups=5
EOF
  # 便于裸 supervisorctl / supervisord 命令自动找到主配置
  ln -sf "$SUP_CONF" /etc/supervisord.conf
  ok "已写入 supervisor 配置：$SUP_PROG_CONF"
}

supervisor_is_up() { "$SUPERVISORCTL" -c "$SUP_CONF" pid >/dev/null 2>&1; }

wait_supervisor() {
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    supervisor_is_up && return 0
    sleep 0.5
  done
  return 1
}

reload_program() {
  "$SUPERVISORCTL" -c "$SUP_CONF" reread || true
  "$SUPERVISORCTL" -c "$SUP_CONF" update || true
  if [ "${NEED_RESTART:-0}" -eq 1 ]; then
    info "二进制或配置有更新，通过 supervisorctl 重启服务…"
    "$SUPERVISORCTL" -c "$SUP_CONF" restart mortis-code-server || true
  else
    "$SUPERVISORCTL" -c "$SUP_CONF" start mortis-code-server >/dev/null 2>&1 || true
    info "二进制与配置均无更新，未重启（已确保服务在运行）。"
  fi
}

# ---- 启动 supervisord 并配置开机自启（三级回退） ----------------------------
BOOT_MODE=""
start_and_enable() {
  # 仅当二进制或配置发生变化时才需要重启服务
  NEED_RESTART=0
  [ "${BINARY_UPDATED:-0}" -eq 1 ] && NEED_RESTART=1
  [ "${CONFIG_CHANGED:-0}" -eq 1 ] && NEED_RESTART=1

  if [ -d /run/systemd/system ]; then
    BOOT_MODE="systemd"
    info "检测到 systemd，写入 supervisord.service 并设为开机自启…"
    local unit=/etc/systemd/system/supervisord.service unit_changed=0
    cat > "$unit.new" <<EOF
[Unit]
Description=Supervisor process control system (mortis-code-server)
After=network.target

[Service]
Type=simple
ExecStart=$SUPERVISORD -n -c $SUP_CONF
ExecStop=$SUPERVISORCTL -c $SUP_CONF shutdown
ExecReload=$SUPERVISORCTL -c $SUP_CONF reload
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
    if [ -f "$unit" ] && cmp -s "$unit.new" "$unit"; then
      rm -f "$unit.new"
    else
      mv -f "$unit.new" "$unit"; unit_changed=1
    fi
    systemctl daemon-reload
    systemctl enable supervisord >/dev/null 2>&1 || true
    if ! systemctl is-active --quiet supervisord; then
      info "启动 supervisord…"
      systemctl start supervisord
    elif [ "$unit_changed" -eq 1 ]; then
      info "supervisord 单元已更新，重启 supervisord…"
      systemctl restart supervisord
    else
      info "supervisord 已在运行，无需重启（仅按需重载服务）。"
    fi
    wait_supervisor || die "supervisord 启动失败，请查看 $SUP_LOG_DIR/supervisord.log"
    reload_program

  elif command -v crontab >/dev/null 2>&1; then
    BOOT_MODE="cron"
    info "未检测到 systemd，改用 cron @reboot 实现开机自启…"
    local line="@reboot $SUPERVISORD -c $SUP_CONF"
    local current; current="$(crontab -l 2>/dev/null || true)"
    if ! printf '%s\n' "$current" | grep -Fq "$line"; then
      { [ -n "$current" ] && printf '%s\n' "$current"; printf '%s\n' "$line"; } | crontab -
    fi
    if supervisor_is_up; then reload_program; else "$SUPERVISORD" -c "$SUP_CONF"; fi
    wait_supervisor || die "supervisord 启动失败，请查看 $SUP_LOG_DIR/supervisord.log"

  else
    BOOT_MODE="manual"
    warn "未检测到 systemd 或 cron：无法配置开机自启。"
    if supervisor_is_up; then reload_program; else "$SUPERVISORD" -c "$SUP_CONF"; fi
    wait_supervisor || die "supervisord 启动失败，请查看 $SUP_LOG_DIR/supervisord.log"
  fi
}

# ---- 收尾信息 ---------------------------------------------------------------
print_summary() {
  local exposed_note=""
  [ "$HOST" = "0.0.0.0" ] && exposed_note="  ${c_yellow}（监听所有网卡，请确保防火墙与 token 已妥善配置）${c_reset}"
  printf '\n%s========== 部署完成 ==========%s\n' "$c_green" "$c_reset"
  if [ "${BINARY_UPDATED:-0}" -eq 1 ] || [ "${CONFIG_CHANGED:-0}" -eq 1 ]; then
    printf '本次执行 : 检测到更新，已重新部署并重启服务\n'
  else
    printf '本次执行 : 无更新，服务保持原样运行\n'
  fi
  printf '监听地址 : http://%s:%s%s\n' "$HOST" "$PORT" "$exposed_note"
  printf '          REST → /api/v1    MCP → /mcp\n'
  printf 'principal: %s\n' "$PRINCIPAL"
  if [ -n "$TOKEN" ]; then
    printf 'token    : %s\n' "$TOKEN"
    [ "$TOKEN_RANDOM" -eq 1 ] && printf '           （随机生成，已写入配置；请妥善保存）\n'
    [ "$CONFIG_PRESERVE" -eq 1 ] && printf '           （读取自现有配置）\n'
    printf '调用示例 : curl -H "Authorization: Bearer %s" http://%s:%s/api/v1/repos\n' "$TOKEN" "$HOST" "$PORT"
  else
    printf 'token    : （未能从现有配置解析，请查看 %s）\n' "$CONFIG_PATH"
    printf '调用示例 : curl -H "Authorization: Bearer <token>" http://%s:%s/api/v1/repos\n' "$HOST" "$PORT"
  fi
  printf '\n路径:\n'
  printf '  二进制   %s/bin/mortis-code-server\n' "$INSTALL_DIR"
  if [ "$CONFIG_PRESERVE" -eq 1 ]; then
    printf '  配置     %s （已保留，未改动；加 --reconfigure 可重新生成）\n' "$CONFIG_PATH"
  else
    printf '  配置     %s\n' "$CONFIG_PATH"
  fi
  printf '  数据     %s\n' "$DATA_DIR"
  printf '  日志     %s/app.log （应用主日志，已开启 mortis_vcs=debug 诊断）\n' "$LOG_DIR"
  printf '           %s/{stdout,stderr}.log （supervisor 捕获的标准输出/错误）\n' "$LOG_DIR"
  printf '  运行用户 %s\n' "$SVC_USER"
  printf '\n开机自启 : %s' "$BOOT_MODE"
  case "$BOOT_MODE" in
    systemd) printf '（systemctl status supervisord）\n' ;;
    cron)    printf '（root crontab @reboot）\n' ;;
    manual)  printf '\n'
             printf '  %s未配置开机自启%s：请把下面命令加入容器 entrypoint 或启动脚本：\n' "$c_yellow" "$c_reset"
             printf '    %s -c %s\n' "$SUPERVISORD" "$SUP_CONF" ;;
  esac
  printf '\n常用命令:\n'
  printf '  supervisorctl status\n'
  printf '  supervisorctl restart mortis-code-server\n'
  printf '  tail -f %s/app.log              # 应用日志（含 VCS 同步诊断）\n' "$LOG_DIR"
  printf '  supervisorctl tail -f mortis-code-server stderr\n'
  printf '\n修改配置后重载：编辑 %s 然后 supervisorctl restart mortis-code-server\n' "$CONFIG_PATH"
}

# ---- 主流程 -----------------------------------------------------------------
main() {
  parse_args "$@"
  preflight
  resolve_params          # 先把参数问清楚，随后构建/安装全程无人值守
  install_system_deps
  setup_user_and_dirs
  build_binary
  install_binary
  grant_net_bind_capability
  write_config
  install_supervisor
  write_supervisor_conf
  start_and_enable
  print_summary
  trap - EXIT
}

main "$@"
