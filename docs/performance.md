# 性能调查记录

一轮密集排查的账。**结论会过期，方法和“已排除清单”留下来。**

> **注意：这份调查讲的是 iroh 那条传输，而它已经删掉了**（ADR-0007 决策六）。里面的数字不再描述产品现在的任何路径。留下来的是两样跟走哪条路无关的东西：**测量方法论**（profiler 会少采、`du` 会被 APFS 的 clonefile 骗、能数计数器就别比耗时），以及一张**“哪些路已经堵死”的负面地图**——将来真要把 iroh 加回来时，省得重走。

## 怎么测才不被骗

这条链路（Mac ↔ Orange Pi 走 WiFi）的**单次样本噪声 ±40%**。用 2 个样本下过两次
错误结论（"窗口调大有 +54%"、"BBR3 更快"），样本加到 6 个都塌了。

- **交替 A/B，至少 6 次**，比比值而不是比相邻单样本。
- **能数确定性计数器就别比吞吐量。** 反向帧数、包数、CPU 时间都比吞吐稳定一个量级。
- **测吞吐前必须清空接收端 store。** 否则命中的是本地副本，量到的是"导出"而不是
  "传输"——我因此把 200MB 的导出当成传输，算出过 1.2 GB/s 的假数。
- **清空 store 之后要校验字节数/哈希**，别让失败的 fetch 静默变成"很快"。
- **绝对 CPU 用 `ps -o time`（单调计数器），别信 profiler 的绝对值。** `xctrace`
  的 Time Profiler 确实是按 CPU 时间采样（`sleep 10` 只报 1 个样本，不采阻塞线程——
  这点比 `sample` 强），**但它对多线程守护进程会少采**：同一进程 `ps` 量到 2.0 核，
  xctrace 只报 **0.26 核**，差 4 倍；而同机上的单/双线程 harness 两者吻合
  （1.84 vs 1.93 核）。**所以 xctrace 只能看形状，不能看量。**

## 已排除的（每条都有实测）

| 嫌疑 | 判据 |
|---|---|
| 中继 / n0 DNS | `BasketLink` 里只有 `Ip(...)`，没有 `Relay(...)` |
| mDNS 地址发现 | 旧链接跨 cherryd 重启仍 22ms 通 |
| store 自己的流水线 | loopback 导出 547 MB/s |
| BLAKE3（bao 校验）| profile 里 124 样本 |
| AEAD 加密 | 22 样本；走 ARMv8 硬件 AES |
| 磁盘 | `pread`/`pwrite` 极少 |
| Pi 的 CPU | 最忙的核 6~12%（按核看，别用 `vmstat` 的 12 核均值）|
| rtw89 省电模式 | 这个是**真 bug**：丢了 8~12.5% 的包，RTT 峰值 267ms。已修 |
| 每流接收窗口 1/8/16MB | 各 6 次：9.9 / 10.5 / 10.7 MB/s，无差别 |
| GSO（Linux） | 开关无差别 |
| Apple fast datapath | 只有 +8.9%，而接收端 CPU +32%。不值得 |
| 拥塞控制 BBR3 | 干净链路上 90 vs 78 Mbit/s，无优势 |
| **逐包系统调用** | 批量 10 倍（`MAX_TRANSMIT_SEGMENTS`）只换来 +8.9% |
| **逐包 cmsg** | 给 1452 字节的 `sendmsg` 加 `IP_TOS` / `IP_PKTINFO` cmsg：5.06 / 5.08 µs，
与不带（4.96）**没区别**。noq 每个包都带 cmsg，**但那不是成本**（基准见下一节）|
| **pacing 突发大小** | 改 `noq-proto` 的 `TARGET_BURST_INTERVAL` 2ms → 20ms / 100ms（突发大 10~50 倍）：
CPU 2.38 / 2.35 / 2.36 核，**无差别**。pacing 不是杠杆 |
| **裸 UDP 能省下 `__sendmsg` 的钱** | 错。裸 UDP（一次 `sendmsg`/包，1452 字节）是 **5 µs/包**，
cherry 是 **~43 µs**——差距真的存在，但位置不在 syscall 形状上 |
| **socket 缓冲区大小** | `SO_SNDBUF`/`SO_RCVBUF` 从 16MB 一路降到 64KB：**5.0~5.4 µs/包，无差别** |
| **tracing span 开销** | `poll_send` 每包建一个 `#[instrument]` span（默认 `info` 级，正好在过滤器里）。
把它滤掉（`CHERRY_LOG=warn`）：CPU 5.74 → 5.38 秒，**只值 6%** |
| **两次 syscall 就是两倍成本** | 口径错误。cherry 每个数据包付 **4 次 syscall**（数据 1发1收 + ACK 1发1收）+ 26% 的 ACK 数据报。
按这个折算基准是 ~6.4 µs/数据包，cherry 是 39.7 µs——**真实差距约 6 倍**，不是 12 倍 |

