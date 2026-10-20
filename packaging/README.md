# CherryShare __VERSION__

让一台机器把本地文件只读分享给别人。接收者用浏览器看和下载，也可以用系统 NFS
客户端挂成目录，不需要安装 CherryShare。Linux 挂载可能需要先装 NFS 客户端包。

这个文件夹里是发布包。只用于可信局域网；HTTP 和 NFS 都不加密，不要把端口直接暴露到公网。
链接里的篮子 id 就是访问凭证，不要转发给不该看到内容的人。

## 先看或挂载

打开对方发来的网页链接，就能浏览和下载。想当目录用，在页面展开“挂载说明”，选择
macOS 或 Linux，复制命令到终端。挂上后是只读目录，不能修改创建者的文件。

macOS 挂载和卸载不需要 sudo，卸载是 `umount 挂载点`。Linux 挂载和卸载要 root，
卸载用 `sudo umount 挂载点`（准备步骤见下面「Linux 上挂别人的篮子」）。

## 装到自己机器上

    bash install.sh

它把 `cherry`、`cherryd` 拷到 `~/.local/bin`，把网页放到
`~/.local/share/cherryshare/web/`，再安装常驻服务。macOS 用 LaunchAgent，登录时启动；
Linux 用 systemd user unit，并尝试开启 linger。开启失败会提示，未开启时注销后服务可能停止。
安装不需要 sudo；若系统不允许当前用户开启 linger，需要按提示请管理员处理。服务异常退出会重启。

安装后用 `cherry status` 查看 NFS 和网页的监听是否起来。监听正常不代表网页文件一定完整，
还应打开分享链接确认一次。

取消常驻（二进制、网页和篮子配置都保留）：

    bash install.sh uninstall

**用 `bash install.sh`，别用 `./install.sh`。** 此前 macOS 挂载实测中直接执行曾报
`bad interpreter: Permission denied`，不把这个结果当成所有系统的规则。脚本还会先把
自己拷到本地，避免重启服务时原挂载点失效，导致脚本读到一半中断。

首次手动安装、不需要常驻时，可以自己拷。在发布包目录里先建目标目录，再按系统选一条
二进制复制命令（已有安装的更新优先重新跑安装脚本，不要覆盖正在运行的二进制）：

    mkdir -p ~/.local/bin ~/.local/share/cherryshare/web
    cp bin/macos-universal/cherry bin/macos-universal/cherryd ~/.local/bin/    # macOS
    cp bin/linux-x86_64/cherry bin/linux-x86_64/cherryd ~/.local/bin/          # Linux x86_64
    cp bin/linux-aarch64/cherry bin/linux-aarch64/cherryd ~/.local/bin/        # Linux arm64

然后复制网页并设置本地权限：

    cp -R web/. ~/.local/share/cherryshare/web/
    chmod 755 ~/.local/bin/cherry ~/.local/bin/cherryd
    chmod -R u=rwX,go=rX ~/.local/share/cherryshare/web

临时使用时前台运行 `cherryd`，保持这个终端开启，在另一个终端运行 `cherry`。Ctrl-C 停服务。

`~/.local/bin` 不在 PATH 里的话，往 `~/.zshrc`（或 `~/.bashrc`）加一行：

    export PATH="$HOME/.local/bin:$PATH"

macOS 那份是 ad-hoc 签名的：从挂载点拷过来不会带上 macOS 的 quarantine 标记，所以
不该有"无法验证开发者"的弹窗。

## 反过来，你自己分享东西给别人

先让服务常驻（`bash install.sh` 就是干这个的；临时用一下也可以直接前台跑 `cherryd`，
Ctrl-C 停）。然后：

    cherry list                            # 每个篮子一段：名字和可用地址的网页链接

篮子就是 `~/.config/cherryshare/` 里的一个文件（这个位置跟着 `XDG_CONFIG_HOME`），一行一条路径
（`~/` 开头或绝对路径）。
建篮子、加东西、拿走东西都是改这个文件；服务盯着它，存盘就生效，不用重起：

    vim ~/.config/cherryshare/我的照片.basket

    # 给谁看
    ~/Pictures/2026
    /Volumes/外置盘/素材

