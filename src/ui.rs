use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Duration,
};

use gpui::{
    AnyElement, App, ClickEvent, Context, FontWeight, Image as GpuiImage,
    ImageFormat as GpuiImageFormat, ObjectFit, SharedString, Window, div, img, prelude::*, px, rgb,
    rgba,
};

use crate::{
    capture,
    model::{
        CalibrationRequest, CaptureDeviceInfo, CardAnalysis, ComparisonReport, Layout, MAX_CARDS,
        ScreenInfo, SelectedCaptureCard, supported_layouts, validate_calibration,
    },
    overlay::OverlaySession,
    pattern::{index_for_letter, letter_for_index, render_pattern},
    workflow::{
        CalibrationOutcome, CardMapping, WorkerMessage, build_report, export_report,
        start_calibration_worker,
    },
};

const BG: u32 = 0x0b0d10;
const PANEL: u32 = 0x12151b;
const PANEL_ALT: u32 = 0x171b22;
const BORDER: u32 = 0x272d36;
const TEXT: u32 = 0xeef1f4;
const MUTED: u32 = 0x8d96a3;
const ACCENT: u32 = 0xb6f34a;
const ACCENT_DARK: u32 = 0x1a260f;
const WARN: u32 = 0xffc857;
const ERROR: u32 = 0xff6b6b;
const OK: u32 = 0x54d6a1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Configure,
    Capturing,
    Mapping,
    Results,
}

#[derive(Clone, Debug)]
struct SelectedDevice {
    device_id: usize,
    mode_index: usize,
}

pub struct ComparatorApp {
    screens: Vec<ScreenInfo>,
    devices: Vec<CaptureDeviceInfo>,
    selected_screen: Option<u32>,
    selected_devices: Vec<SelectedDevice>,
    layout_index: usize,
    stage: Stage,
    progress: String,
    error: Option<String>,
    device_error: Option<String>,
    overlay: Option<OverlaySession>,
    worker_rx: Option<Receiver<WorkerMessage>>,
    outcome: Option<CalibrationOutcome>,
    mappings: Vec<CardMapping>,
    manual_mapping_required: usize,
    report: Option<ComparisonReport>,
    exported_path: Option<PathBuf>,
}

impl ComparatorApp {
    pub fn new() -> Self {
        let mut screen_error = None;
        let screens = match capture::enumerate_screens() {
            Ok(screens) => screens,
            Err(error) => {
                screen_error = Some(error);
                Vec::new()
            }
        };
        let devices = match capture::enumerate_capture_devices() {
            Ok(devices) => devices,
            Err(error) => {
                screen_error = Some(match screen_error {
                    Some(previous) => format!("{previous}\n{error}"),
                    None => error,
                });
                Vec::new()
            }
        };
        let selected_screen = screens
            .iter()
            .find(|screen| screen.primary)
            .or_else(|| screens.first())
            .map(|screen| screen.id);

        Self {
            screens,
            devices,
            selected_screen,
            selected_devices: Vec::new(),
            layout_index: 0,
            stage: Stage::Configure,
            progress: "等待配置".into(),
            error: screen_error,
            device_error: None,
            overlay: None,
            worker_rx: None,
            outcome: None,
            mappings: Vec::new(),
            manual_mapping_required: 0,
            report: None,
            exported_path: None,
        }
    }

    fn layouts(&self) -> Vec<Layout> {
        supported_layouts(self.selected_devices.len())
    }

    fn current_layout(&self) -> Option<Layout> {
        let layouts = self.layouts();
        layouts
            .get(self.layout_index.min(layouts.len().saturating_sub(1)))
            .copied()
    }

    fn build_request(&self) -> Result<CalibrationRequest, String> {
        let screen = self
            .selected_screen
            .and_then(|id| self.screens.iter().find(|screen| screen.id == id))
            .cloned()
            .ok_or_else(|| "请先选择需要接管的 DXI 屏幕。".to_string())?;
        let layout = self
            .current_layout()
            .ok_or_else(|| "请选择至少一张采集卡。".to_string())?;
        if self.selected_devices.len() != layout.card_count() {
            return Err("采集卡数量与拼接模式不一致。".into());
        }
        let mut cards = Vec::with_capacity(self.selected_devices.len());
        for selected in &self.selected_devices {
            let device = self
                .devices
                .iter()
                .find(|device| device.id == selected.device_id)
                .cloned()
                .ok_or_else(|| "有一张已选采集卡已离线，请重新枚举。".to_string())?;
            if device.modes.is_empty() {
                return Err(format!("{} 没有可用采集格式。", device.name));
            }
            cards.push(SelectedCaptureCard {
                mode_index: selected.mode_index.min(device.modes.len() - 1),
                device,
            });
        }
        Ok(CalibrationRequest {
            screen,
            layout,
            cards,
        })
    }