## 找到的

### 1. rtw89 省电模式（真 bug，已修）

```
echo 'options rtw89_core disable_ps_mode=Y' | sudo tee /etc/modprobe.d/rtw89.conf && sudo reboot
```

负载下丢包 8~12.5% → **0%**；RTT 均值 71 → 32ms，峰值 267 → 77ms。

### 2. MTU 发现的上限被锁在 1452

`MtuDiscoveryConfig::default().upper_bound = 1452`，而 `EndpointConfig::max_udp_payload_size`
默认 1472，**两者取小**。所以不管链路真实 MTU 多大，两端都只肯发/收 ~1.4KB 的包。

loopback（MTU 16384）传 200MB：

| | 吞吐 | CPU | CPU/字节 | 平均每包 |
|---|---|---|---|---|
| 默认 | 47.8 MB/s | 2.34 核 | 49 ns/B | 915 B |
| 抬到 16K | **223.9 MB/s** | **1.96 核** | **8.8 ns/B** | 11.4 KB |

**4.7 倍吞吐，CPU 反而少 16%，包数少 13.3 倍。**

**但只对高 MTU 链路有效**：loopback、开了 jumbo frame 的局域网（9000）。
普通以太网/WiFi（1500）上 1452 本来就是对的 → **收益为零**。

上游文档明确推荐抬它（*"Deployments on links with larger MTUs, e.g. loopback or
Ethernet with jumbo frames"*），但 **iroh 没有暴露 `max_udp_payload_size`**——
它硬编码在 `iroh/src/socket.rs` 内部。只抬 `upper_bound` 会被对端声明的 1472 夹住。

**注意：不要动 `initial_mtu`。** 抬到 16000 会把握手打死（fetch 挂住 90s+）：握手就用
`initial_mtu`，而对端还没准备收那么大的包，大包被静默丢弃。只抬 `upper_bound` 不会，
因为发现发生在握手之后。

### 3. 包数就是成本

每一包都要过一遍 QUIC：解析、AEAD、ACK、拥塞记账、流重组。所以**减少包数**
（大 MTU、避免多连接）比调任何拥塞参数都有效。

## 那个 ~44µs 的固定每包成本 —— 曾归因到内核 UDP 收发，**现已推翻**

> **后记。** 这条曲线是在**原默认（`num_cpus` 个 tokio worker）**下测的，所以那个 `44µs`
> 里混着一大块 **tokio 运行时开销**，不是"用户态 QUIC 的固有每包成本"——见下面 §5。
>
> 而且下那张 xctrace 归因表**不可信**：xctrace 对 cherryd 这种多线程守护进程**少采约 4 倍**
> （见"怎么测才不被骗"）。所以"时间全在 `__sendmsg`/`__recvmsg` 里"这个结论**不成立**。
>
> **仍然成立的部分**：包越小、每字节越贵（曲线本身），以及 MTU 上限被锁在 1452。

用四种包尺寸测（都清空接收端 store、都校验字节数），得到一条干净的曲线：

| MTU 上限 | 平均每包 | 吞吐 | CPU 总量 | **µs / 包** | **ns / 字节** | 包/秒 |
|---|---|---|---|---|---|---|
| 1452（默认）| 917 B | 46.9 MB/s | 10.64 核·s | 43.1 | 50.7 | 55,160 |
| 3000 | 2204 B | 98.3 MB/s | 5.62 核·s | 57.2 | 26.8 | 46,192 |
| 6000 | 4391 B | 151.4 MB/s | 3.46 核·s | 71.1 | 16.5 | 35,278 |
| 16000 | 11507 B | 251.3 MB/s | 1.82 核·s | 98.5 | 8.7 | 22,228 |

