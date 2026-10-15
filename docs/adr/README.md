# ADR 索引

| 编号 | 标题 | 状态 |
| --- | --- | --- |
| [0001](0001-basket-model.md) | basket 的模型与边界 | Accepted |
| [0002](0002-basket-access.md) | basket 的访问方式 | Accepted |
| [0003](0003-runtime-model.md) | 运行时模型 | Accepted |
| [0004](0004-ui-form.md) | UI 形态 | Accepted |
| [0005](0005-transport-stays-on-iroh.md) | 传输层留在 iroh / QUIC 上 | Accepted |
| [0006](0006-mount-data-path.md) | mount 的数据路径 | Accepted |

## 术语

项目叫 **CherryShare**：CLI 是 `cherry`，服务是 `cherryd`，挂载是 `cherry-mount`。分享单位叫 **basket**，篮子里装的是 **cherry**。

- **basket** —— 用户创建的分享单位。创建者是其中所有内容的唯一源。它是一篮 cherry，不是目录：文件留在原处，basket 里只记录引用。
- **cherry** —— basket 里的一颗：`名字 + 源路径 + 类型（文件 / 文件夹）`。名字在 basket 内唯一，默认取原文件名。
- **BasketLink** —— 分享用的那条链接：`{ addr: EndpointAddr, basket_id: [u8; 16] }`。
- **目录（listing）** —— **一个目录**的内容清单：`(名字, 种类, size, mtime)`。由创建者现算，不是制品，也不含 hash。（ADR-0006 之前是「由 cherry 展开出的整棵树」；改成一个目录，因为 FUSE 的 `readdir` 本来就是按目录的。）
- **服务** —— 持有全部状态的常驻进程（identity key、blobs store、索引缓存、basket 配置）。由系统拉起，独占这份清单。
- **客户端** —— 通过 local socket 使用服务的东西：GUI、CLI、mount。都不持有状态。

## 刻意没有什么

以下都是讨论过并被明确否掉的，列在这里是为了防止将来被当成"顺手加一下"重新提出。

**因为 basket 只有一个源**（创建者不在线就不可用）

- 账号、登录、身份系统
- 成员表、成员发现、入群审批
- 做种 / 常驻缓存节点 / 可用性广播
- 跨 basket 内容去重、多源下载（多源 ≠ 多流：对同一个源并发取不同区间是允许的）
- 服务端、中心目录、DHT

**因为不是多写者协作**

- `iroh-docs` / CRDT / gossip 广播树
- 目录的签名、版本号、本地缓存——目录是响应，不是制品

**因为自用、小范围、局域网优先**

- 沙盒与 App Store 分发
- 自建 relay；离线可用性（"对方必须在线"是产品定位，不是缺陷）
- token 兑换层、次数预算、把限流当访问控制

**因为不碰 web**

- HTTP 服务、端口、token、runtime 文件、SSE、跨进程版本协商
- 浏览器能打开的 UI

**因为不需要**

- 扫描用的文件监听器——**索引缓存让「比对」变成一次 `stat`，监听器没有用武之地**；而且监听器本身不可信（停过、丢事件、网络卷），必须配周期重扫兜底。完整理由见 ADR-0006
- store 的 pin / GC 生命周期——store 里没有"只此一份"的数据

**因为留在 iroh / QUIC 上**（ADR-0005）

- 自研 TCP 传输层
- QUIC-over-TCP（队头阻塞 + 用户态开销一分不省）

## 其他文档

- [`../errors.md`](../errors.md) —— 失败清单。
- [`../performance.md`](../performance.md) —— 性能调查记录（已排除清单、实测数据、自我更正）。
- [`../../spikes/gpui-ui/`](../../spikes/gpui-ui/) —— gpui-kit 的验证 spike（ADR-0004 的依据）。

## 已推迟

- **basket 的命名**（给 basket 起一个人可读的名字）。次要，暂不设计。
- **隐藏文件的默认处理**：创建者分享一个文件夹时，是否默认跳过以 `.` 开头的文件和目录（macOS 上任意文件夹都带 `.DS_Store`）。
- **macOS 路径授权的提示名义**、**退出时的 UX（菜单栏 / 开机自启）**、**local socket 的具体协议**、**identity key 的导出与恢复**。（记在 ADR-0003）
- **挂载的机制与策略**：macFUSE / FUSE-T / NFS loopback / File Provider 选哪个，inode 策略、块大小、预读、网络超时、**`entry_timeout` / `attr_timeout` 的具体取值**、xattr。真要动手时单独写 `docs/mount.md`。