文件里没有 `id` 那行时服务会自己补一行（那是访问凭证，别删）。

把 `cherry list` 里的一条网页链接发给对方。通常有两条：本机名（如 `mbpmax.local`）
在前，本机 IP 在后；取不到的地址就不显示。名字解析正常时，DHCP 换地址后链接仍可用；
名字解析不了时换 IP。Linux 是否能用 `.local` 取决于 mDNS 配置，跨网段或 VPN 下也不保证可达。
这不是跨网方案。

网页能浏览、下载，并预览浏览器支持的视频、图片和部分纯文本。点表头可按名字、大小或
修改时间排序，再点一次反向，文件夹始终在前。想挂成目录，用页面上的 macOS / Linux
挂载命令。网页文件不在二进制里，换页面只需更新 `~/.local/share/cherryshare/web/`。

别的：

    cherry status                服务、NFS 和网页监听状态
    cherry list                  本机有哪些篮子及可用地址的网页链接
    cherry show 我的照片          把篮子文件原样打出来，顺带说哪几行读不到
    cherry show                  每个篮子都打一遍
    cherry                       全部命令和版本

往上只有读命令：写就是改文件，所以没有 `new` / `add` / `rm`。服务得一直开着才能读取
未缓存的内容。篮子里的名字取那一行路径的最后一段；两行最后一段一样时那个文件读不进来，
`cherry list` 会告诉你哪一行。文件放进去只说明路径在，不保证所有内容都有读取权限——
请从分享链接或挂载点实际试读。

源文件被删或移动后，列表会跳过它，`cherry show` 标一句「源不在了」，但文件里那一行不动。
权限或其他 IO 错误也一样：跳过、记日志，不改你的文件。macOS 日志在
`~/Library/Logs/cherryd.log`，Linux 用 `journalctl --user -u cherryd -f`；前台运行时看
终端输出。

停止分享一颗就是删掉那一行。想让发出去的旧链接失效有两条路：删掉整个篮子文件，或者改掉
文件里 `id` 那一行——两种都会换凭证，旧链接立刻打不开。只删路径行只是把篮子清空，
旧链接仍然有效。删掉或不改都不会取消已开始的网页下载或打包，已下载或缓存的字节也无法收回。

## Linux 上挂别人的篮子

**Linux 挂载和安装流程尚未在真实 Linux 环境验证。** 包里的二进制是 musl 静态构建，
不把交叉编译成功当成运行验证。第一次用请顺手检查挂载、读取和卸载。

先装 NFS 客户端（Debian / Ubuntu 是 `nfs-common`，别的发行版是 `nfs-utils` 之类）：

    sudo apt install nfs-common

推荐直接复制网页生成的 Linux 命令。下面只是格式示例，IP 和 id 必须换成对方的值
（Linux 的 `nolock` 比 macOS 的 `nolocks` 少一个 s）：

    sudo mkdir -p /mnt/cherry && sudo mount -t nfs -o ro,nolock,vers=3,tcp,port=20490,mountport=20490,soft,actimeo=5 10.1.2.52:/52cd04ab86d57f73cf58cc12207a54e4 /mnt/cherry

卸掉：

    sudo umount /mnt/cherry

## 会碰到什么

- **篮子里有一行读不进来**：`cherry list` 会把那个文件单独列出来，原因带行号——相对路径、
  两行最后一段撞名、`id` 写坏或者写了不止一行 `id`。这个文件整个不生效，别的篮子照常。
- **创建者关掉服务或网络断了**：未缓存的 NFS 读取会等待后报超时。macOS 使用生成的
  `soft` 命令实测约 8 秒，不保证 Linux 一样。先恢复服务和网络再重试，不是重挂就能解决。
- **创建者服务重启**：macOS 实测通常不用重挂。根句柄仍有效，缓存中的子句柄可能报
  `Stale NFS file handle`，客户端重新按名字查找后恢复，实测约两三秒。Linux 恢复行为未验证。
