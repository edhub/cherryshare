//! GPUI Kit spike —— 只验证四件事，验证完即可丢弃：
//!   1. 原生文件对话框能否拿到真实绝对路径
//!   2. 从 Finder 拖入能否拿到真实路径
//!   3. 输入框能否用中文输入法
//!   4. 超长列表能否流畅滚动

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// 诊断用：证明窗口真的在画，以及虚拟列表真的只构建可见行。
static FRAMES: AtomicUsize = AtomicUsize::new(0);
static ROWS_BUILT: AtomicUsize = AtomicUsize::new(0);

use gpui_kit::base::StyledExt;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::*;

const ROW_COUNT: usize = 200_000;

struct Spike {
    input: Entity<InputState>,
    typed: SharedString,
    picked: SharedString,
    dropped: SharedString,
    rows: Arc<Vec<SharedString>>,
    _subscriptions: Vec<Subscription>,
}

impl Spike {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("在这里打中文，比如：测试文件"));

        let subscription = cx.subscribe_in(&input, window, |this, state, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.typed = state.read(cx).value().to_string().into();
                cx.notify();
            }
        });

        let rows = Arc::new(
            (0..ROW_COUNT)
                .map(|i| SharedString::from(format!("文件 {i:06}.bin")))
                .collect(),
        );

        Self {
            input,
            typed: "".into(),
            picked: "(未选择)".into(),
            dropped: "(还没拖东西进来)".into(),
            rows,
            _subscriptions: vec![subscription],
        }
        .driven(cx)
    }

    /// GPUI 按需重绘，没输入就没帧。加 `--drive` 时主动驱动一段帧，用来量虚拟列表。
    fn driven(self, cx: &mut Context<Self>) -> Self {
        if !std::env::args().any(|a| a == "--drive") {
            return self;
        }
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            for _ in 0..600 {
                executor.timer(Duration::from_millis(8)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
            println!("[spike] 驱动结束");
        })
        .detach();
        self
    }

    fn prompt(&mut self, directories: bool, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: true,
            prompt: None,
        });

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                let text = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                this.update(cx, |this, cx| {
                    this.picked = text.into();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = FRAMES.fetch_add(1, Ordering::Relaxed);
        if frame == 0 {
            println!("[spike] 第一帧已渲染 —— 窗口真的在画");
        }
        // 每约两秒（120 帧）报一次：证明虚拟化
        if frame > 0 && frame % 120 == 0 {
            let rows = ROWS_BUILT.swap(0, Ordering::Relaxed);
            println!(
                "[spike] 帧 {frame}：最近 120 帧共构建 {rows} 行（列表 {ROW_COUNT} 行）；viewport={:?}",
                window.viewport_size()
            );
        }

        let rows = self.rows.clone();

        div()
            .v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child("GPUI Kit Spike")
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child("1. 原生文件对话框")
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                Button::new("pick-dir")
                                    .label("选文件夹")
                                    .on_click(cx.listener(|this, _, _, cx| this.prompt(true, cx))),
                            )
                            .child(
                                Button::new("pick-files")
                                    .label("选文件")
                                    .on_click(cx.listener(|this, _, _, cx| this.prompt(false, cx))),
                            ),
                    )
                    .child(self.picked.clone()),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child("2. 从 Finder 拖入")
                    .child(
                        div()
                            .id("dropzone")
                            .h(px(72.))
                            .w_full()
                            .border_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("把文件或文件夹拖到这里")
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                let text = paths
                                    .0
                                    .iter()
                                    .map(|p| p.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                this.dropped = text.into();
                                cx.notify();
                            })),
                    )
                    .child(self.dropped.clone()),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child("3. 中文输入法")
                    .child(Input::new(&self.input))
                    .child(self.typed.clone()),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(format!("4. 大列表（{ROW_COUNT} 行）"))
                    .child(uniform_list(
                        "rows",
                        rows.len(),
                        move |range: Range<usize>, _, _| {
                            ROWS_BUILT.fetch_add(range.len(), Ordering::Relaxed);
                            if FRAMES.load(Ordering::Relaxed) <= 1 {
                                println!("[spike] 本帧列表渲染范围 = {range:?}");
                            }
                            range
                                .map(|i| {
                                    div()
                                        .h(px(20.))
                                        .px_2()
                                        .child(rows[i].clone())
                                        .into_any_element()
                                })
                                .collect()
                        },
                    )
                    .h(px(220.))
                    .border_1()),
            )
    }
}

fn main() {
    println!(
        "GPUI Kit spike\n\
         请逐项试：\n\
           1. 点「选文件夹」/「选文件」——看是否弹出 macOS 原生对话框，选完路径显示在下面\n\
           2. 从 Finder 拖几个文件到虚线框里——看路径是否显示出来\n\
           3. 在输入框里用中文输入法打字（比如输入「测试」）——看上方是否同步\n\
           4. 滚动下面那个 20 万行的列表\n\
         （加 --drive 会主动重绘几秒，用于量虚拟化）"
    );

    application()
        .with_assets(assets::Assets)
        .run(|cx| {
            init(cx);

            open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Spike::new(window, cx))
            })
            .expect("Failed to open window");
        });
}
