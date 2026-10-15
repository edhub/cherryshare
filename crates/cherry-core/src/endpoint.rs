//! 服务侧持有唯一一个 iroh endpoint。见 ADR-0003。

use anyhow::{Context, Result};
use iroh::{Endpoint, RelayMode, SecretKey, endpoint::presets};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use std::sync::Arc;

/// LAN 优先。
///
/// 不开 relay、不依赖 n0 DNS：`BasketLink` 带完整的 `EndpointAddr`，第一次连接
/// 不需要任何地址发现服务。mDNS 是显式打开的（`presets::N0` 里没有它），
/// 用来在对方 IP 变了之后还能找到人。
///
/// 要跨网段时，这里是 ADR-0002 说的两条退出通道的落点：换成
/// `presets::N0` 就接上 n0 DNS/pkarr 和公共 relay。
pub async fn bind(secret: SecretKey) -> Result<Endpoint> {
    Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .relay_mode(RelayMode::Disabled)
        .address_lookup(MdnsAddressLookup::builder())
        // 只覆盖下面两项，都有实测依据。
        //
        // 别顺手加 stream_receive_window：noq 的默认值是 1.25 MB，而它的公式看着
        // 就像一堵「单流 12.5 MB/s」的墙——
        //
        //     STREAM_RWND = MAX_STREAM_BANDWIDTH / 1000 * EXPECTED_RTT
        //                 = 12_500_000 B/s / 1000 * 100 ms = 1.25 MB
        //
        // 我们实测正好卡在 10.7 MB/s（86%），非常像个靶子。但把它调到 8 MB / 16 MB
        // 后各测 6 次，得到 9.9 / 10.5 / 10.7 MB/s——三档差 8%，而单样本在
        // 6.8~14.2 之间飘（±40%）。没有效果，所以用默认值。
        // （教训：这条链路上拿两个相邻样本比大小没有意义。我第一次用 2 个样本得出
        // 「窗口 +54%」，加到 6 个就塌了。上游 #4286 也是同样的思路，他们调到 32 MB
        // 报 37 → 48 MB/s，但在我们这里量不出来。）
        .transport_config(
            iroh::endpoint::QuicTransportConfig::builder()
                .enable_segmentation_offload(gso_enabled())
                .congestion_controller_factory(congestion_controller())
                .ack_frequency_config(ack_frequency())
                .build(),
        )
        .bind()
        .await
        .context("iroh endpoint 绑定失败")
}