- **服务重起的时候你正打开着某个文件**（跑到一半的 `cp` / `rsync`）：那个句柄跑不掉了，
  得关掉重来。
- **挂载时报 `No such file or directory`**：检查篮子 id。服务没在跑或网络不通通常是
  连接或超时错误，不要只按 id 错误排查。
- **写不进去**（`Read-only file system`）：故意的，共享是只读的。
- **大文件拷到一半断了**：普通 `cp` 要重来。其他工具能否复用部分文件取决于工具和选项，
  不是 NFS 自动提供的。网页单文件支持 HTTP Range，下载工具可据此续传；目录 zip 不支持。
- **创建者那边增删了东西**：NFS 有属性缓存，生成命令设为 5 秒，不保证所有客户端严格
  在 5 秒内可见；网页要刷新或重新进入目录。文件夹大小在标签页内也可能有缓存，刷新重算。
- **zip 下载成功但少了文件或内容**：打包遇到打开 / 读取失败时可能跳过或截短，错误只在
  创建者日志里。重要内容请核对文件数、字节数或哈希，能解压不等于内容完整。
- **名字以 `_` 开头的顶层条目**：网页保留了这个前缀，列表可能显示但按文件路径访问不到；
  用 NFS 读取。普通隐藏文件不会自动排除，分享前请检查目录内容。
- **共享目录里的符号链接不会出现在列表里**，硬按名字读也是明确的"没有这个文件"。
  这是故意的：不然一个链接就能把共享边界外面的东西读出去。同理，`..` 出不了篮子根。

完整的失败清单见 `errors.md`。（那份是从仓库里直接拿过来的，里面的 ADR 编号指向仓库的
设计文档，这次没打进包里。）

## 这一包是什么

来自 `cherryshare` 仓库的 commit `__COMMIT__`，版本 __VERSION__，__DATE__ 编的。

相对 0.4.4 的主要变化：

- **篮子变成数据目录里的一个文件**：`~/.config/cherryshare/<名字>.basket`。一行一条路径，
  `id` 那一行是访问凭证。建篮子、加东西、拿走东西都是改这个文件；服务盯着它，存盘就生效，
  不用重起。删掉文件就是吊销，改掉 `id` 那一行就是换凭证。
- `cherry new` / `add` / `rm` 已删除，CLI 只剩 `status` / `list` / `show` 三个读命令。
  `cherry show` 打的是文件原文，并标出哪几行现在读不到。
- 老的 `baskets.json` 会在服务启动时自动拆成一个个 `.basket` 文件，原文件留成
  `baskets.json.migrated`。
- 网页的挂载说明可以在本机名和 IP 两条地址之间切换。

- `bin/macos-universal/` —— macOS 通用二进制（arm64 + x86_64）。`cargo build --release`
  编出来，两个切片 `lipo` 合到一起之后重做了 ad-hoc 签名。Apple Silicon 上要 macOS 11
  以上，Intel 上要 10.12 以上；但 **Intel 那个切片只做了签名校验，没在真的 Intel 机器上
  跑过**。
- `bin/linux-x86_64/`、`bin/linux-aarch64/` —— Linux 的两种架构，`cargo zigbuild` 编的
  静态二进制（musl）。`install.sh` 按 `uname -m` 挑（0.4.3 起有 arm64）。
  **没在 Linux 上跑过**。
- `install.sh` —— 上面那个安装脚本。macOS 那一支跑过（装、取消、杀掉让它重起都试了）；
  Linux 那一支没跑过。
- `install.sh` **不在 e2e 里**：它要占真实端口、还要动 `$HOME`，要自动化得先搭个假 HOME。
  手工验过这三步：装一遍、再装一遍、从一个只读目录起也要能修好（0.4.2、0.4.3 都验了）。
  以后它要是频繁出问题，再考虑给它搭这条测试。
- 接收者浏览和下载需要浏览器；挂载需要 NFS 客户端，Linux 可能需另装。创建者安装
  常驻服务需要 macOS launchd 或 Linux systemd；不使用 systemd 时可手动前台运行。
- 设计文档（ADR）在仓库里，没打进这个包。
