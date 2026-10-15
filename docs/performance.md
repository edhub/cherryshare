# 性能调查记录

一轮密集排查的账。**结论会过期，方法和"已排除清单"留下来。**

## 怎么测才不被骗

这条链路（Mac ↔ Orange Pi 走 WiFi）的**单次样本噪声 ±40%**。用 2 个样本下过两次
错误结论（"窗口调大有 +54%"、"BBR3 更快"），样本加到 6 个都塌了。

- **交替 A/B，至少 6 次**，比比值而不是比相邻单样本。
- **能数确定性计数器就别比吞吐量。** 反向帧数、包数、CPU 时间都比吞吐稳定一个量级。
- **测吞吐前必须清空接收端 store。** 否则命中的是本地副本，量到的是"导出"而不是
  "传输"——我因此把 200MB 的导出当成传输，算出过 1.2 GB/s 的假数。
- **清空 store 之后要校验字节数/哈希**，别让失败的 pick 静默变成"很快"。

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

**注意：不要动 `initial_mtu`。** 抬到 16000 会把握手打死（pick 挂住 90s+）：握手就用
`initial_mtu`，而对端还没准备收那么大的包，大包被静默丢弃。只抬 `upper_bound` 不会，
因为发现发生在握手之后。

### 3. 包数就是成本

每一包都要过一遍 QUIC：解析、AEAD、ACK、拥塞记账、流重组。所以**减少包数**
（大 MTU、避免多连接）比调任何拥塞参数都有效。

## 还没解释的：~44µs 的固定每包成本 —— **已归因到内核 UDP 收发**

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

`CHERRY_REORDER`（`reordering_threshold`）在 loopback 上无额外效果（不乱序）。
WiFi 上的次线性降幅（阈值 ×50 只降到 5.3×）可能就是它造成的，但**无法在 loopback 上验**。

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

## 复现用的本地补丁

两个临时补丁试完就撤了，没进仓库：

- `iroh`：暴露 `IROH_MAX_UDP_PAYLOAD`（`iroh/src/socket.rs` 里 `endpoint_config` 之后）
- `netwatch`：调 `UdpSocketState::set_apple_fast_path()`（`netwatch/src/udp.rs` 的
  `UdpSocketState::new` 之后）

用 `[patch.crates-io]` 指向本地路径接进来。注意 `iroh` 的 patch 必须连带
`iroh-base` / `iroh-relay` / `iroh-dns` 一起，否则图里会出现两个 `iroh-base`，
`iroh::PublicKey` 和 `iroh_base::PublicKey` 会被当成不同类型。
