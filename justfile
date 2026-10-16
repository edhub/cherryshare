# CherryShare 的常用任务。`just` 列出来，`just <名字>` 跑。
#
#   just check                  提交之前跑：格式、lint、测试
#   just install                从源码编一份装到本机，并让 launchd 重启服务
#   just e2e                    端到端跑一遍：临时数据目录起服务、挂上、比对、卸掉
#   just release 0.2.0          出一个版本：编两份、组包、算校验和
#
# `release` 只认干净的工作区，而且要求 `Cargo.toml` 里的版本就是你要发的那个——这样包里的
# commit 号和版本号不可能是假的。它只在 macOS 上跑得通（要 lipo、codesign、zig）。
# 包里的说明从 `packaging/README.md` 渲染（`__VERSION__` / `__COMMIT__` / `__DATE__`）。

set shell := ["bash", "-euo", "pipefail", "-c"]

# 包放哪儿。跟 install.sh 无关——那是对方那边的事。
dist := env_var_or_default("CHERRY_DIST", home_directory() / "Public")

default:
    @just --list

# 提交之前跑这个
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets
    cargo test --workspace

# 从源码编一份装到本机（~/.local/bin），并让服务重启到这一份。
# 包里的那份走 `bash packaging/install.sh`，不是这条。
install: web
    cargo build --release --workspace
    mkdir -p "$HOME/.local/bin"
    cp target/release/cherry target/release/cherryd "$HOME/.local/bin/"
    rm -rf "$HOME/.local/share/cherryshare/web"
    mkdir -p "$HOME/.local/share/cherryshare/web"
    cp -R {{justfile_directory()}}/web/dist/. "$HOME/.local/share/cherryshare/web/"
    if launchctl print "gui/$(id -u)/com.cherryshare.cherryd" >/dev/null 2>&1; then launchctl kickstart -k "gui/$(id -u)/com.cherryshare.cherryd"; echo "服务已重启到这一份"; elif [ -f "$HOME/Library/LaunchAgents/com.cherryshare.cherryd.plist" ]; then launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.cherryshare.cherryd.plist" && echo "服务之前没在跑（大概是为了跑 e2e 停掉的），重新拉起来了"; else echo "服务没装着（跑 bash packaging/install.sh 装一个）"; fi

# 编网页的静态文件（web/dist/）。产物不编进二进制，装的时候拷过去。
web:
    "{{justfile_directory()}}/web/build.sh"