    fn toggle_device(&mut self, device_id: usize, cx: &mut Context<Self>) {
        if let Some(position) = self
            .selected_devices
            .iter()
            .position(|selected| selected.device_id == device_id)
        {
            self.selected_devices.remove(position);
        } else if self.selected_devices.len() >= MAX_CARDS {
            self.error = Some(format!("当前版本最多比较 {MAX_CARDS} 张采集卡。"));
        } else if let Some(device) = self.devices.iter().find(|device| device.id == device_id) {
            if device.modes.is_empty() {
                self.error = Some(format!(
                    "{} 无法读取采集格式。\n{}",
                    device.name,
                    device
                        .probe_error
                        .clone()
                        .unwrap_or_else(|| "请确认设备未被 OBS/其他程序占用。".into())
                ));
            } else {
                self.selected_devices.push(SelectedDevice {
                    device_id,
                    mode_index: device.default_mode().unwrap_or(0),
                });
            }
        }
        self.layout_index = 0;
        self.report = None;
        self.stage = Stage::Configure;
        cx.notify();
    }

    fn cycle_mode(&mut self, selected_index: usize, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_devices.get_mut(selected_index) else {
            return;
        };
        let Some(device) = self
            .devices
            .iter()
            .find(|device| device.id == selected.device_id)
        else {
            return;
        };
        if !device.modes.is_empty() {
            selected.mode_index = (selected.mode_index + 1) % device.modes.len();
        }
        cx.notify();
    }

    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        match capture::enumerate_capture_devices() {
            Ok(devices) => {
                self.devices = devices;
                self.selected_devices
                    .retain(|selected| self.devices.iter().any(|d| d.id == selected.device_id));
                self.device_error = None;
                self.progress = format!("已发现 {} 个视频采集设备", self.devices.len());
            }
            Err(error) => {
                self.device_error = Some(error);
            }
        }
        cx.notify();
    }

    fn start_calibration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worker_rx.is_some() {
            return;
        }
        self.error = None;
        let request = match self.build_request().and_then(|request| {
            validate_calibration(&request)?;
            Ok(request)
        }) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };

        let pattern = match render_pattern(request.screen.size, request.layout) {
            Ok(pattern) => pattern,
            Err(error) => {
                self.error = Some(format!("无法生成标定图: {error}"));
                cx.notify();
                return;
            }
        };
        let overlay = match OverlaySession::open(&request.screen, &pattern) {
            Ok(overlay) => overlay,
            Err(error) => {
                self.error = Some(format!("接管 DXI 屏幕失败: {error}"));
                cx.notify();
                return;
            }
        };
        drop(pattern);

        let (sender, receiver) = mpsc::channel();
        start_calibration_worker(request, sender);
        self.overlay = Some(overlay);
        self.worker_rx = Some(receiver);
        self.stage = Stage::Capturing;
        self.progress = "全屏标定图已显示，正在读取采集卡...".into();
        self.outcome = None;
        self.report = None;
        self.exported_path = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let finished = this
                    .update(cx, |this, cx| this.poll_worker(cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll_worker(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(receiver) = &self.worker_rx else {
            return true;
        };
        loop {
            match receiver.try_recv() {
                Ok(WorkerMessage::Progress(progress)) => {
                    self.progress = progress;
                    cx.notify();
                }
                Ok(WorkerMessage::Complete(outcome)) => {
                    self.worker_rx = None;
                    self.prepare_mapping(*outcome);
                    cx.notify();
                    return true;
                }
                Ok(WorkerMessage::Failed(error)) => {
                    self.worker_rx = None;
                    self.overlay = None;
                    self.stage = Stage::Configure;
                    self.error = Some(error);
                    cx.notify();
                    return true;
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.worker_rx = None;
                    self.overlay = None;
                    self.stage = Stage::Configure;
                    self.error = Some("采集线程意外退出。".into());
                    cx.notify();
                    return true;
                }
            }
        }
    }

    fn prepare_mapping(&mut self, outcome: CalibrationOutcome) {
        let tile_count = outcome.cards.len();
        let mut used_tiles = HashSet::new();
        let mut mappings = Vec::with_capacity(tile_count);
        let mut manual_required = 0;

        for card in &outcome.cards {
            let best = card.best();
            let detected_tile = best.letter.and_then(index_for_letter);
            let tile = detected_tile
                .filter(|tile| *tile < tile_count && !used_tiles.contains(tile))
                .or_else(|| (0..tile_count).find(|tile| !used_tiles.contains(tile)))
                .unwrap_or(0);
            if detected_tile != Some(tile) {
                manual_required += 1;
            }
            used_tiles.insert(tile);
            mappings.push(CardMapping {
                tile_index: tile,
                rotation_degrees: best.rotation_degrees,
            });
        }

        self.outcome = Some(outcome);
        self.mappings = mappings;
        self.manual_mapping_required = manual_required;
        self.stage = Stage::Mapping;
        self.progress = if manual_required == 0 {
            "OCR 已完成，请确认物理位置后计算".into()
        } else {
            format!("{manual_required} 张采集卡需要手动确认物理位置")
        };
    }

    fn cycle_mapping_tile(&mut self, card_index: usize, cx: &mut Context<Self>) {
        let card_count = self
            .outcome
            .as_ref()
            .map(|outcome| outcome.cards.len())
            .unwrap_or(0);
        if card_count == 0 {
            return;
        }
        if let Some(mapping) = self.mappings.get_mut(card_index) {
            mapping.tile_index = (mapping.tile_index + 1) % card_count;
        }
        cx.notify();
    }

    fn cycle_mapping_rotation(&mut self, card_index: usize, cx: &mut Context<Self>) {
        if let Some(mapping) = self.mappings.get_mut(card_index) {
            mapping.rotation_degrees = (mapping.rotation_degrees + 90) % 360;
        }
        cx.notify();
    }

    fn confirm_mapping(&mut self, cx: &mut Context<Self>) {
        let (Some(outcome), Some(request)) = (self.outcome.as_ref(), self.build_request().ok())
        else {
            self.error = Some("校准会话已失效，请重新开始。".into());
            return;
        };
        if self.mappings.len() != outcome.cards.len() {
            self.error = Some("采集卡映射数量不完整。".into());
            cx.notify();
            return;
        }
        let mut unique = HashSet::new();
        for mapping in &self.mappings {
            if !unique.insert(mapping.tile_index) {
                self.error = Some(format!(
                    "物理位置 {} 被重复分配，请逐张采集卡修正。",
                    letter_for_index(mapping.tile_index)
                ));
                cx.notify();
                return;
            }
        }
        match build_report(&request, outcome, &self.mappings) {
            Ok(report) => {
                self.report = Some(report);
                self.overlay = None;
                self.stage = Stage::Results;
                self.progress = "比较完成".into();
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn cancel_mapping(&mut self, cx: &mut Context<Self>) {
        self.overlay = None;
        self.outcome = None;
        self.mappings.clear();
        self.report = None;
        self.stage = Stage::Configure;
        self.progress = "已取消本次校准".into();
        cx.notify();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let (Some(report), Some(outcome)) = (self.report.as_ref(), self.outcome.as_ref()) else {
            return;
        };
        match export_report(report, outcome, &self.mappings) {
            Ok(path) => self.exported_path = Some(path),
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn render_header(&self) -> AnyElement {
        div()
            .h(px(66.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px_6()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(0x0e1116))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(32.0))
                            .rounded_md()
                            .bg(rgb(ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(0x10140c))
                            .font_weight(FontWeight::BOLD)
                            .child("CC"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Card Reader Comparator"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(MUTED))
                                    .child("采集卡像素位置与轴缩放比较器"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_pill("DXGI SCREEN"))
                    .child(status_pill("MEDIA FOUNDATION"))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child("v0.1 · Windows"),
                    ),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut sidebar = div()
            .id("sidebar-scroll")
            .w(px(365.0))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(0x0f1217))
            .overflow_y_scroll();

        sidebar = sidebar.child(section_heading("01", "DXI 输出屏幕"));
        if self.screens.is_empty() {
            sidebar = sidebar.child(empty_hint("没有发现 DXI 显示器"));
        } else {
            for (index, screen) in self.screens.clone().into_iter().enumerate() {
                let selected = self.selected_screen == Some(screen.id);
                let screen_id = screen.id;
                sidebar = sidebar.child(
                    div()
                        .id(("screen", index))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(if selected { rgb(ACCENT) } else { rgb(BORDER) })
                        .bg(if selected {
                            rgb(ACCENT_DARK)
                        } else {
                            rgb(PANEL)
                        })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_screen = Some(screen_id);
                            this.report = None;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(format!("显示器 {}", index + 1)),
                                )
                                .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(
                                    format!(
                                        "{}×{} · {} Hz",
                                        screen.size.width, screen.size.height, screen.refresh_hz
                                    ),
                                )),
                        )
                        .child(if screen.primary {
                            status_pill("主屏")
                        } else {
                            div().size(px(1.0)).into_any_element()
                        }),
                );
            }
        }

        let layouts = self.layouts();
        sidebar = sidebar.child(section_heading("02", "拼接模式"));
        if layouts.is_empty() {
            sidebar = sidebar.child(empty_hint("先选择采集卡，再选择拼接模式"));
        } else {
            let mut row = div().flex().flex_wrap().gap_2();
            for (index, layout) in layouts.into_iter().enumerate() {
                let selected = index == self.layout_index.min(layouts_for_count(self).len() - 1);
                row = row.child(
                    div()
                        .id(("layout", index))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(if selected { rgb(ACCENT) } else { rgb(BORDER) })
                        .bg(if selected {
                            rgb(ACCENT_DARK)
                        } else {
                            rgb(PANEL)
                        })
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.layout_index = index;
                            cx.notify();
                        }))
                        .child(layout.label()),
                );
            }
            sidebar = sidebar.child(row);
        }

        sidebar = sidebar.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(section_heading("03", "采集卡"))
                .child(
                    div()
                        .id("refresh-devices")
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_size(px(11.0))
                        .text_color(rgb(ACCENT))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_devices(cx)))
                        .child("重新枚举"),
                ),
        );

        if self.devices.is_empty() {
            sidebar = sidebar.child(empty_hint(
                self.device_error
                    .clone()
                    .unwrap_or_else(|| "未发现 Media Foundation 视频采集设备".into()),
            ));
        } else {
            let mut device_list = div()
                .id("device-list")
                .flex()
                .flex_col()
                .gap_2()
                .max_h(px(220.0))
                .overflow_y_scroll();
            for device in self.devices.clone() {
                let device_id = device.id;
                let selected_position = self
                    .selected_devices
                    .iter()
                    .position(|selected| selected.device_id == device_id);
                let selected = selected_position.is_some();
                let mode = selected_position
                    .and_then(|position| self.selected_devices.get(position))
                    .and_then(|selected| device.modes.get(selected.mode_index));
                let mode_label = mode
                    .or_else(|| {
                        device
                            .default_mode()
                            .and_then(|index| device.modes.get(index))
                    })
                    .or_else(|| device.modes.first())
                    .map(|mode| mode.label())
                    .unwrap_or_else(|| "无可用格式".into());
                let probe_error = device.probe_error.clone();
                device_list = device_list.child(
                    div()
                        .id(("device", device_id))
                        .flex()
                        .gap_3()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(if selected { rgb(ACCENT) } else { rgb(BORDER) })
                        .bg(if selected {
                            rgb(ACCENT_DARK)
                        } else {
                            rgb(PANEL)
                        })
                        .cursor_pointer()
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_device(device_id, cx)),
                        )
                        .child(
                            div()
                                .mt_0p5()
                                .size(px(14.0))
                                .flex_none()
                                .rounded_sm()
                                .border_1()
                                .border_color(if selected { rgb(ACCENT) } else { rgb(MUTED) })
                                .bg(if selected {
                                    rgb(ACCENT)
                                } else {
                                    rgba(0x00000000)
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(device.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(if probe_error.is_some() {
                                            rgb(ERROR)
                                        } else {
                                            rgb(MUTED)
                                        })
                                        .child(probe_error.unwrap_or(mode_label)),
                                )
                                .child(div().text_size(px(9.0)).text_color(rgb(0x616a77)).child(
                                    format!(
                                        "设备号 {:02} · {}",
                                        device.id,
                                        short_device_path(&device.path)
                                    ),
                                )),
                        ),
                );
            }
            sidebar = sidebar.child(device_list);
        }

        if !self.selected_devices.is_empty() {
            let mut selected_list = div().flex().flex_col().gap_2();
            for (index, selected) in self.selected_devices.iter().enumerate() {
                let Some(device) = self
                    .devices
                    .iter()
                    .find(|device| device.id == selected.device_id)
                    .cloned()
                else {
                    continue;
                };
                let mode = device.modes.get(selected.mode_index);
                selected_list = selected_list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(device.name),
                                )
                                .child(
                                    div().text_size(px(10.0)).text_color(rgb(MUTED)).child(
                                        mode.map(|mode| mode.label())
                                            .unwrap_or_else(|| "无格式".into()),
                                    ),
                                )
                                .child(div().text_size(px(9.0)).text_color(rgb(0x616a77)).child(
                                    format!(
                                        "ID {:02} · {}",
                                        device.id,
                                        short_device_path(&device.path)
                                    ),
                                )),
                        )
                        .child(
                            div()
                                .id(("mode", index))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(PANEL_ALT))
                                .text_size(px(10.0))
                                .text_color(rgb(ACCENT))
                                .cursor_pointer()
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.cycle_mode(index, cx)),
                                )
                                .child("切模式"),
                        ),
                );
            }
            sidebar = sidebar.child(selected_list);
        }

        sidebar
            .child(
                div()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .id("start-calibration")
                            .h(px(44.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .bg(rgb(ACCENT))
                            .text_color(rgb(0x10140c))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.start_calibration(window, cx)
                                }),
                            )
                            .child("校验配置并开始"),
                    ),
            )
            .into_any_element()
    }

    fn render_configure(&self) -> AnyElement {
        let request = self.build_request();
        let validation = Some(match &request {
            Ok(request) => validate_calibration(request),
            Err(error) => Err(error.clone()),
        });
        let mut content = div()
            .id("configure-scroll")
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(26.0))
                    .font_weight(FontWeight::BOLD)
                    .child("先定位，再量化。"),
            )
            .child(
                div()
                    .max_w(px(720.0))
                    .text_size(px(13.0))
                    .text_color(rgb(MUTED))
                    .line_height(px(21.0))
                    .child(
                        "标定图案用 A–G 识别采集卡对应的物理分区，再用四角定位标拟合 X/Y 轴的缩放与平移。画面压缩损失不作为本版本的判据。",
                    ),
            )
            .child(workflow_steps())
            .child(self.render_validation_panel(validation.as_ref()));

        if let Ok(request) = request {
            let layout = request.layout;
            let tile_width = request.screen.size.width / layout.cols as u32;
            let tile_height = request.screen.size.height / layout.rows as u32;
            content = content.child(
                panel().child(label("预期的单卡画面")).child(
                    div()
                        .mt_2()
                        .flex()
                        .items_end()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(24.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("{tile_width}×{tile_height}")),
                        )
                        .child(
                            div()
                                .pb_1()
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "DXI {}×{} ÷ {} 个分区",
                                    request.screen.size.width,
                                    request.screen.size.height,
                                    layout.card_count()
                                )),
                        ),
                ),
            );
        }
        content.into_any_element()
    }

    fn render_validation_panel(&self, validation: Option<&Result<(), String>>) -> AnyElement {
        let (color, title, detail) = match validation {
            Some(Ok(())) => (
                OK,
                "配置可执行",
                "整数比例校验通过；开始时将接管所选 DXI 屏幕。",
            ),
            Some(Err(error)) => (WARN, "配置尚未通过", error.as_str()),
            None => (
                MUTED,
                "等待选择",
                "选择 DXI 屏幕、采集卡数量和拼接模式后显示校验结果。",
            ),
        };
        div()
            .w_full()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .flex()
            .gap_3()
            .child(div().mt_0p5().size(px(10.0)).rounded_full().bg(rgb(color)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child(detail.to_string()),
                    ),
            )
            .into_any_element()
    }

    fn render_capturing(&self) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_5()
            .p_8()
            .child(
                div()
                    .size(px(76.0))
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(ACCENT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(22.0))
                    .font_weight(FontWeight::BOLD)
                    .child("A"),
            )
            .child(
                div()
                    .text_size(px(21.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("正在采集标定画面"),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(MUTED))
                    .child(self.progress.clone()),
            )
            .child(
                div()
                    .mt_2()
                    .p_4()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child("全屏层显示期间可按 Esc 退出接管；主界面建议放在另一台控制显示器。"),
            )
            .into_any_element()
    }

    fn render_mapping(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(outcome) = &self.outcome else {
            return empty_hint("校准结果不存在");
        };
        let duplicate_positions = self
            .mappings
            .iter()
            .map(|mapping| {
                self.mappings
                    .iter()
                    .filter(|other| other.tile_index == mapping.tile_index)
                    .count()
                    > 1
            })
            .collect::<Vec<_>>();

        let mut grid = div()
            .id("mapping-grid")
            .flex()
            .flex_wrap()
            .gap_3()
            .overflow_y_scroll();
        for (index, (card, mapping)) in outcome.cards.iter().zip(&self.mappings).enumerate() {
            let detection = card.detection_for_rotation(mapping.rotation_degrees);
            let preview = card.preview_for_rotation(mapping.rotation_degrees);
            let duplicate = duplicate_positions.get(index).copied().unwrap_or(false);
            grid = grid.child(
                div()
                    .w(px(345.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(if duplicate { rgb(ERROR) } else { rgb(BORDER) })
                    .bg(rgb(PANEL))
                    .overflow_hidden()
                    .child(
                        div()
                            .h(px(170.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .overflow_hidden()
                            .bg(rgb(0x050607))
                            .child(preview_image(
                                preview,
                                card.native_size,
                                mapping.rotation_degrees,
                            )),
                    )
                    .child(
                        div()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(card.device_name.clone()),
                                    )
                                    .child(div().text_size(px(10.0)).text_color(rgb(MUTED)).child(
                                        format!(
                                            "{}×{}",
                                            card.native_size.width, card.native_size.height
                                        ),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(if let Some(letter) = detection.letter {
                                        status_pill_color(
                                            format!(
                                                "OCR {letter} · {:.0}%",
                                                detection.ocr_confidence * 100.0
                                            ),
                                            OK,
                                        )
                                    } else {
                                        status_pill_color("OCR 失败", ERROR)
                                    })
                                    .child(status_pill(format!(
                                        "定位标 {}/4",
                                        detection.marker_count
                                    ))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .id(("mapping-tile", index))
                                            .flex_1()
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .bg(rgb(PANEL_ALT))
                                            .border_1()
                                            .border_color(if duplicate {
                                                rgb(ERROR)
                                            } else {
                                                rgb(BORDER)
                                            })
                                            .text_size(px(11.0))
                                            .text_color(rgb(TEXT))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.cycle_mapping_tile(index, cx)
                                            }))
                                            .child(format!(
                                                "物理位置 {} · 点击切换",
                                                letter_for_index(mapping.tile_index)
                                            )),
                                    )
                                    .child(
                                        div()
                                            .id(("mapping-rotation", index))
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .bg(rgb(PANEL_ALT))
                                            .border_1()
                                            .border_color(rgb(BORDER))
                                            .text_size(px(11.0))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.cycle_mapping_rotation(index, cx)
                                            }))
                                            .child(format!("旋转 {}°", mapping.rotation_degrees)),
                                    ),
                            ),
                    ),
            );
        }

        let mut content = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(22.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("确认物理位置"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(if self.manual_mapping_required > 0 {
                                        rgb(WARN)
                                    } else {
                                        rgb(MUTED)
                                    })
                                    .child(self.progress.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button(
                                "cancel-mapping",
                                "取消",
                                false,
                                cx.listener(|this, _, _, cx| this.cancel_mapping(cx)),
                            ))
                            .child(action_button(
                                "confirm-mapping",
                                "确认并计算",
                                true,
                                cx.listener(|this, _, _, cx| this.confirm_mapping(cx)),
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child(
                        "截图已按所选旋转方向摆正。若存在盲插/交叉接线，逐张点击“物理位置”修正；A→G 与 DXI 分区按从左到右、从上到下对应。",
                    ),
            )
            .child(grid);

        if !outcome.warnings.is_empty() {
            content = content.child(
                panel().child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(WARN))
                        .child(outcome.warnings.join(" · ")),
                ),
            );
        }
        content.into_any_element()
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(report) = &self.report else {
            return empty_hint("没有比较结果");
        };
        let max_offset = report.max_offset_px;
        let warning_count = report
            .cards
            .iter()
            .filter(|card| {
                card.marker_count < 4
                    || card
                        .transform
                        .as_ref()
                        .map(|transform| {
                            transform.stretch_x_pct.abs() > 0.25
                                || transform.stretch_y_pct.abs() > 0.25
                                || transform.offset_x_px.abs() > 1.5
                                || transform.offset_y_px.abs() > 1.5
                        })
                        .unwrap_or(true)
            })
            .count();

        let mut content = div()
            .id("results-scroll")
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(24.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("画面位置比较"),
                            )
                            .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(
                                format!(
                                    "{}×{} · 拼接 {}×{} · {} 张采集卡",
                                    report.source_size.width,
                                    report.source_size.height,
                                    report.layout.cols,
                                    report.layout.rows,
                                    report.cards.len()
                                ),
                            )),
                    )
                    .child(action_button(
                        "export-report",
                        "导出报告",
                        true,
                        cx.listener(|this, _, _, cx| this.export(cx)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(metric_box(
                        "X 轴伸缩中位数",
                        format!("{:+.3}%", report.median_stretch_x_pct),
                    ))
                    .child(metric_box(
                        "Y 轴伸缩中位数",
                        format!("{:+.3}%", report.median_stretch_y_pct),
                    ))
                    .child(metric_box(
                        "中心偏移中位数",
                        format!(
                            "{:+.2}, {:+.2} px",
                            report.median_offset_x_px, report.median_offset_y_px
                        ),
                    ))
                    .child(metric_box(
                        "最大卡间偏差",
                        format!("{:.2} px · {} 告警", max_offset, warning_count),
                    )),
            );

        let mut rows = div().flex().flex_col().gap_2();
        for card in &report.cards {
            rows = rows.child(render_result_row(card));
        }
        content = content.child(rows).child(
            panel()
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("拉伸 / 偏移的表达顺序"),
                )
                .child(
                    div()
                        .mt_1()
                        .text_size(px(11.0))
                        .line_height(px(18.0))
                        .text_color(rgb(MUTED))
                        .child(
                            "本版本以画面中心报告“先按轴缩放，再平移”的等效模型；表中 source 偏移是同一仿射变换折算回源坐标后的“先平移，再缩放”表示。单帧无法区分两种因果顺序，因为它们代数等价，但两种校正比例均可复核。",
                        ),
                ),
        );
        if let Some(path) = &self.exported_path {
            content = content.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(ACCENT_DARK))
                    .text_size(px(11.0))
                    .text_color(rgb(ACCENT))
                    .child(format!("已导出：{}", path.display())),
            );
        }
        content.into_any_element()
    }

    fn render_error_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(error) = &self.error else {
            return div().size(px(0.0)).into_any_element();
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000b8))
            .child(
                div()
                    .w(px(500.0))
                    .p_5()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(ERROR))
                    .bg(rgb(PANEL))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .text_size(px(17.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("无法继续"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .line_height(px(20.0))
                            .text_color(rgb(TEXT))
                            .child(error.clone()),
                    )
                    .child(div().flex().justify_end().child(action_button(
                        "close-error",
                        "知道了",
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        }),
                    ))),
            )
            .into_any_element()
    }
}