（CPU 是创建者+查看者两端合计；每行传的都是 200MB。）

最小二乘拟合：

```
每包成本 = 44.2 µs + 4.90 ns/字节          残差 -7.6% ~ +12.9%
```

- **`44 µs` 是固定每包成本**（两端合计），是健康 QUIC（~1~2µs/端）的 **20~40 倍**。
  还没找到它是什么。已排除加密、BLAKE3、磁盘、约 90% 的系统调用。
- `4.9 ns/字节` 正常（加密 + 校验 + 拷贝）。

**为什么要紧**：固定成本除以包大小才是每字节成本。在大包（11.5KB）下它只有 3.8 ns/B，
被摊得看不见；**换回普通以太网/WiFi 的 1400 字节包，它就是 31 µs/包、约 36 ns/字节**——
也就是每核只能处理约 **27 MB/s**。**这才是任何普通 MTU 链路上的真瓶颈。**

profile 剩下的三个线索（都还没验证）：

- `__psynch_mutexwait`（锁竞争，1,358 样本）
- `kevent`（reactor 唤醒，1,456 样本）
- iroh 自己那层逐包 socket 处理（地址映射 / 路径选择，每个包都要过）

### 归因结果（xctrace Time Profiler，按 CPU 时间采样）

之前用的 `sample` **不能做 CPU 归因**：它按墙钟采样所有线程，把阻塞在 `__psynch_cvwait`
的线程也算进去。那 27,155 个 cvwait 样本不是 CPU，是“这个线程存在且没跑”。
换成 `xctrace` 的 Time Profiler + `[profile.profiling]`（带符号）后才看清楚：

| 叶子帧 | 创建者 | | 接收者 | |
|---|---|---|---|---|
| **`__sendmsg`** | **779** | **51.6%** | **488** | **33.2%** |
| iroh `socket::transports::Transport` | 77 | 5.1% | 136 | 9.3% |
| `__recvmsg` | 90 | 6.0% | 135 | 9.2% |
| `kevent` / `mio::poll` | 72 | 4.8% | 83 | 5.7% |
| tokio scheduler | 62 | 4.1% | 20 | 1.4% |
| blake3 | 49 | 3.2% | 52 | 3.5% |
| store `bao_file` | — | — | 34 | 2.3% |
| `pwrite` | — | — | 16 | 1.1% |
| ring aead | 12 | 0.8% | 12 | 0.8% |
| `__psynch_mutexwait` | 10 | 0.7% | — | — |

**`__sendmsg` + `__recvmsg` 占了创建者 57.6%、接收者 42.4%。**
接收者最大的一项也是 `__sendmsg`——那是它在回 ACK。

> 正式排除：锁竞争（0.7%）、AEAD（0.8%）、BLAKE3（3.2%）、磁盘（1.1%）。
> 之前那个“锁竞争”线索是 `sample` 的假象。

**为什么 batching 只值 +8.9%**：`sendmsg_x` 摊薄的是系统调用入口/出口，而这块成本的大头是
**逐数据报的内核处理**（路由查表、cmsg、拷进对端缓冲），多一个 iovec 也省不掉。
所以**减包数有效（MTU），合系统调用无效**。这与 MTU 那条完全自洽。

**限定**：这是 macOS loopback 上的 `sendmsg`。loopback 的 UDP 发送路径特别重（完整协议栈
+ 拷贝），真实网卡上这部分由驱动/DMA 分担。**这个 51.6% 不能直接搬到 Pi 上。**

而且**它不可并行**：两个客户端拉同一个 blob 各剩一半（27.6 + 26.9 ≈ 单客户端 56）；
两个客户端拉**不同** blob 也一样（26.0 + 25.4 ≈ 51）；4/8 条连接反而更慢。
共同点指向"**每进程一个、所有连接共享的单线程 actor**"。

### 顺带记下的 QUIC vs TCP

同样 loopback，iperf3 TCP 是 7.7 GB/s，cherry QUIC 是 0.057 GB/s——但那 135 倍
**是 loopback 的假象**（没有网线，TCP 就是内核 memcpy）。真实链路上：

| | 吞吐 |
|---|---|
| TCP 单流（Pi→Mac，对端写 /dev/null）| 16.9 MB/s |
| cherry（对端写盘 + 校验）| 11.3 MB/s |

