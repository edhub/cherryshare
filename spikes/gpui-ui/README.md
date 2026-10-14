# Spike: gpui-kit 能不能当 UI

一次性验证，用完即弃。只回答四个问题：原生文件对话框、从 Finder 拖入、中文输入法、超长列表。

## 跑

```sh
cd spikes/gpui-ui
cargo run              # 正常跑，CPU 空闲
cargo run -- --drive   # 额外主动重绘几秒，打印虚拟列表的实测数据
```

前置条件：macOS 需要 Metal toolchain（`xcodebuild -downloadComponent MetalToolchain`，约 840MB）。

## 已验证

机器可测的部分：

- [x] **能构建**：`gpui-kit 0.7.3` + `gpui-pre 0.3.9` 在 macOS 27 / Xcode 27 下通过。首次构建 2m36s，增量约 1.5s。
- [x] **能开窗并渲染**：窗口 1536×1052，Metal 路径正常，无 panic。
- [x] **虚拟列表是虚拟的**：200,000 行，每帧只构建 11～14 行元素（列表高 220px，行高 20px，可见 11 行）。
- [x] **API 存在且能编译**：`App::prompt_for_paths(PathPromptOptions) -> Receiver<Result<Option<Vec<PathBuf>>>>`；外部拖入被 GPUI 翻译成内部 drag，payload 是 `ExternalPaths(SmallVec<[PathBuf; 2]>)`，用普通的 `on_drop` 接。

人工验证（已全部通过）：

- [x] 文件对话框弹出 macOS 原生面板并回传绝对路径
- [x] 从 Finder 拖入拿到真实路径
- [x] **中文输入法正常**——这是唯一无法用代码模拟、也是风险最高的一条

## 踩到的坑

- `uniform_list` 的高度要加在它**自己**身上（`.h(px(220.))`），套一层定高 `div` 会让它塌成一行高；行元素需要显式高度。
- `v_flex()` / `h_flex()` 来自 `gpui_kit::base::StyledExt`，不在默认 prelude 里。
- `gpui-kit` 里没有 `Dropzone` 组件（我在网上搜到的那个属于另一个 GPUI 组件库），拖拽要自己用 `on_drop::<ExternalPaths>` 接。