impl Render for ComparatorApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.stage {
            Stage::Configure => self.render_configure(),
            Stage::Capturing => self.render_capturing(),
            Stage::Mapping => self.render_mapping(cx),
            Stage::Results => self.render_results(cx),
        };

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Microsoft YaHei UI")
            .child(self.render_header())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(content)),
            )
            .child(self.render_error_modal(cx))
    }
}

fn layouts_for_count(app: &ComparatorApp) -> Vec<Layout> {
    app.layouts()
}

fn section_heading(number: &'static str, title: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(ACCENT))
                .font_weight(FontWeight::BOLD)
                .child(number),
        )
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .into_any_element()
}

fn label(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(10.0))
        .text_color(rgb(MUTED))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
        .into_any_element()
}

fn panel() -> gpui::Div {
    div()
        .w_full()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
}

fn empty_hint(text: impl Into<SharedString>) -> AnyElement {
    div()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .text_size(px(11.0))
        .text_color(rgb(MUTED))
        .child(text.into())
        .into_any_element()
}

fn status_pill(text: impl Into<SharedString>) -> AnyElement {
    status_pill_color(text, MUTED)
}

fn status_pill_color(text: impl Into<SharedString>, color: u32) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .text_size(px(9.5))
        .text_color(rgb(color))
        .child(text.into())
        .into_any_element()
}

