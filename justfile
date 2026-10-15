# CherryShare 的常用任务。`just` 列出来，`just <名字>` 跑。
#
#   just check                  提交之前跑：格式、lint、测试
#   just dev-install            编好装到本机（~/.local/bin），并让 launchd 重启服务
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

# 编好装到本机，并让服务重启到这一份（开发用）
dev-install:
    cargo build --release --workspace
    mkdir -p "$HOME/.local/bin"
    cp target/release/cherry target/release/cherryd "$HOME/.local/bin/"
    if launchctl print "gui/$(id -u)/com.cherryshare.cherryd" >/dev/null 2>&1; then launchctl kickstart -k "gui/$(id -u)/com.cherryshare.cherryd"; echo "服务已重启到这一份"; else echo "服务没装着（跑 bash packaging/install.sh 装一个）"; fi

# 端到端跑一遍（全程用临时目录，最后自己收拾干净）
e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    # 20490 得空着——本机服务在跑就先停掉（launchctl bootout gui/$(id -u)/com.cherryshare.cherryd）。
    # 二进制用绝对路径：下面有个子 shell 要 cd 进别处去试相对路径。
    CHERRY="{{justfile_directory()}}/target/release/cherry"
    CHERRYD="{{justfile_directory()}}/target/release/cherryd"
    # 每次都编一遍：不然测的可能是个陈年 release 二进制（cargo 自己会判断要不要真编）。
    (cd "{{justfile_directory()}}" && cargo build --release --workspace)
    if lsof -nP -iTCP:20490 >/dev/null 2>&1; then
        echo "20490 被占了：先停掉本机的服务（launchctl bootout gui/$(id -u)/com.cherryshare.cherryd）" >&2
        exit 1
    fi

    D=$(mktemp -d)      # 服务的数据目录
    BASE=$(mktemp -d)   # 要分享的东西（下面在它里面建个定名子目录：cherry 的名字取自目录名）
    SRC="$BASE/share"
    mkdir -p "$SRC/sub"
    CHERRYSHARE_DATA_DIR="$D" "$CHERRYD" >"$D/log" 2>&1 &
    SVC=$!
    MP=""
    trap 'umount "$MP" 2>/dev/null || true; kill "$SVC" 2>/dev/null || true; rm -rf "$D" "$BASE" "$MP"' EXIT

    for _ in $(seq 50); do [ -S "$D/cherryd.sock" ] && break; sleep 0.1; done
    cherry() { "$CHERRY" --data-dir "$D" "$@"; }

    head -c 200000 /dev/urandom >"$SRC/big.bin"
    echo hi >"$SRC/sub/a.txt"
    ln -s "$SRC/sub/a.txt" "$SRC/link"   # 符号链接不该被共享

    cherry new t >/dev/null
    # 特意用相对路径加：客户端得按自己的 cwd 解析（服务那头的 cwd 是 /）。
    (cd "$BASE" && cherry add t ./share) >/dev/null
    cherry show t
    LINK=$(cherry link t | head -1)
    echo "链接 $LINK"

    # 就按它打出来的那行命令挂（把 "macOS" 那一段抠出来执行）
    CMD=$(cherry link t | grep -A1 '^  macOS' | tail -1 | sed 's/^ *//')
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

    echo "✓ 端到端通过：内容一致、符号链接没出来、写不进去、源没了之后连坐没有发生"

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
    sed -e "s/__VERSION__/$V/g" -e "s/__COMMIT__/$COMMIT/g" -e "s/__DATE__/$(date +%Y-%m-%d)/g" \
        packaging/README.md >"$DIR/README.md"

    (cd "$DIR" &&
        shasum -a 256 bin/macos-universal/* bin/linux-x86_64/* install.sh README.md errors.md >SHA256SUMS &&
        shasum -a 256 -c SHA256SUMS >/dev/null &&
        echo "七个文件的校验和都对上了")

    printf '\n包好了：%s\n  commit %s，版本 %s\n\n放进篮子：\n  cherry add <篮子名> %s\n' "$DIR" "$COMMIT" "$V" "$DIR"