# 端到端跑一遍（全程用临时目录，最后自己收拾干净）
e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    # 端口那条检查只看 LISTEN：浏览器留下的 CLOSE_WAIT 连接也算「占用」的话会白报一次。
    # 停服务是 `launchctl bootout gui/$(id -u)/com.cherryshare.cherryd`；跑完用
    # `just install`（它会 bootstrap 回来）或 `launchctl bootstrap …` 起回去。
    # 二进制用绝对路径：下面有个子 shell 要 cd 进别处去试相对路径。
    CHERRY="{{justfile_directory()}}/target/release/cherry"
    CHERRYD="{{justfile_directory()}}/target/release/cherryd"
    WEB="{{justfile_directory()}}/web/dist"
    # 每次都编一遍：不然测的可能是个陈年 release 二进制（cargo 自己会判断要不要真编）。
    (cd "{{justfile_directory()}}" && cargo build --release --workspace)
    for port in 20490 20480; do
        # 只看 LISTEN：浏览器留下的 CLOSE_WAIT 连接也算「占用」的话，会白报一次。
        if lsof -nP -iTCP:$port -sTCP:LISTEN >/dev/null 2>&1; then
            echo "$port 被占了：先停掉本机的服务（launchctl bootout gui/$(id -u)/com.cherryshare.cherryd）" >&2
            exit 1
        fi
    done
    [ -f "$WEB/index.html" ] || { echo "网页还没编：先跑 `just web`" >&2; exit 1; }

    D=$(mktemp -d)      # 服务的数据目录
    BASE=$(mktemp -d)   # 要分享的东西（下面在它里面建个定名子目录：cherry 的名字取自目录名）
    SRC="$BASE/share"
    mkdir -p "$SRC/sub"
    CHERRYSHARE_DATA_DIR="$D" CHERRYSHARE_WEB_DIR="$WEB" "$CHERRYD" >"$D/log" 2>&1 &
    SVC=$!
    MP=""
    trap 'umount "$MP" 2>/dev/null || true; kill "$SVC" 2>/dev/null || true; rm -rf "$D" "$BASE" "$MP"' EXIT

    for _ in $(seq 50); do [ -S "$D/cherryd.sock" ] && break; sleep 0.1; done
    cherry() { CHERRYSHARE_DATA_DIR="$D" "$CHERRY" "$@"; }

    head -c 200000 /dev/urandom >"$SRC/big.bin"
    echo hi >"$SRC/sub/a.txt"
    ln -s "$SRC/sub/a.txt" "$SRC/link"   # 符号链接不该被共享

    cherry new t >/dev/null
    # 特意用相对路径加：客户端得按自己的 cwd 解析（服务那头的 cwd 是 /）。
    (cd "$BASE" && cherry add t ./share) >/dev/null
    cherry show t

    WID=$(cherry show t | head -1 | sed 's/.*\[\(.*\)\]/\1/')
    W="http://127.0.0.1:20480"
    LINK=$(cherry list | awk '{print $2}')
    echo "链接 $LINK"
    case "$LINK" in
        "http://"*":20480/b/$WID/") ;;
        *) echo "cherry list 打的不是一条能用的网页链接 ✗: $LINK" >&2; exit 1 ;;
    esac

    # 挂载命令从**页面上**拿（mounts 里第一条是 macOS）：那正是对方看到的那一条，
    # 也正是这次要验的一条路——命令行里已经不单打一份了。
    CMD=$(curl -sf "$W/b/$WID/_api/list" | grep -o '"command":"[^"]*"' | head -1 | sed 's/^"command":"//; s/"$//')
    eval "$CMD"
    MP=$(eval echo "${CMD##* }")        # 展开那行里的 ~
    echo "挂在 $MP"

    diff -r -x link "$SRC" "$MP/share" >/dev/null || {
        echo "内容和源不一致 ✗" >&2
        diff -r -x link "$SRC" "$MP/share" | head -5 >&2
        exit 1
    }
    if [ -e "$MP/share/link" ]; then echo "符号链接泄漏出来了 ✗" >&2; exit 1; fi
    A=$(shasum -a 256 "$SRC/big.bin" | cut -d' ' -f1)
    B=$(shasum -a 256 "$MP/share/big.bin" | cut -d' ' -f1)
    [ "$A" = "$B" ] || { echo "字节不一致 ✗" >&2; exit 1; }
    if touch "$MP/share/x" 2>/dev/null; then echo "居然能写 ✗" >&2; exit 1; fi

    # 网页那条路：不挂载也能看和拿。页面、样式、JSON、文件、Range、越界都得对。
    # （得放在删源之前——篮子空了之后这些就无从验起了。）
    # 注意这里只验接口，页面里的 JS（在线看、大小、缓存那些）是手验的，别以为这条盖住了。
    curl -sf "$W/b/$WID/" >/dev/null || { echo "页面打不开 ✗" >&2; exit 1; }
    curl -sf "$W/b/$WID/_app/app.css" >/dev/null || { echo "页面的样式拿不到 ✗" >&2; exit 1; }
    curl -sf "$W/b/$WID/_api/list" | grep -q '"name"' || { echo "列表 JSON 不对 ✗" >&2; exit 1; }
    curl -sf "$W/b/$WID/_api/list?path=share" | grep -q 'big.bin' || { echo "进不了子目录 ✗" >&2; exit 1; }
    C=$(curl -sf "$W/b/$WID/share/big.bin" | shasum -a 256 | cut -d' ' -f1)
    [ "$C" = "$A" ] || { echo "网页上取到的字节跟源不一致 ✗" >&2; exit 1; }
    R=$(curl -s -o /dev/null -w '%{http_code} %{size_download}' -r 0-99 "$W/b/$WID/share/big.bin")
    [ "$R" = "206 100" ] || { echo "Range 不对 ✗: $R" >&2; exit 1; }
    for bad in "/" "/b/00000000000000000000000000000000/" "/b/$WID/../../../etc/passwd"; do
        code=$(curl -s -o /dev/null -w '%{http_code}' "$W$bad")
        [ "$code" = "404" ] || { echo "该 404 的返回了 $code ✗: $bad" >&2; exit 1; }
    done

    # 源没了：服务把这颗从篮子里摘掉，挂载点根目录照常列（不是整层失败），日志留一条。
    rm -rf "$SRC"
    sleep 6   # 等 actimeo=5 的目录缓存过期
    if ! LEFT=$(ls "$MP" 2>&1); then
        echo "源没了之后挂载点根目录列不出来了 ✗: $LEFT" >&2
        exit 1
    fi
    [ -z "$LEFT" ] || { echo "摘掉之后根目录里还剩东西 ✗: $LEFT" >&2; exit 1; }
    if cherry show t | grep -q share; then echo "篮子里还留着那颗 ✗" >&2; exit 1; fi
    grep -q "源没了" "$D/log" || { echo "日志里没有那条 ✗" >&2; exit 1; }

    echo "✓ 端到端通过：内容一致、符号链接没出来、写不进去、源没了之后连坐没有发生、网页那条路通"