fn action_button(
    id: &'static str,
    text: &'static str,
    primary: bool,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(if primary { rgb(ACCENT) } else { rgb(BORDER) })
        .bg(if primary { rgb(ACCENT) } else { rgb(PANEL_ALT) })
        .text_color(if primary { rgb(0x10140c) } else { rgb(TEXT) })
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .active(|style| style.opacity(0.82))
        .on_click(listener)
        .child(text)
        .into_any_element()
}

fn workflow_steps() -> AnyElement {
    let step_data = [
        ("1", "选源", "选择要接管的 DXI 屏幕"),
        ("2", "定模", "校验整数比例与拼接分区"),
        ("3", "识卡", "OCR 给出 A–G 物理位置"),
        ("4", "量化", "拟合 X/Y 拉伸、偏移与抖动"),
    ];
    let mut row = div().flex().gap_3();
    for (number, title, detail) in step_data {
        row = row.child(
            div()
                .flex_1()
                .p_3()
                .rounded_lg()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(ACCENT))
                        .child(number),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child(detail),
                ),
        );
    }
    row.into_any_element()
}

fn preview_image(
    bytes: &[u8],
    native_size: crate::model::PixelSize,
    rotation_degrees: u16,
) -> AnyElement {
    let image = Arc::new(GpuiImage::from_bytes(GpuiImageFormat::Png, bytes.to_vec()));
    let (width, height) = if rotation_degrees % 180 == 90 {
        (native_size.height as f32, native_size.width as f32)
    } else {
        (native_size.width as f32, native_size.height as f32)
    };
    let scale = (345.0 / width).min(170.0 / height);
    img(image)
        .w(px(width * scale))
        .h(px(height * scale))
        .object_fit(ObjectFit::Contain)
        .into_any_element()
}

