# ADR-0004: UI 形态

- 状态: **Deferred**（2026-10-15 改）
- 日期: 2026-10-14

## 决策

**v1 不做 GUI。CLI 是完整的操作界面**（ADR-0003 已经把这条定成硬要求），所以砍掉 GUI 不损失任何能力。

理由：范围是自用、两个人之间（ADR-0001）。而做 GUI 要背上 `gpui-kit` 这一串东西——它建立在 pre-1.0 的 GPUI 之上，官方明说版本间会有 breaking change，平台成熟度不均（macOS 是参考实现，Linux 落后，Windows 更晚），构建还要先装 840MB 的 Metal toolchain。这些代价换不来 v1 里任何现在缺的东西。

## 将来真要做的话

**选原生 Rust（`gpui-kit`），不做 web UI，也不做 webview。**

三条理由，按重要性：

1. **不走本机 HTTP。** web UI 要浏览器能打开 → 浏览器用不了 unix socket → 只能监听 loopback TCP → 于是必须处理：任意本机进程能连（token + 只读 0600 的 runtime 文件）、任意网页能打（Host 校验挡 DNS rebinding、自定义 header 挡 CSRF）、以及最容易犯的"端口绑成 0.0.0.0"。native UI 直接走 local socket，这四件一件都不需要。
2. 与核心同语言：操作与响应的类型只定义一次（`cherry-control`），两端共用同一个 crate，不会有 JSON schema 与 Rust 类型漂移。
3. 一套 Rust 代码覆盖三平台，不嵌 webview、不带 JS runtime。

已经验证过的东西（代码与实测数据见 `spikes/gpui-ui/`）：`gpui-kit 0.7.3` 在 macOS 27 上能构建（首次 2m36s，增量约 1.5s）、窗口渲染走 Metal 无 panic、**虚拟列表确实是虚拟的**（200,000 行每帧只构建 11～14 行）、原生文件对话框与从 Finder 拖入可用、**中文输入法正常**（spike 里唯一无法用代码模拟、也是风险最高的一条，已人工验证）。

## 非目标

- **不做 web UI，也不做"分享服务顺便 serve 一个网页"。** 后者尤其要写死：一旦把一个监听本机的 HTTP 服务当成分享通道，就变成一个公网 HTTP 文件服务器，ADR-0001 / ADR-0002 里全部的能力与身份设计随之作废。**现在服务本来就有一个端口（NFS 导出，ADR-0007），这个诱惑比过去更大，所以这条更该留着。**

## 重新评估的触发条件

- 需要把工具交给不用终端的人 → 做 GUI，按上面那三条来。
- `gpui-kit` / GPUI 的 breaking churn 变得难以承受，或者它在 unix / windows 上的成熟度不可接受 → 退到"本机 HTTP + web UI"，但**届时 ADR-0003 要一起改**（客户端就多了一条 HTTP 边界）。
- 需要 UI 被远程访问（比如从手机看）→ 那是另一个产品形态，不是本 ADR 的延伸。