/// UDP 分段卸载（GSO）默认关掉。
///
/// 这不是所有网卡驱动都支持：Orange Pi 6 的 wlan0（rtw89 / RTL8852BE）上 `sendmsg`
/// 会直接返回 `EIO`。noq 会自动降级，但降级前那个窗口里的丢包足以把整条连接打死——
/// 表现为「第一次传大文件必定失败，重试就好」，正好是最难排查的那种问题。
///
/// GSO 只是吞吐优化，不是正确性需要，所以默认关。
///
/// 注：实测它对吞吐其实**没有**可测量的影响（Mac 上开关 GSO：111 vs 115 Mbit/s，
/// 在噪声范围内），所以关掉它也不心痛。想拿它换吞吐就设 `CHERRY_GSO=1`。
fn gso_enabled() -> bool {
    matches!(
        std::env::var("CHERRY_GSO").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// 实验开关（`CHERRY_ACK_THRESHOLD=N`）：请对方「攒够 N 个包再回一个 ACK」。
///
/// 用的是 ACK_FREQUENCY 扩展，而且**默认就是协商好的**：双方都会声明 `min_ack_delay`
/// 这个传输参数，而 noq 的 `peer_supports_ack_frequency()` 判据就是它非空。
/// 所以**只需要在数据发送方设置**（它去请求接收方少回 ACK）；接收方不用动。
///
/// 为什么在意：在接收端数过，传 50MB 时
///
/// ```text
///                     正向帧   反向 ACK 帧   每 MB 反向帧
/// cherry (QUIC)       26,508      6,852          137
/// iperf3 (TCP)        16,808        955           19
/// ```
///
/// 也就是每 MB 多 7.2 倍的反向帧。半双工 WiFi 上，一个 90 字节的反向帧要抢一次空口
/// （DIFS + 退避 + 前导码 + SIFS + BA ≈ 130µs），而它携带的 90 字节本身只占 4µs。
///
/// noq 默认 `ack_eliciting_threshold = 1`，即「每隔一个 ack-eliciting 包就回 ACK」
/// （`AckFrequencyConfig::default()`）。TCP 靠 delayed-ACK + 内核 GRO 合并，
/// 实测是每 17.6 个帧才回一个。
///
/// **loopback 上实测**（噪声 ±3%，各跑 3 次）：
///
/// ```text
/// 阈值  1（默认）   56.1 MB/s   2.38 核
/// 阈值 10          68.5 MB/s   1.93 核   +22%
/// 阈值 50          73.7 MB/s   1.85 核   +31%，CPU -22%
/// ```
///
/// 单调、可复现到 ±0.5%。之前 WiFi 上看到的“阈值 10 反而更慢”是那条链路 ±40% 的
/// 噪声，不是这个旋钮的性质。
///
/// 机制：接收者 CPU 的头号开销就是它在回 ACK（`__sendmsg` 占 33.2%）。少回 ACK →
/// 少发数据报 → 少烧 CPU。
///
/// `CHERRY_REORDER`（`reordering_threshold`）在 loopback 上加不出额外效果——
/// 那里不乱序。它只可能在 WiFi 上起作用。
///
/// **为什么还不当默认值**：loopback 无丢包、无乱序，**恰好测不出这个改动的代价**
/// （延迟确认 → 更晚发现丢包 → 更晚重传）。代价有上界（`max_ack_delay`，默认 25ms），
/// 但要在丢包链路上验过才敢改默认。要验就用 WiFi，6 次起步。
fn ack_frequency() -> Option<iroh::endpoint::AckFrequencyConfig> {
    let varint = |n: u64| iroh::endpoint::VarInt::from_u64(n).ok();
    let threshold = env_num("CHERRY_ACK_THRESHOLD");
    let reorder = env_num("CHERRY_REORDER");
    if threshold.is_none() && reorder.is_none() {
        return None;
    }
    let mut cfg = iroh::endpoint::AckFrequencyConfig::default();
    if let Some(v) = threshold.and_then(varint) {
        cfg.ack_eliciting_threshold(v);
    }
    if let Some(v) = reorder.and_then(varint) {
        cfg.reordering_threshold(v);
    }
    Some(cfg)
}

fn env_num(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

// MTU 发现的上限被锁在 1452。记在这里，因为它是“为什么传数据要烧这么多 CPU”的答案之一，
// 而且现在从 cherry 这边改不了。
//
// noq 的 `MtuDiscoveryConfig::default()` 把 `upper_bound` 定在 1452（按 1500 以太网 MTU），
// 而 `EndpointConfig::max_udp_payload_size` 默认也是 1472，**两者取小**。所以不管链路真实
// MTU 多大，两端都只肯发/收 ~1.4KB 的包。
//
// loopback（MTU 16384）实测，传 200MB：
//
// ```text
//                  吞吐        CPU       CPU/字节   平均每包
// 默认           47.8 MB/s   2.34 核     49 ns/B     915 B
// 抬到 16K      223.9 MB/s   1.96 核      8.8 ns/B  11.4 KB
//               快 4.7 倍   反而少 16%   省 5.6 倍   包数少 13.3 倍
// ```
//
// 只对高 MTU 链路有效（loopback、开了 jumbo frame 的局域网）。普通以太网/WiFi（1500）
// 上 1452 本来就是对的，收益为零。
//
// 但反过来看更重要：四种包尺寸拟合出 `每包成本 = 44µs + 4.9 ns/字节`（两端合计）。
// 那 44µs 是固定的，在大包下被摊成 3.8 ns/B（看不见），**在 1400 字节的普通链路上
// 却是约 31µs/包、36 ns/字节——每核只能处理约 27 MB/s**。所以真正的瓶颈是那个固定
// 每包成本，而不是 MTU 本身；MTU 只是把它藏起来了。
//
// 那 44µs 是什么：xctrace 按 CPU 时间采样后，`__sendmsg` + `__recvmsg` 占了创建者
// 57.6%、接收者 42.4%（接收者最大的一项也是 `__sendmsg`——它在回 ACK）。不是锁、
// 不是加密、不是 BLAKE3、不是磁盘（都 ≤1%）。代价就是“每个数据报一次内核 UDP 收发”。
// 所以减包数有效，合系统调用（sendmsg_x）无效。详见 docs/performance.md。
//
// 上游文档明确推荐抬它（"Deployments on links with larger MTUs, e.g. loopback or
// Ethernet with jumbo frames"），但 **iroh 没有暴露 `max_udp_payload_size`**——它硬编码在
// `iroh/src/socket.rs` 内部。只抬 `upper_bound`（iroh 暴露了）没用，会被对端声明的 1472 夹住。
//
// 另：**不要动 `initial_mtu`**。把它抬到 16000 会把握手打死（pick 挂住）——握手就用
// initial_mtu，而对端还没准备收那么大的包，大包被静默丢弃。只抬 upper_bound 不会，
// 因为发现发生在握手之后。
//
// 完整的调查记录见 docs/performance.md。

/// 拥塞控制器。默认跟随 noq 的 Cubic。
///
/// noq 出厂用 Cubic（`noq-proto/src/config/transport.rs`）。在**高速**局域网上
/// Cubic 会崩：上游 n0-computer/iroh#4286（OPEN，标 `perf`）在 110 MB/s 的 LAN 上
/// 量到单流 iroh-blobs：BBR 42~48 MB/s，**Cubic 只有 1.3~1.7 MB/s（差 30 倍）**，
/// 报告里管这叫 noq 的拥塞控制器缺陷，只在「真实 LAN、有轻微丢包与 RTT 抖动」时发作。
///
/// 但那条结论**在这条链路上不成立**。干净链路上的交替 A/B 实测（Orange Pi WiFi）：
/// cubic 90 / 80 Mbit/s，bbr3 57 / 100 Mbit/s——同一控制器内部的波动就和两者之间的
/// 差异一样大，没有证据说 BBR 更好。所以默认跟随上游。
///
/// **什么时候该试 BBR**：把创建者挪到千兆有线网上之后。那种高 BDP 链路正是 #4286
/// 里 Cubic 崩掉的场景。用 `CHERRY_CC=cubic|reno|bbr3` 切。
fn congestion_controller() -> Arc<dyn noq_proto::congestion::ControllerFactory + Send + Sync> {
    match std::env::var("CHERRY_CC").as_deref() {
        Ok("bbr") | Ok("bbr3") => Arc::new(noq_proto::congestion::Bbr3Config::default()),
        Ok("reno") | Ok("new_reno") => Arc::new(noq_proto::congestion::NewRenoConfig::default()),
        _ => Arc::new(noq_proto::congestion::CubicConfig::default()),
    }
}