# 出一个版本。参数必须跟 Cargo.toml 里的版本一致。
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    V="{{version}}"
    DIST="{{dist}}"
    DIR="$DIST/cherryshare-$V"

    if [ -z "${CHERRY_RELEASE_ALLOW_DIRTY:-}" ] && [ -n "$(git status --porcelain)" ]; then
        echo "工作区不干净——包里的 commit 号会跟实际内容对不上。先提交；真要硬来就 CHERRY_RELEASE_ALLOW_DIRTY=1。" >&2
        exit 1
    fi
    HAVE=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
    [ "$HAVE" = "$V" ] || { echo "Cargo.toml 里写的是 $HAVE，不是 $V" >&2; exit 1; }
    COMMIT=$(git rev-parse --short HEAD)

    cargo build --release --workspace
    cargo build --release --target x86_64-apple-darwin -p cherry -p cherryd
    cargo zigbuild --release --target x86_64-unknown-linux-musl -p cherry -p cherryd
    # 网页：静态文件，跟包一起发，但不编进二进制。
    web/build.sh >/dev/null

    # lipo 会把链接器给的签名弄没，所以合成之后要重签。
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    for b in cherry cherryd; do
        lipo -create -output "$TMP/$b" "target/release/$b" "target/x86_64-apple-darwin/release/$b"
        codesign -s - -f "$TMP/$b"
        codesign -v "$TMP/$b"
    done

    rm -rf "$DIR"
    mkdir -p "$DIR/bin/macos-universal" "$DIR/bin/linux-x86_64"
    cp "$TMP/cherry" "$TMP/cherryd" "$DIR/bin/macos-universal/"
    cp target/x86_64-unknown-linux-musl/release/cherry target/x86_64-unknown-linux-musl/release/cherryd "$DIR/bin/linux-x86_64/"
    cp packaging/install.sh "$DIR/install.sh"
    chmod 755 "$DIR/install.sh"
    cp docs/errors.md "$DIR/errors.md"
    mkdir -p "$DIR/web"
    cp -R web/dist/. "$DIR/web/"
    sed -e "s/__VERSION__/$V/g" -e "s/__COMMIT__/$COMMIT/g" -e "s/__DATE__/$(date +%Y-%m-%d)/g" \
        packaging/README.md >"$DIR/README.md"

    (cd "$DIR" &&
        shasum -a 256 bin/macos-universal/* bin/linux-x86_64/* install.sh README.md errors.md $(find web -type f) >SHA256SUMS &&
        shasum -a 256 -c SHA256SUMS >/dev/null &&
        echo "$(wc -l <SHA256SUMS | tr -d ' ') 个文件的校验和都对上了")

    printf '\n包好了：%s\n  commit %s，版本 %s\n\n放进篮子：\n  cherry add <篮子名> %s\n' "$DIR" "$COMMIT" "$V" "$DIR"
