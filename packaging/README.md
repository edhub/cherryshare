# CherryShare __VERSION__

一个很小的工具：让一台机器把本地的文件**借**给别人看。创建者那边开一个**只读**的
NFS 导出，对方用系统自带的客户端挂上来——对方什么都不用装。

这个文件夹里就是它本身。只有同一个局域网里能用。

## 先挂上

发你的那条消息里有挂载命令，粘进终端就行。给你的那段应该是两行（macOS 一行、Linux
一行），挑你这台机器那行。挂上之后是个普通的只读目录，创建者篮子里的东西就在里面。
卸掉是 `umount 那个路径`。

macOS 上挂载不用 sudo，Linux 上要 root（命令见下面「Linux 上挂别人的篮子」那一节）。

## 装到自己机器上

    bash install.sh

它做两件事：把 `cherry`、`cherryd` 拷到 `~/.local/bin`，再让系统把服务常驻起来
（macOS 用 LaunchAgent，Linux 用 systemd 的 user unit）。两边都只动你自己的家目录，
不需要 sudo。之后开机自己起，崩了也自己重起。

取消常驻（二进制留着）：

    bash install.sh uninstall

**用 `bash install.sh`，别用 `./install.sh`。** NFS 挂载点上的文件不能直接执行——
这是挂载的规矩，跟这个脚本无关，系统会报 `bad interpreter: Permission denied`。
脚本自己清楚这一点：它先把二进制拷到本地，再在那儿跑。

不想让它常驻就自己拷（两个都要）：

    cp bin/macos-universal/cherry bin/macos-universal/cherryd ~/.local/bin/    # macOS
    cp bin/linux-x86_64/cherry bin/linux-x86_64/cherryd ~/.local/bin/          # Linux

`~/.local/bin` 不在 PATH 里的话，往 `~/.zshrc`（或 `~/.bashrc`）加一行：

    export PATH="$HOME/.local/bin:$PATH"

macOS 那份是 ad-hoc 签名的：从挂载点拷过来不会带上 macOS 的 quarantine 标记，所以
不该有"无法验证开发者"的弹窗。

## 反过来，你自己分享东西给别人

先让服务常驻（`bash install.sh` 就是干这个的；临时用一下也可以直接前台跑 `cherryd`，
Ctrl-C 停）。然后：

    cherry new 我的照片                    # 建一个篮子
    cherry add 我的照片 ~/Pictures/2026     # 往里放东西（只是引用，文件不动）
    cherry link 我的照片                    # 打出链接 + 对方要粘的命令

把 `cherry link` 打出来的整段发给对方就行。别的：

    cherry list                  本机有哪些篮子
    cherry show 我的照片          篮子里有什么
    cherry rm 我的照片 2026       拿走一颗
    cherry --help                全部命令

服务得一直开着，关掉对方就读不了了。放进去的只是路径引用：源文件被删了、移了、或者
外置盘拔了，对方就取不到了，服务会把那颗从篮子里摘掉（日志里记一条，篮子里不留
打不开的名字）。`cherry add` 收相对路径（按你敲命令时所在的目录算），
篮子里记的永远是绝对路径。这一节在 macOS 和 Linux 上一样，不需要 root。

## Linux 上挂别人的篮子

**这一条我没在 Linux 上跑过。** 手上没有 Linux 机器，只确认了这个二进制是静态链接、
能执行、里面的命令和 macOS 那份是同一套代码编出来的。第一次用请顺手验一下。

先装 NFS 客户端（Debian / Ubuntu 是 `nfs-common`，别的发行版是 `nfs-utils` 之类）：

    sudo apt install nfs-common

然后（Linux 的选项名比 macOS 少一个 s，而且要 sudo）：

    sudo mkdir -p /mnt/cherry && sudo mount -t nfs -o nolock,vers=3,tcp,port=20490,mountport=20490,soft,actimeo=5 10.1.2.52:/52cd04ab86d57f73cf58cc12207a54e4 /mnt/cherry

卸掉：

    sudo umount /mnt/cherry

## 会碰到什么

- **创建者关掉了服务，或者你俩不在一个网里**：读的时候会等大约 8 秒，然后报
  `Operation timed out`。不会卡死，重新挂一次就行。
- **创建者那边的服务重起过**（崩了自动重起、机器重启、装了新版本）：**不用重挂。**
  挂载根、列目录、`df` 从第一秒起就是好的；头一两秒如果碰上你这边缓存里的旧句柄，会报
  一次 `Stale NFS file handle`，它自己会重新按名字找，实测两三秒后就全好了。
- **服务重起的时候你正打开着某个文件**（跑到一半的 `cp` / `rsync`）：那个句柄跑不掉了，
  得关掉重来。
- **挂载时报 `No such file or directory`**：要么 id 不对，要么对方没在跑。故意不区分
  "不存在"和"没权限"——对外来说是一回事。
- **写不进去**（`Read-only file system`）：故意的，共享是只读的。
- **大文件拷到一半断了**：得从头再来，NFS 不管这个。要续传用 `rsync -P 挂载点/文件 .`。
- **创建者那边增删了东西**：你这边最多 5 秒后才看得见（属性缓存的时长）。
- **共享目录里的符号链接不会出现在列表里**，硬按名字读也是明确的"没有这个文件"。
  这是故意的：不然一个链接就能把共享边界外面的东西读出去。同理，`..` 出不了篮子根。

完整的失败清单见 `errors.md`。（那份是从仓库里直接拿过来的，里面的 ADR 编号指向仓库的
设计文档，这次没打进包里。）

## 这一包是什么

来自 `cherryshare` 仓库的 commit `__COMMIT__`，版本 __VERSION__，__DATE__ 编的。

和 0.1.0 的差别（篮子里两份都在，可以对着看）：

- **创建者那边服务重启之后，对方不用重新挂载了。** 0.1.0 里会开始报
  `Stale NFS file handle`，只能重挂；__VERSION__ 里顶多头一两秒报一次，之后就自己好了。
- `cherry mount` 子命令删掉了——`cherry link` 打出来的那行命令本来就够用，
  而且它对两个系统各打一行（0.1.0 只打 macOS 的）。
- 新增 `install.sh`：装二进制 + 让系统把服务常驻起来（LaunchAgent / systemd）。

- `bin/macos-universal/` —— macOS 通用二进制（arm64 + x86_64）。`cargo build --release`
  编出来，两个切片 `lipo` 合到一起之后重做了 ad-hoc 签名。Apple Silicon 上要 macOS 11
  以上，Intel 上要 10.12 以上；但 **Intel 那个切片只做了签名校验，没在真的 Intel 机器上
  跑过**。
- `bin/linux-x86_64/` —— Linux x86_64，`cargo zigbuild` 编的静态二进制（musl）。
  **没在 Linux 上跑过**。
- `install.sh` —— 上面那个安装脚本。macOS 那一支跑过（装、取消、杀掉让它重起都试了）；
  Linux 那一支没跑过。
- 除了系统自带的 NFS 客户端，没有别的依赖。
- 设计文档（ADR）在仓库里，没打进这个包。
