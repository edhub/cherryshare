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
    chmod -R u=rwX,go=rX "$HOME/.local/share/cherryshare/web"
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
    head -c 50000 /dev/urandom >"$SRC/photo.png"   # 名字是 png，内容不可压：该被原样存
    echo hi >"$SRC/sub/a.txt"
    mkdir -p "$SRC/emptydir"                        # 空目录也得留得住
    ln -s "$SRC/sub/a.txt" "$SRC/link"   # 符号链接不该被共享

    # 篮子就是数据目录里的一个文件（ADR-0008）：写一行路径就有了，没有写命令。
    # 这里故意不给 id，看服务会不会自己补一行、而且补在说明后面。
    printf '# e2e\n%s\n' "$SRC" >"$D/t.basket"
    cherry show t
    grep -q '^id [0-9a-f]\{32\}$' "$D/t.basket" || { echo "没补 id ✗" >&2; exit 1; }
    head -1 "$D/t.basket" | grep -q '^# e2e$' || { echo "说明被冲掉了 ✗" >&2; exit 1; }

    WID=$(grep -o '^id [0-9a-f]\{32\}' "$D/t.basket" | cut -d' ' -f2)
    W="http://127.0.0.1:20480"
    # 一个篮子一小段：头上是名字，下面两条链接（名字一条、IP 一条）。
    URLS=$(cherry list | grep -o 'http://[^ ]*')
    LINK=$(echo "$URLS" | sed -n 1p)
    echo "链接 $LINK"
    case "$LINK" in
        "http://"[0-9]*.[0-9]*.[0-9]*.[0-9]*":20480/b/"*)
            echo "`cherry list` 第一条该是本机名那条，不该是 IP ✗: $LINK" >&2; exit 1 ;;
        "http://"*":20480/b/$WID/") ;;
        *) echo "cherry list 打的不是一条能用的网页链接 ✗: $LINK" >&2; exit 1 ;;
    esac
    # 第二条是降级那条：地址段得是个 IP，而不是本机名。
    IP_LINK=$(echo "$URLS" | sed -n 2p)
    case "$IP_LINK" in
        "http://"[0-9]*.[0-9]*.[0-9]*.[0-9]*":20480/b/$WID/") ;;
        *) echo "cherry list 第二条不是 IP 链接 ✗: $IP_LINK" >&2; exit 1 ;;
    esac
    echo "降级那条 $IP_LINK"

    # 挂载命令从**页面上**拿（mounts 里第一条是 macOS）：那正是对方看到的那一条，
    # 也正是这次要验的一条路——命令行里已经不单打一份了。
    # 取第一个 `"command"`：`alt_command` 里 `command` 前头是下划线，匹配不上，
    # 但这也是靠字段顺序（`command` 在 `alt_command` 前面）——换序了得跟着改。
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
    # 权限位照实报：挂载点上看到的就是源文件的权限，对方 `cp` 出来才是正常文件。
    # （0.4.2 之前导出把它报成 0444，`cp -R` 出来的东西全是只读的。）
    [ "$(stat -f %Lp "$MP/share/big.bin")" = "$(stat -f %Lp "$SRC/big.bin")" ] || {
        echo "挂载点上报的权限跟源文件不一样 ✗" >&2
        exit 1
    }
    cp "$MP/share/big.bin" "$D/copied.bin"
    [ "$(stat -f %Lp "$D/copied.bin")" = "$(stat -f %Lp "$SRC/big.bin")" ] || {
        echo "从挂载点拷出来的文件权限不对 ✗" >&2
        exit 1
    }
    # `ro`：客户端那头就挂成只读，写失败说的也是这件事。
    mount | grep "$MP " | grep -q "read-only" || { echo "挂载点不是只读的 ✗" >&2; exit 1; }
    WRITE_ERR=$(touch "$MP/share/x" 2>&1 || true)
    case "$WRITE_ERR" in
        *"Read-only file system"*) ;;
        *) echo "写失败的提示没说是只读 ✗：$WRITE_ERR" >&2; exit 1 ;;
    esac

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
    # 挂载说明里两条地址都给：这条请求走的是 127.0.0.1，所以默认那条该是 IP，另一条该是本机名。
    LJ=$(curl -sf "$W/b/$WID/_api/list")
    echo "$LJ" | grep -q '"host":{"kind":"ip"' || { echo "默认地址该是来访者用的那个 ✗" >&2; exit 1; }
    echo "$LJ" | grep -q '"alt_host":{"kind":"name"' || { echo "没给出能换的那条地址 ✗" >&2; exit 1; }
    # 缓存：页面和文件的名字是固定的、没版本号，所以得能问「变了没」。少了条件请求的
    # 支持，浏览器每次都得重下整个文件；活索引那几个 JSON 则一句都不许缓存。
    curl -sI "$W/b/$WID/_app/app.js" | grep -qi "^cache-control: no-cache" ||
        { echo "页面的缓存指令不对 ✗" >&2; exit 1; }
    LM=$(curl -sI "$W/b/$WID/_app/app.js" | awk 'tolower($1)=="last-modified:"{sub(/\r$/,""); $1=""; sub(/^ /,""); print}')
    code=$(curl -s -o /dev/null -w '%{http_code}' -H "If-Modified-Since: $LM" "$W/b/$WID/_app/app.js")
    [ "$code" = "304" ] || { echo "问「变了没」没得到 304，得到 $code ✗" >&2; exit 1; }
    curl -sI "$W/b/$WID/_api/list" | grep -qi "^cache-control: no-store" ||
        { echo "活索引的 JSON 不该被缓存 ✗" >&2; exit 1; }
    curl -sf "$W/b/$WID/_api/list?path=share" | grep -q 'big.bin' || { echo "进不了子目录 ✗" >&2; exit 1; }
    C=$(curl -sf "$W/b/$WID/share/big.bin" | shasum -a 256 | cut -d' ' -f1)
    [ "$C" = "$A" ] || { echo "网页上取到的字节跟源不一致 ✗" >&2; exit 1; }
    R=$(curl -s -o /dev/null -w '%{http_code} %{size_download}' -r 0-99 "$W/b/$WID/share/big.bin")
    [ "$R" = "206 100" ] || { echo "Range 不对 ✗: $R" >&2; exit 1; }
    for bad in "/" "/b/00000000000000000000000000000000/" "/b/$WID/../../../etc/passwd"; do
        code=$(curl -s -o /dev/null -w '%{http_code}' "$W$bad")
        [ "$code" = "404" ] || { echo "该 404 的返回了 $code ✗: $bad" >&2; exit 1; }
    done

    # 打包下载：把整个子树打成一个 zip，结构、空目录、字节都得对。
    curl -sf "$W/b/$WID/share?dl=zip" -o "$D/pack.zip" || { echo "打包下不下来 ✗" >&2; exit 1; }
    unzip -t "$D/pack.zip" >/dev/null || { echo "打出来的包坏了 ✗" >&2; exit 1; }
    mkdir -p "$D/unpacked"
    ditto -x -k "$D/pack.zip" "$D/unpacked" || { echo "解不开 ✗" >&2; exit 1; }
    diff -r -x link "$SRC" "$D/unpacked/share" >/dev/null || {
        echo "包里的内容跟源不一致 ✗" >&2
        diff -r -x link "$SRC" "$D/unpacked/share" | head -5 >&2
        exit 1
    }
    [ -d "$D/unpacked/share/emptydir" ] || { echo "空目录在包里丢了 ✗" >&2; exit 1; }
    if [ -e "$D/unpacked/share/link" ]; then echo "符号链接混进包里了 ✗" >&2; exit 1; fi
    P=$(shasum -a 256 "$D/unpacked/share/big.bin" | cut -d' ' -f1)
    [ "$P" = "$A" ] || { echo "包里的大文件字节不一致 ✗" >&2; exit 1; }
    # 已经压过的按扩展名原样存（省 CPU），能压的才压。这个判错只会多烧 CPU，不会出错，
    # 所以要有一条看着它的。
    unzip -v "$D/pack.zip" | grep "photo.png" | grep -q "Stored" || { echo "png 没按原始存 ✗" >&2; exit 1; }
    unzip -v "$D/pack.zip" | grep "a.txt" | grep -q "Defl" || { echo "文本没被压 ✗" >&2; exit 1; }
    # 整篮子：篮子根是一排 cherry，没有外套那层。
    curl -sf "$W/b/$WID/?dl=zip" -o "$D/all.zip" || { echo "整篮子打不下来 ✗" >&2; exit 1; }
    unzip -l "$D/all.zip" | grep -q "share/big.bin" || { echo "整篮子里没有那颗 cherry ✗" >&2; exit 1; }
    # 参数写错、或者对文件要打包，都要明说，不能默默给个别的东西。
    code=$(curl -s -o /dev/null -w '%{http_code}' "$W/b/$WID/share?dl=tar")
    [ "$code" = "400" ] || { echo "参数写错该是 400，得到 $code ✗" >&2; exit 1; }
    code=$(curl -s -o /dev/null -w '%{http_code}' "$W/b/$WID/share/big.bin?dl=zip")
    [ "$code" = "400" ] || { echo "对文件打包该是 400，得到 $code ✗" >&2; exit 1; }
    grep -q "打包完了" "$D/log" || { echo "日志里没有打包那条 ✗" >&2; exit 1; }

    # 算大小：只 stat 不读内容，而且有预算。过了一万项就停下说「文件太多」，
    # 而不是接着走完——这台机器是用户自己的，同一台还在给别人做 NFS。
    curl -sf "$W/b/$WID/_api/size?path=share" | grep -q '"bytes":250003' || {
        echo "算出来的大小不对 ✗" >&2
        exit 1
    }
    curl -sf "$W/b/$WID/_api/size?path=share" | grep -q '"stop":"done"' || {
        echo "这么小的树不该提前停 ✗" >&2
        exit 1
    }
    MANY="$BASE/many"
    # 一次 mkdir 建一万多个，比建一万个文件便宜。注意数目得**过**一万：正好一万条是
    # 算得完的，那时报 done 才是对的。
    mkdir -p "$MANY"/{00001..10050}
    printf '%s\n' "$MANY" >>"$D/t.basket"
    curl -sf "$W/b/$WID/_api/size?path=many" | grep -q '"stop":"too_many"' || {
        echo "过了一万项该停下 ✗" >&2
        exit 1
    }
    grep -v "^$MANY$" "$D/t.basket" >"$D/t.tmp" && mv "$D/t.tmp" "$D/t.basket"
    code=$(curl -s -o /dev/null -w '%{http_code}' "$W/b/$WID/_api/size")
    [ "$code" = "400" ] || { echo "算大小不给 path 该是 400，得到 $code ✗" >&2; exit 1; }

    # 源没了：挂载点根目录照常列（不是整层失败）、日志留一条；但文件是你的，工具不替你改
    # （ADR-0008）——那一行还在，`cherry show` 得把它标出来。
    rm -rf "$SRC"
    sleep 6   # 等 actimeo=5 的目录缓存过期
    if ! LEFT=$(ls "$MP" 2>&1); then
        echo "源没了之后挂载点根目录列不出来了 ✗: $LEFT" >&2
        exit 1
    fi
    [ -z "$LEFT" ] || { echo "摘掉之后根目录里还剩东西 ✗: $LEFT" >&2; exit 1; }
    cherry show t | grep -q "源不在了" || { echo "show 没标出源不在了 ✗" >&2; exit 1; }
    grep -qx "$SRC" "$D/t.basket" || { echo "工具擅自改了你的篮子文件 ✗" >&2; exit 1; }
    grep -q "源没了" "$D/log" || { echo "日志里没有那条 ✗" >&2; exit 1; }

    # 删篮子就是删文件：id 跟着没了，对方手里那条链接立刻打不开（这就是吊销）。
    rm "$D/t.basket"
    code=$(curl -s -o /dev/null -w '%{http_code}' "$W/b/$WID/")
    [ "$code" = "404" ] || { echo "删掉篮子之后旧链接还能用 ✗: $code" >&2; exit 1; }
    if cherry show t >/dev/null 2>&1; then echo "篮子还在 ✗" >&2; exit 1; fi

    echo "✓ 端到端通过：内容一致、符号链接没出来、写不进去、源没了之后连坐没有发生、网页那条路通、打包能解开、目录大小算得对且有预算、删篮子之后旧链接打不开"
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
    cargo zigbuild --release --target aarch64-unknown-linux-musl -p cherry -p cherryd
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
    mkdir -p "$DIR/bin/macos-universal" "$DIR/bin/linux-x86_64" "$DIR/bin/linux-aarch64"
    cp "$TMP/cherry" "$TMP/cherryd" "$DIR/bin/macos-universal/"
    cp target/x86_64-unknown-linux-musl/release/cherry target/x86_64-unknown-linux-musl/release/cherryd "$DIR/bin/linux-x86_64/"
    cp target/aarch64-unknown-linux-musl/release/cherry target/aarch64-unknown-linux-musl/release/cherryd "$DIR/bin/linux-aarch64/"
    cp packaging/install.sh "$DIR/install.sh"
    chmod 755 "$DIR/install.sh"
    cp docs/errors.md "$DIR/errors.md"
    mkdir -p "$DIR/web"
    cp -R web/dist/. "$DIR/web/"
    sed -e "s/__VERSION__/$V/g" -e "s/__COMMIT__/$COMMIT/g" -e "s/__DATE__/$(date +%Y-%m-%d)/g" \
        packaging/README.md >"$DIR/README.md"

    (cd "$DIR" &&
        shasum -a 256 bin/macos-universal/* bin/linux-x86_64/* bin/linux-aarch64/* \
            install.sh README.md errors.md $(find web -type f) >SHA256SUMS &&
        shasum -a 256 -c SHA256SUMS >/dev/null &&
        echo "$(wc -l <SHA256SUMS | tr -d ' ') 个文件的校验和都对上了")

    printf '\n包好了：%s\n  commit %s，版本 %s\n\n放进篮子（在数据目录里写一行就分享了）：\n  %s >> ~/.config/cherryshare/<篮子名>.basket\n' "$DIR" "$COMMIT" "$V" "$DIR"
