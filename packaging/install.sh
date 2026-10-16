#!/bin/sh
# CherryShare 的安装脚本（unix）：把 cherry 和 cherryd 放好，并让系统把服务常驻起来。
#
#   bash install.sh              装。重复跑就是更新到这一份
#   bash install.sh uninstall    取消常驻（二进制留着）
#
# 用 `bash install.sh` 调，不要 `./install.sh`：挂载点上的文件不能直接执行（见下）。
#
# macOS 用 LaunchAgent，Linux 用 systemd 的 user unit。两边都只动你自己的家目录，
# 不需要 sudo。这个脚本得跟包里的 bin/ 放在一起（它按自己所在的目录找）。
#
# **macOS 那一支在这台机器上跑过；Linux 那一支是照着 systemd / loginctl 的文档写的，
# 还没在 Linux 上跑过**（手上没有 Linux 机器）。
#
# 写这个脚本时注意：变量后面紧跟中文标点必须写 `${VAR}`。bash 在 UTF-8 locale 里
# 把全角括号当成标识符的合法字符，于是 `$VAR）` 被当成变量名 `VAR）`，在 `set -u` 下
# 当场报 unbound variable。

set -eu

LABEL=com.cherryshare.cherryd
BIN_DIR="$HOME/.local/bin"
WEB_DIR="$HOME/.local/share/cherryshare/web"
if [ -n "${CHERRY_INSTALL_HERE:-}" ]; then
    HERE="$CHERRY_INSTALL_HERE"
else
    HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fi

say() { printf '%s\n' "$*"; }
die() { printf '装不上：%s\n' "$*" >&2; exit 1; }

# 先把自己拷到本地，再从那份跑。
#
# 因为这个脚本经常是从挂载点上被调起来的，而它接下来会把创建者那份服务重启——那一瞬间
# 挂载点就 stale 了，而 bash 是边读边执行的，会当场报 `script file read error: Stale NFS
# file handle`。（二进制在此之前已经拷完了，不受影响。）
if [ -z "${CHERRY_INSTALL_COPY:-}" ]; then
    COPY=$(mktemp "${TMPDIR:-/tmp}/cherry-install.XXXXXX") || die "建不出临时文件"
    cp "$0" "$COPY" || die "把自己拷到 $COPY 失败"
    CHERRY_INSTALL_COPY="$COPY" CHERRY_INSTALL_HERE="$HERE" exec /bin/sh "$COPY" "$@"
fi
trap 'rm -f "$CHERRY_INSTALL_COPY"' EXIT INT TERM

# ---- 这是哪台机器，用包里哪一堆二进制 ----

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
Darwin) BIN_SRC="$HERE/bin/macos-universal" ;;
Linux)
    case "$ARCH" in
    x86_64 | amd64) BIN_SRC="$HERE/bin/linux-x86_64" ;;
    aarch64 | arm64) BIN_SRC="$HERE/bin/linux-aarch64" ;;
    *) die "这份包里的 Linux 二进制有 x86_64 和 aarch64 两种，你这台是 ${ARCH}。" ;;
    esac
    ;;
*) die "$OS 还没做（现在只有 macOS 和 Linux）。" ;;
esac
[ -d "$BIN_SRC" ] ||
    die "找不到 $BIN_SRC —— 这个脚本得跟 bin/ 放在一起（也就是它自己所在的那个文件夹）。"

# ---- 取消常驻 ----

if [ "${1:-}" = uninstall ]; then
    case "$OS" in
    Darwin)
        launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
        rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
        ;;
    Linux)
        systemctl --user disable --now cherryd.service 2>/dev/null || true
        rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/cherryd.service"
        systemctl --user daemon-reload 2>/dev/null || true
        ;;
    esac
    say "取消常驻了。二进制还在 $BIN_DIR 里，要一并删：rm -f $BIN_DIR/cherry $BIN_DIR/cherryd"
    exit 0
fi

# ---- 二进制 ----

mkdir -p "$BIN_DIR"
for f in cherry cherryd; do
    [ -f "$BIN_SRC/$f" ] || die "$BIN_SRC/$f 不见了，这个包不完整。"
    # 先删再拷：0.4.2 及更早的导出把权限位报成只读，那时装下来的文件没有写位，
    # 直接 cp 盖不上去。
    rm -f "$BIN_DIR/$f"
    cp "$BIN_SRC/$f" "$BIN_DIR/$f"
    chmod 755 "$BIN_DIR/$f"
done
say "二进制放好了：$BIN_DIR/cherry、$BIN_DIR/cherryd"

