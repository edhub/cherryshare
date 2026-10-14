# ADR-0004: UI 形态

- 状态: Accepted
- 日期: 2026-10-14

## 背景

ADR-0003 定了服务与客户端的关系，这里定 GUI 这个客户端用什么做。

曾认真考虑过的方案是"服务 serve 一个网页 UI"：核心监听 loopback HTTP，UI 是网页，CLI 与未来的挂载都是 HTTP 客户端。

## 决策

**native Rust UI，用 `gpui-kit`。不做 web UI，不做 webview。**

理由：

1. **与核心同语言。** 操作与事件的类型只定义一次（`cherry-control`），两端共用同一个 crate——不会有 JSON schema 与 Rust 类型之间的漂移。**但确实存在一条序列化边界**：ADR-0003 决定 GUI 是服务的客户端，不是核心本身，所以这一条不再是“没有 IPC”。
2. **没有本机攻击面。** 因果链是这样：web UI 得是浏览器能打开的东西 → 浏览器用不了 unix socket → 只能监听 loopback TCP → 于是必须处理四件事：任意本机进程能连（要 token + 只读 0600 的 runtime 文件）、任意网页能打（Host 校验挡 DNS rebinding、要求自定义 header 挡 CSRF）、以及最容易犯的"端口绑成 0.0.0.0"。native UI 没有这个约束，可以直接走 local socket，于是这四件一件都不需要。
3. 一套 Rust 代码覆盖三平台。
4. 体积小：不嵌 webview，不带 JS runtime。

**已验证**（代码与实测数据见 `spikes/gpui-ui/`）：

- 构建通过：`gpui-kit 0.7.3` / `gpui-pre 0.3.9`，macOS 27 / Xcode 27。首次 2m36s，增量约 1.5s
- 窗口渲染正常，Metal 路径无 panic
- **虚拟列表确实是虚拟的**：200,000 行，每帧只构建 11～14 行元素
- 原生文件对话框：`App::prompt_for_paths(PathPromptOptions) -> Receiver<Result<Option<Vec<PathBuf>>>>`
- 从 Finder 拖入：GPUI 把它翻译成内部 drag，payload 是 `ExternalPaths(SmallVec<[PathBuf; 2]>)`，用普通的 `on_drop` 接
- **中文输入法正常**（这一条是 spike 里唯一无法用代码模拟、也是风险最高的，已人工验证）

## 非目标

- **不做 web UI，也不做"分享服务顺便 serve 一个网页"。** 后者尤其要写死：一个监听本机的 HTTP 服务只对 UI 有效，不是分享通道；一旦把它当分享通道，就变成一个公网 HTTP 文件服务器，ADR-0001 / ADR-0002 里全部的能力与身份设计随之作废。

## 后果

正面：

- 没有端口，所以没有 token、没有 runtime 文件、没有 DNS rebinding / CSRF，也没有“端口绑成 0.0.0.0”这个最容易犯的错。访问控制由 socket 文件权限完成。
- UI 与核心在同一仓库、同一语言，类型不会漂移。
- macOS 的原生文件对话框与拖拽直接可用。

负面：

- **多了一条 IPC 序列化边界**（ADR-0003）。比 HTTP 那套小得多，但确实存在。
- **放弃 web 前端栈**（SvelteKit / Tailwind / DaisyUI）。UI 迭代慢于 web——没有 devtools 与热重载那种顺滑。
- **`gpui-kit` 建立在 pre-1.0 的 GPUI 之上**：官方明说版本间会有 breaking change，且 crates.io 上的 GPUI 发布严重滞后于 monorepo（`0.2.2` 从 2025-10 到 2026-07 一直是 latest）。必须锁版本，并接受升级成本。
- **平台成熟度不均**：macOS 是参考实现，Linux 明确落后，Windows 后加入。首要目标是 macOS，时序上吻合，但走到 unix / windows 阶段要重新评估。
- **系统集成要自己接**：GPUI 自绘界面、不用平台原生控件。文件对话框由 GPUI 提供，但"在 Finder 中显示""用默认程序打开""菜单栏"之类要自己调 AppKit。
- 构建前置条件：macOS 需要 Metal toolchain（`xcodebuild -downloadComponent MetalToolchain`，约 840MB）。

## 备选方案

1. **HTTP + SSE 服务 + web UI（浏览器直开或 Tauri 壳）** —— 弃。它换来的是“UI 可以用 web 栈写、可以热重载、浏览器能直接打开”，代价是一整套本机 HTTP 攻击面（token、runtime 文件、Host 校验、端口绑定）加一个需要长期维护的跨进程接口。**如果将来 gpui-kit 的维护成本变得不可接受，这是退路——但届时 ADR-0003 要一起改。**
2. **SwiftUI 原生** —— 弃。macOS 体验最好，但只有 macOS，与"后续 unix / windows"冲突。
3. **Tauri 加前端打包进 app** —— 弃。同 1，且多一个 webview、多一套可执行文件。

## 重新评估的触发条件

- `gpui-kit` / GPUI 的 breaking churn 变得难以承受 → 退到备选方案 1。
- unix / windows 阶段发现 GPUI 在那两个平台上的成熟度不可接受。
- 需要 UI 被远程访问（比如从手机看）→ 那是另一个产品形态，不是本 ADR 的延伸。
