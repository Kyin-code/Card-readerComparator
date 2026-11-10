use serde::{Deserialize, Serialize};

pub const MAX_CARDS: usize = 7;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn center(self) -> Point {
        Point::new(self.x + self.width * 0.5, self.y + self.height * 0.5)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

impl PixelSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub rows: usize,
    pub cols: usize,
}

impl Layout {
    pub const fn new(rows: usize, cols: usize) -> Self {
        Self { rows, cols }
    }

    pub fn card_count(self) -> usize {
        self.rows * self.cols
    }

    pub fn label(self) -> String {
        format!("{} × {}", self.cols, self.rows)
    }

    pub fn tile_index(self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenInfo {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub size: PixelSize,
    pub primary: bool,
    pub refresh_hz: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub fourcc: String,
}

impl CaptureMode {
    pub fn label(&self) -> String {
        format!(
            "{}×{} @{}fps {}",
            self.width, self.height, self.fps, self.fourcc
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureDeviceInfo {
    pub id: usize,
    pub name: String,
    pub path: String,
    pub modes: Vec<CaptureMode>,
    pub probe_error: Option<String>,
}

impl CaptureDeviceInfo {
    pub fn default_mode(&self) -> Option<usize> {
        self.modes
            .iter()
            .enumerate()
            .max_by_key(|(_, mode)| (mode.width * mode.height, mode.fps))
            .map(|(index, _)| index)
    }
}

#[derive(Clone, Debug)]
pub struct SelectedCaptureCard {
    pub device: CaptureDeviceInfo,
    pub mode_index: usize,
}

impl SelectedCaptureCard {
    pub fn mode(&self) -> &CaptureMode {
        &self.device.modes[self.mode_index]
    }
}

#[derive(Clone, Debug)]
pub struct CalibrationRequest {
    pub screen: ScreenInfo,
    pub layout: Layout,
    pub cards: Vec<SelectedCaptureCard>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AxisFit {
    pub scale: f64,
    pub offset: f64,
    pub rmse: f64,
    pub points: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransformMeasurement {
    pub nominal_scale_x: f64,
    pub nominal_scale_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub stretch_x_pct: f64,
    pub stretch_y_pct: f64,
    pub offset_x_px: f64,
    pub offset_y_px: f64,
    pub source_offset_x_px: f64,
    pub source_offset_y_px: f64,
    pub fit_rmse_px: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CardAnalysis {
    pub device_id: usize,
    pub device_name: String,
    pub device_path: String,
    pub native_size: PixelSize,
    pub requested_size: PixelSize,
    pub assigned_tile: usize,
    pub assigned_letter: char,
    pub detected_letter: Option<char>,
    pub ocr_confidence: f64,
    pub rotation_degrees: u16,
    pub marker_count: usize,
    pub transform: Option<TransformMeasurement>,
    pub temporal_jitter_x_px: f64,
    pub temporal_jitter_y_px: f64,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub schema_version: u32,
    pub generated_unix_ms: u64,
    pub source_screen: String,
    pub source_size: PixelSize,
    pub layout: LayoutReport,
    pub cards: Vec<CardAnalysis>,
    pub median_stretch_x_pct: f64,
    pub median_stretch_y_pct: f64,
    pub median_offset_x_px: f64,
    pub median_offset_y_px: f64,
    pub max_stretch_deviation_pct: f64,
    pub max_offset_px: f64,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayoutReport {
    pub rows: usize,
    pub cols: usize,
    pub source_tile_width: u32,
    pub source_tile_height: u32,
}

pub fn supported_layouts(card_count: usize) -> Vec<Layout> {
    if card_count == 0 {
        return Vec::new();
    }

    let mut layouts = Vec::new();
    for cols in 1..=card_count {
        if !card_count.is_multiple_of(cols) {
            continue;
        }
        let rows = card_count / cols;
        layouts.push(Layout::new(rows, cols));
    }

    layouts.sort_by_key(|layout| {
        let difference = layout.cols.abs_diff(layout.rows);
        let portrait_penalty = usize::from(layout.rows > layout.cols);
        (difference, portrait_penalty, layout.rows)
    });
    layouts
}

pub fn validate_calibration(request: &CalibrationRequest) -> Result<(), String> {
    let expected_cards = request.layout.card_count();
    if request.cards.len() != expected_cards {
        return Err(format!(
            "拼接模式需要 {expected_cards} 张采集卡，当前选择了 {} 张。",
            request.cards.len()
        ));
    }
    if request.cards.is_empty() || request.cards.len() > MAX_CARDS {
        return Err(format!("采集卡数量必须在 1 到 {MAX_CARDS} 之间。"));
    }
    if request.screen.size.width == 0 || request.screen.size.height == 0 {
        return Err("没有可用的 DXI 屏幕分辨率。".into());
    }
    if !request
        .screen
        .size
        .width
        .is_multiple_of(request.layout.cols as u32)
        || !request
            .screen
            .size
            .height
            .is_multiple_of(request.layout.rows as u32)
    {
        return Err(format!(
            "DXI 画面 {}×{} 无法均分为 {} 个区块，请选择正确的拼接模式。",
            request.screen.size.width, request.screen.size.height, expected_cards
        ));
    }

    let tile_width = request.screen.size.width / request.layout.cols as u32;
    let tile_height = request.screen.size.height / request.layout.rows as u32;

    let mut common_size = None;
    for card in &request.cards {
        let mode = card.mode();
        if mode.width == 0 || mode.height == 0 {
            return Err(format!("{} 没有可用的采集格式。", card.device.name));
        }
        if let Some(size) = common_size {
            if size != PixelSize::new(mode.width, mode.height) {
                return Err("当前版本要求所有比对采集卡使用相同输出分辨率。".into());
            }
        } else {
            common_size = Some(PixelSize::new(mode.width, mode.height));
        }
        if !tile_width.is_multiple_of(mode.width) || !tile_height.is_multiple_of(mode.height) {
            return Err(format!(
                "错误的拼接模式：DXI 单块 {}×{} 与 {} 汇报的 {}×{} 不能形成整数比例。",
                tile_width, tile_height, card.device.name, mode.width, mode.height
            ));
        }
    }

    Ok(())
}

pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) * 0.5
    } else {
        sorted[mid]
    }
}

pub fn standard_deviation(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>()
        / (values.len() - 1) as f64;
    variance.sqrt()
}

pub fn unix_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_balanced_and_complete() {
        assert_eq!(supported_layouts(4).len(), 3);
        assert_eq!(supported_layouts(4)[0], Layout::new(2, 2));
        assert_eq!(
            supported_layouts(7),
            vec![Layout::new(1, 7), Layout::new(7, 1)]
        );
        assert!(supported_layouts(0).is_empty());
    }

    #[test]
    fn median_and_deviation_are_stable() {
        assert_eq!(median(&[9.0, 1.0, 4.0]), 4.0);
        assert_eq!(median(&[1.0, 2.0, 8.0, 10.0]), 5.0);
        assert!((standard_deviation(&[2.0, 2.0, 2.0]) - 0.0).abs() < 1e-9);
    }
}