# ---- 网页 ----
#
# 页面是磁盘上的静态文件，不编进二进制（换页面不用重新出包）。守护进程按
# `~/.local/share/cherryshare/web` 找它们，所以这里整目录拷过去。
# 缺了不会静默：起不来页面时日志里有、HTTP 那边也会说一句。
WEB_SRC="$HERE/web"
if [ -d "$WEB_SRC" ]; then
    # 这个包常常是直接从挂载点上装起来的。0.4.2 及更早的导出把权限位报成只读的
    # （文件 0444、目录 0555），`cp -R` 会把那些模式一并带过来，于是本地这份也是只读的、
    # 下一次就删不掉了（0.4.3 起导出改成照实报，但旧安装留下的只读目录还得能收拾）。
    # 所以两件事都做：删之前先把写位加回来，放好之后再按我们自己的模式摆一遍。
    [ -d "$WEB_DIR" ] && chmod -R u+w "$WEB_DIR" 2>/dev/null
    rm -rf "$WEB_DIR" || die "删不掉 $WEB_DIR —— 先 chmod -R u+w 一下再跑"
    mkdir -p "$WEB_DIR"
    cp -R "$WEB_SRC/." "$WEB_DIR/"
    chmod -R u=rwX,go=rX "$WEB_DIR"
    say "网页放好了：$WEB_DIR"
else
    say "包里没有 web/，网页这条路用不了（挂载和命令行不受影响）。"
fi

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*)
    say ""
    say "$BIN_DIR 不在 PATH 里，所以直接敲 cherry 会找不到。往 shell 配置里加一行："
    say "    export PATH=\"\$HOME/.local/bin:\$PATH\""
    ;;
esac

# ---- 腾出端口 ----
# 手动起过的那份会占着 20490。接管的意思就是把它换成由系统管的那一份。

if pgrep -x cherryd >/dev/null 2>&1; then
    OLD=$(pgrep -x cherryd | tr '\n' ' ')
    pkill -x cherryd || true
    i=0
    while pgrep -x cherryd >/dev/null 2>&1 && [ "$i" -lt 20 ]; do
        sleep 0.2
        i=$((i + 1))
    done
    say "把正在跑的 cherryd 停掉了（PID ${OLD%% }），交给系统那份重新起。"
fi

# ---- 常驻 ----

case "$OS" in
Darwin)
    PLIST_DIR="$HOME/Library/LaunchAgents"
    PLIST="$PLIST_DIR/$LABEL.plist"
    LOG_FILE="$HOME/Library/Logs/cherryd.log"
    mkdir -p "$PLIST_DIR" "$HOME/Library/Logs"
    cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>$BIN_DIR/cherryd</string>
	</array>
	<!-- 登录就起；崩了自动重起，正常退出（比如你 kill 的）不重起。 -->
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>$LOG_FILE</string>
	<key>StandardErrorPath</key>
	<string>$LOG_FILE</string>
</dict>
</plist>
EOF
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST" || die "launchctl 没能加载 $PLIST"
    say "由 launchd 管起来了：$PLIST"
    say "  日志：$LOG_FILE"
    ;;
Linux)
    command -v systemctl >/dev/null 2>&1 || die "这台机器没有 systemctl，这个脚本现在只认 systemd。"
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    UNIT="$UNIT_DIR/cherryd.service"
    mkdir -p "$UNIT_DIR"
    cat >"$UNIT" <<EOF
[Unit]
Description=CherryShare 的服务（cherryd）

[Service]
ExecStart=$BIN_DIR/cherryd
# 崩了自动重起，正常退出不重起（跟 macOS 那边 KeepAlive / SuccessfulExit 一个意思）。
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable --now cherryd.service ||
        die "systemctl 没能把 cherryd.service 拉起来（systemctl --user status cherryd 看详情）"
    # 不开 linger 的话，注销之后这个服务就停了。
    if ! loginctl enable-linger "$(id -un)" >/dev/null 2>&1; then
        say "（没能开 linger：注销之后服务会停。要它一直活着：sudo loginctl enable-linger $(id -un)）"
    fi
    say "由 systemd 管起来了：$UNIT"
    say "  日志：journalctl --user -u cherryd -f"
    ;;
esac

# ---- 回头看它到底起没起 ----

sleep 1
PIDS=$(pgrep -x cherryd | tr '\n' ' ')
if [ -n "$PIDS" ]; then
    say "在跑了（PID ${PIDS% }）"
else
    say "没看到 cherryd 起来。它会自己重试几轮；还不行就看上面的日志。"
fi

say ""
say "接着可以："
say "    cherry list                 本机有哪些篮子（每个带名字和 IP 两条链接）"
say "    cherry new 我的照片          建一个篮子"
say "    cherry add 我的照片 <路径>   往里放东西（只是引用，文件不动）"
say "    cherry list                  每个篮子两条链接：本机名一条、本机 IP 一条"