约 1.5×，**而且那 1.5× 不是 CPU 造成的**（Pi 的 CPU 只占 6~12%）。

**所以现在不值得换内核 TCP。** 触发重新考虑的条件：出现一条 ≥500 Mbit/s 的链路。
（已固化成 [ADR-0005](adr/0005-transport-stays-on-iroh.md)。）

### iroh 的 custom transports 帮不上忙

`unstable-custom-transports` 的抽象边界是**数据报**（`CustomSender::poll_send` 收
`&Transmit`），替换的是 UDP 套接字——**QUIC 还是照样跑在上面**，逐包开销一分不省。
写 QUIC-over-TCP 只会更糟（多一层成帧 + 队头阻塞）。它真正服务的场景是非 IP 路径
（内存、UNIX 套接字、Tor、蓝牙）。

### 4. ACK 频率（有真实收益，但还没敢进默认值）

ACK_FREQUENCY 扩展是默认就协商好的（双方都声明 `min_ack_delay`）。noq 默认
`ack_eliciting_threshold = 1`，也就是“每隔一个 ack-eliciting 包回一个 ACK”；
TCP 靠 delayed-ACK + 内核 GRO 合并，实测是每 17.6 个帧才回一个。

**loopback 实测**（噪声 ±3%，各 3 次）：

| 阈值 | 吞吐 | CPU |
|---|---|---|
| 1（默认）| 56.1 MB/s | 2.38 核 |
| 10 | 68.5 MB/s | 1.93 核 |
| **50** | **73.7 MB/s** | **1.85 核** |

单调、可复现到 ±0.5%：**+31% 吞吐，CPU 反而 -22%**。

机制：接收者 CPU 的头号开销就是它在回 ACK（`__sendmsg` 占 33.2%）。

**为什么不当默认值**：loopback 无丢包、无乱序，**恰好测不出这个改动的代价**
（延迟确认 → 更晚发现丢包）。代价有上界（`max_ack_delay`，默认 25ms），
但要在丢包链路上验过才敢改。

**但动机值得重新说一遍**：接收端 33% 的 CPU 花在回逐包 ACK 上——**少回 ACK 就是
少烧用户的电**。这个旋钮值钱的不是那 +31% 吞吐（局域网本来就链路受限），
而是"别让发起方白热"。

`CHERRY_REORDER`（`reordering_threshold`）在 loopback 上无额外效果（不乱序）。
WiFi 上的次线性降幅（阈值 ×50 只降到 5.3×）可能就是它造成的，但**无法在 loopback 上验**。

**和 worker 数叠加**（同一台机器，200 MiB）：

| 配置 | 吞吐 | CPU | ns/字节 |
|---|---|---|---|
| 原始 | 56.8~58.4 MB/s | 2.31~2.38 核 | ~41 |
| `CHERRY_WORKERS=1` | 66.0 / 66.4 | 1.83 / 1.83 | 27.7 |
| **`CHERRY_WORKERS=1` + 阈值 50** | **74.0 / 73.6** | **1.66 / 1.68** | **22.5** |

### 5. tokio worker 数 —— 那个 `44µs` 的主要来源

`cherryd` 原来是 `#[tokio::main]`，即 tokio 默认的 `num_cpus` 个 worker（这台机 ~10 个）。
把它显式压小，是整轮调查里最大的一条。实测（macOS loopback，200 MiB，两端合计 CPU，
单位 **ns/字节**）：

| workers | 1 个下载 | 2 个并发 | 4 个并发 |
|---|---|---|---|
| 1 | **27.5** | **26.8** | **26.8** |
| 2 | 35.3 | 49.4 | 49.9 |
| 4 | 41.8 | 65.1 | 84.5 |
| ~10（原默认）| 42.3 | 71.2 | **105.7** |

**worker 越多越差，而且成本随并发退化**：4 个并发下载在原默认下烧 **22.17 CPU 秒**
处理 200 MiB，而串行 4 次只要 8.9 秒——**浪费 2.5 倍**。

机制：这个负载是几条**热的单线程状态机**（endpoint driver、每条连接的 driver、
blob 传输）。摊到多个核上只会付**任务迁移 + 跳核唤醒 + 工作窃取**；压在少数 worker 上
反而让它们在同一核上跑，缓存是热的。