fn metric_box(title: &'static str, value: String) -> AnyElement {
    div()
        .flex_1()
        .p_3()
        .rounded_lg()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(div().text_size(px(9.5)).text_color(rgb(MUTED)).child(title))
        .child(
            div()
                .mt_1()
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(value),
        )
        .into_any_element()
}

fn render_result_row(card: &CardAnalysis) -> AnyElement {
    let transform = card.transform.as_ref();
    let warning = card.marker_count < 4
        || transform
            .map(|value| {
                value.stretch_x_pct.abs() > 0.25
                    || value.stretch_y_pct.abs() > 0.25
                    || value.offset_x_px.abs() > 1.5
                    || value.offset_y_px.abs() > 1.5
            })
            .unwrap_or(true);
    div()
        .w_full()
        .px_4()
        .py_3()
        .rounded_md()
        .border_1()
        .border_color(if warning { rgb(0x5d4923) } else { rgb(BORDER) })
        .bg(rgb(PANEL))
        .flex()
        .items_center()
        .gap_4()
        .child(
            div()
                .size(px(34.0))
                .flex_none()
                .rounded_md()
                .bg(if warning {
                    rgb(0x352a18)
                } else {
                    rgb(ACCENT_DARK)
                })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(15.0))
                .font_weight(FontWeight::BOLD)
                .text_color(if warning { rgb(WARN) } else { rgb(ACCENT) })
                .child(card.assigned_letter.to_string()),
        )
        .child(
            div()
                .w(px(190.0))
                .flex_none()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(card.device_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(9.5))
                        .text_color(rgb(MUTED))
                        .child(format!(
                            "{}×{} · OCR {} · 旋转 {}°",
                            card.native_size.width,
                            card.native_size.height,
                            card.detected_letter
                                .map(|letter| letter.to_string())
                                .unwrap_or_else(|| "--".into()),
                            card.rotation_degrees
                        )),
                ),
        )
        .child(metric_cell(
            "拉伸 X / Y",
            transform
                .map(|value| {
                    format!(
                        "{:+.3}% / {:+.3}%",
                        value.stretch_x_pct, value.stretch_y_pct
                    )
                })
                .unwrap_or_else(|| "--".into()),
        ))
        .child(metric_cell(
            "中心偏移 X / Y",
            transform
                .map(|value| format!("{:+.2} / {:+.2} px", value.offset_x_px, value.offset_y_px))
                .unwrap_or_else(|| "--".into()),
        ))
        .child(metric_cell(
            "源偏移 X / Y",
            transform
                .map(|value| {
                    format!(
                        "{:+.2} / {:+.2} px",
                        value.source_offset_x_px, value.source_offset_y_px
                    )
                })
                .unwrap_or_else(|| "--".into()),
        ))
        .child(metric_cell(
            "时序抖动",
            format!(
                "{:.2} / {:.2} px",
                card.temporal_jitter_x_px, card.temporal_jitter_y_px
            ),
        ))
        .into_any_element()
}

fn metric_cell(label: &'static str, value: String) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(120.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(9.0)).text_color(rgb(MUTED)).child(label))
        .child(div().text_size(px(11.0)).child(value))
        .into_any_element()
}

fn short_device_path(path: &str) -> String {
    if path.is_empty() {
        return "无设备路径".into();
    }
    let lowercase = path.to_ascii_lowercase();
    if let Some(start) = lowercase.find("vid_")
        && let Some(vendor_product) = path[start..].split('#').next()
    {
        return vendor_product.to_string();
    }
    let tail = path
        .rsplit(['\\', '/', '#'])
        .find(|part| !part.is_empty())
        .unwrap_or(path);
    if tail.len() <= 28 {
        tail.to_string()
    } else {
        let suffix = tail
            .chars()
            .rev()
            .take(27)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        format!("…{suffix}")
    }
}
