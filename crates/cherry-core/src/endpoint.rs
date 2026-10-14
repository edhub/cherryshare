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