**顺带纠正一个直觉：worker=1 不会串行化。** 并发上去吞吐反而涨（57.3 → 65.0 → 68.0 MB/s），
而每字节成本**完全平坦**（~27 ns）。

**当前取值**：`CHERRY_WORKERS`，默认 **1**（见 `cherryd/src/main.rs` 的 `runtime()`）。

**前提与代价**：1 个 worker 意味着任何阻塞调用都会卡住整个守护进程。已知的阻塞点
已处理（`listing::list_dir` 进了 `spawn_blocking`；iroh-blobs 的 store 有自己的线程），
但 **iroh/noq 内部没审计过**——出现“守护进程整体卡住”的怪象时先试 `CHERRY_WORKERS=4`。

## 那剩下的 ~6 倍：**已知、未解释、便宜手段已用尽**

先得有基准线。**裸 UDP 只发一个 `sendmsg`（1452 字节）时，两端合计 ~4 µs/包**
（= ~3.5 ns/字节），用标准工具就能复现：

```
iperf3 -s -1 & iperf3 -c 127.0.0.1 -u -l 1452 -b 0 -t 5      # → 3.97 µs/包
```

把 W=1 + ACK 两个旋钮打开后，cherry 的每字节 CPU 从 ~41 ns 降到 **22.5 ns**，
但离那个基准还有 **~6 倍**（已按 4 次 syscall 的正确口径折算）。

**位置**：同一个 `sendmsg`/`__recvmsg`，在 cherry 的条件下内核要多花时间——
不在用户态协议逻辑里（传输层旋钮全部试过，见「已排除的」那张表）。

**工具的天花板**：xctrace 在 W=1 下终于是准的（接收者 0.92 核 vs ps 推 0.96 核），
**但它 `--attach` 极不稳定**（连着两次跑，一次只采到一边，一次两边都是 0），
所以至今没拿到 W=1 下可信的叶子帧分布。再往下要 kdebug/kperf 或 DTrace（需关 SIP）。

**但它在目标链路上兑换不了**：W=1 时一个核能跑 ~37 MB/s。WiFi 只给 11~17 MB/s，
所以 cherry 在真实链路上只用 **0.3~0.5 核**，是**链路受限**而不是 CPU 受限。
这 6 倍只在「≥500 Mbit/s 的链路」上才有意义——而那正是 ADR-0005 写的触发条件。

### 6. `BlobReader` 读 store 慢 100 倍 —— 用 `export_ranges`

`store.blobs().reader(hash)` 看着最自然（实现了 `AsyncRead` + `AsyncSeek`），实测却大约
**2 MB/s**：1 KiB 一次 store RPC，而且每次都从头走一遍 bao 树。

换成 `export_ranges(hash, offset..end).concatenate()`（一次遍历）后：

| 读长度 | `BlobReader` | `export_ranges` |
|---|---|---|
| 256 KiB | 90 ms | — |
| 1 MiB | 370 ms | **20 ms** |
| 4 MiB | 1940 ms | **30 ms** |
| 8 MiB | — | **40 ms**（≈200 MB/s）|

这也解释了之前那句「loopback 导出 547 MB/s」——快的是导出路径，不是 reader。
ADR-0003 担心的「文件系统的读是小块、同步的……会慢到不可用」，指的就是这一层。

### 7. iroh-blobs 的 store 语义 —— 几个查了很久才看清的地方

“取字节 / 放进 store / 按范围读回来”这条路我们最后没走，但下面这些是读代码加实测才弄清的，重走时能省很多时间。

**一个 blob 的数据有三种放法**（库自己的 `DESIGN.md`）：`inline`（在 redb 里）、`owned`（在 store 自己的规范目录里，路径可以从 hash 算出来）、`external`（在**用户自己的路径**里，路径必须显式存，一个 blob 记一组）。`ImportMode::TryReference` / `ExportMode::TryReference` 就是为 external 设计的：导入时“把文件编进目录、别复制”，导出时“**move** 到目标路径再记进目录”。outboard 永远不放用户路径。

**删一个 external 条目不会碰用户那个文件。** `store/fs/meta.rs` 里是显式写的：`Inline` 删内联数据、`Owned` 标记删 store 里的文件、`External` **空分支什么都不做**。所以“搬出去 + 让 gc 扫掉条目”是安全的。

**gc 的保留单位是 tag。** `run_gc` 从 `tags().list()` 和 `list_temp_tags()` 收集 live 集合，不在里面的全删；`GcConfig.add_protected` 是个回调，让你每次 gc 前再塞一批 hash 进去。**不传 gc 配置的话 `run_gc` 根本不启动**（`store/fs.rs`），也就是默认只增不减。`gc_run_once` 是公开的，可以按需手动跑。

**读 external 位置不重新校验**：`DataLocation::External(paths, size)` 只按长度信，`File::open` 之后不比对内容。库自己的文档承认 “less safe since the file might be modified”。

**macOS 上“复制一份”可能不花空间。** `reflink_or_copy` 先试 `reflink`，macOS 上就是 `clonefile`（APFS 写时复制）。实测：把一个 256 MiB 文件 `add_path` 进 store，卷只少了 **3 MiB**；`fetch` 下载 256 MiB 再导出到目标，卷只少了 **261 MiB**（真复制该是 512）。**`du` 对 clone 两边各自报满，所以判断空间占用必须看卷的空闲空间（`df -k`）前后差，不能用 `du`。**

## 网页打包下载（zip）的压缩级别 —— 跟上面那轮 iroh 无关

决定「下载整个文件夹」用什么级别时量的。放这儿是因为这份文件是量测的唯一去处。
单核，flate2 的 `DeflateEncoder`，一次 write_all，release。下面这组是 **miniz_oxide** 后端
（flate2 的默认）。实际发出去的那份走的是 **zlib-rs**：zip 9 的默认 deflate 已经不是 miniz，
而且还会拖上 zopfli 那个慢家伙，所以依赖里显式开了 `deflate-flate2-zlib-rs`。
zlib-rs 在 2 级上是 145 MB/s、压缩率 0.377（跟这组几乎一样），定 2 级的结论不变。
语料都 32 MiB 上下：文本（源码加 markdown 拼起来）、混合（八成文本 + 一点 Mach-O + 一点 png）、
媒体（四分之一文本、四分之一 Mach-O、一半随机数——代表已经压过的东西）。

| 级别 | 文本 压缩率/速度 | 混合 | 媒体 | 每 GiB（文本）|
|---|---|---|---|---|
| 1 | 0.449 / 178 MB/s | 0.459 / 180 | 0.779 / 232 | 5.9 s |
| 2 | 0.369 / 136 MB/s | 0.385 / 128 | 0.749 /  68 | 7.6 s |
| 3 | 0.344 /  96 MB/s | 0.362 /  89 | 0.740 /  61 | 10.8 s |
| 4 | 0.344 /  85 MB/s | 0.361 /  81 | 0.739 /  60 | 12.2 s |
| 5 | 0.339 /  69 MB/s | 0.357 /  65 | 0.737 /  55 | 14.8 s |
| 6 | 0.335 /  46 MB/s | 0.353 /  44 | 0.736 /  47 | 22.3 s |
| 9 | 0.335 /  31 MB/s | 0.352 /  29 | 0.735 /  45 | 33 s |

- **曲线在 3 级就平了**：3 → 6 少 2.6% 体积、CPU 翻一倍多；L4 跟 L3 体积一模一样。**crate 默认的 6 级是坏账。**
- **定 2 级**（2026-10-16）。1 → 2 少 18% 体积、CPU 多三成；再往上不值。
  理由：公司内网带宽不宽裕，省下的字节数算得回来。
- **媒体占比高的文件夹，2 级只有 68 MB/s**（比千兆网还慢），1 级是 232 MB/s。
  所以「认出已经压不动的扩展名直接 store」不是可选项——那些东西 1 到 6 级都只差 5.5% 体积，
  却差五倍 CPU。store 不进 deflate，1 GiB 半秒上下。
- **store 清单定了**（2026-10-16）：只按扩展名判，不看魔数，认不出的走 2 级 deflate。
  图片 png jpg jpeg gif webp avif heic heif jxl cr2 cr3 nef arw orf raf rw2；
  视频 mp4 m4v mov webm mkv avi mpg mpeg m2ts mts wmv flv 3gp ogv；
  音频 mp3 m4a aac ogg oga opus flac wma ape wv；
  容器 zip gz tgz bz2 xz zst 7z rar jar war apk ipa xpi crx cbz dmg iso pdf
  docx xlsm xlsx pptx odt ods odp epub pages numbers keynote sketch；字体 woff woff2。
  故意不收的：`ts`（多半是 TypeScript，不是 MPEG-TS）、tiff/wav（可能根本没压）、
  ttf/otf（deflate 能省一半）、dng（可以是未压的 TIFF）。
- **级别号不可移植**：同一个 6 级，miniz 压到 0.335、zlib-rs 0.341；1 级反而 zlib-rs 差（0.475 对 0.449）。
  换后端得重新量，不能照搬数字。
- **遍历本身很便宜**（打包和「算文件夹大小」共用同一套）：`read_dir` + 每个条目一次
  `symlink_metadata`（不读内容），缓存热时 **12~17 µs/项**。一千个文件 15 毫秒，十万个 1.5 秒。
  所以这类「现算」的东西慢不慢只看**文件数**，不看字节数；冷缓存要差一个量级（没在本机试过，
  不能 purge）。这也是为什么它不进列表那条路：NFS 上每次 `ls` 都算子树等于把 `ls` 变慢。
  页面上的目录大小就这么算：**1 万项或 0.5 秒**先到为准（条目数是主闸，确定性；时间是兜底，
  对付冷盘和外置盘），超了就说「文件太多」，不再往下走；同时最多算 4 个。

## 更正过的错误结论

| 说过的 | 实际 |
|---|---|
| "瓶颈就是逐包的系统调用" | 错。批量 10 倍只值 +8.9% |
| "N=4 不聚合是因为 ACK 吃掉空口" | 错。loopback 上没有空口成本，N=4 照样不涨 |
| "loopback 顶在 57 MB/s，是 QUIC 的上限" | 错。是 MTU 假象，修好是 224 MB/s |
| "`add_path` 会复制文件进 store" | 错。APFS 上走 clonefile 引用（store 1.0M / 源 208M）|
| "测 GSO 对吞吐无影响" | 测的是**接收端**的 GSO，而 GSO 只对发送端有意义——等于没测 |
| "MTU 只值 4.7 倍" | 其实**包越小收益越大**：普通 MTU 链路才是这个固定成本的主场 |
| "瓶颈在锁竞争 / reactor" | 错。那是 `sample` 把阻塞线程计入造成的假象。真凶是 `__sendmsg` |
| "阈值 10 反而更慢，所以这个旋钮不靠实" | 错。那是 WiFi 链路 ±40% 的噪声；loopback 上 1 < 10 < 50 完全单调 |
| "那 44µs 是 UDP 套接字的钱，裸 UDP 一分不省" | 错。裸 UDP 只要 **5 µs/包**，cherry 是 ~43 µs |
| "所以剩下的是用户态 QUIC 逻辑的开销" | 也错。主因是 **tokio worker 数**（运行时配置），不是协议逻辑 |
| "瓶颈在传输层" | 错。把传输层的旋钮试遍了（cmsg、pacing、GSO、多连接、MTU、CC）都无效，**杠杆在运行时** |
| "`sample` 不能用，`xctrace` 能用" | 只对一半。xctrace 不采阻塞线程（这点对），但对多线程守护进程**少采 4 倍** |

## 复现用的本地补丁

三个临时补丁试完就撤了，都没进仓库。这里记的是**改哪里、怎么接**，
比存一个补丁文件耐留：

- `iroh`：暴露 `IROH_MAX_UDP_PAYLOAD`（`iroh/src/socket.rs` 里 `endpoint_config` 之后）
- `netwatch`：调 `UdpSocketState::set_apple_fast_path()`（`netwatch/src/udp.rs` 的
  `UdpSocketState::new` 之后）
- `noq-proto`：`pacing.rs` 的 `TARGET_BURST_INTERVAL` / `MAX_BURST_SIZE` 改成可配置
  （**假设已被否定**，撤了）

用 `[patch.crates-io]` 指向本地路径接进来。注意 `iroh` 的 patch 必须连带
`iroh-base` / `iroh-relay` / `iroh-dns` 一起，否则图里会出现两个 `iroh-base`，
`iroh::PublicKey` 和 `iroh_base::PublicKey` 会被当成不同类型。
