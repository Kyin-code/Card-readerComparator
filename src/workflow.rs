use std::{path::PathBuf, sync::mpsc, thread, time::Duration};

use image::RgbaImage;
use serde_json::json;

use crate::{
    analysis::{
        OrientationDetection, Stability, detect_best_orientation, detect_orientation,
        stability_for_offsets,
    },
    capture::{capture_card_frames, capture_dxi_frame, downscale_for_analysis, encode_png},
    model::{
        CalibrationRequest, CardAnalysis, ComparisonReport, LayoutReport, PixelSize, median,
        unix_time_millis,
    },
    pattern::letter_for_index,
};

#[derive(Clone, Debug)]
pub struct CardCaptureOutcome {
    pub device_id: usize,
    pub device_name: String,
    pub device_path: String,
    pub requested_size: PixelSize,
    pub native_size: PixelSize,
    pub preview_png: Vec<u8>,
    pub orientation_previews: Vec<Vec<u8>>,
    pub orientations: Vec<OrientationDetection>,
    pub best_orientation_index: usize,
    pub stability: Stability,
}

impl CardCaptureOutcome {
    pub fn best(&self) -> &OrientationDetection {
        &self.orientations[self.best_orientation_index]
    }

    pub fn detection_for_rotation(&self, rotation_degrees: u16) -> &OrientationDetection {
        self.orientations
            .iter()
            .find(|detection| detection.rotation_degrees == rotation_degrees)
            .unwrap_or_else(|| self.best())
    }

    pub fn preview_for_rotation(&self, rotation_degrees: u16) -> &[u8] {
        let index = self
            .orientations
            .iter()
            .position(|detection| detection.rotation_degrees == rotation_degrees % 360)
            .unwrap_or(self.best_orientation_index);
        self.orientation_previews
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&self.preview_png)
    }
}

#[derive(Clone, Debug)]
pub struct CalibrationOutcome {
    pub cards: Vec<CardCaptureOutcome>,
    pub source_preview_png: Option<Vec<u8>>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CardMapping {
    pub tile_index: usize,
    pub rotation_degrees: u16,
}

#[derive(Clone, Debug)]
pub enum WorkerMessage {
    Progress(String),
    Complete(Box<CalibrationOutcome>),
    Failed(String),
}

pub fn start_calibration_worker(
    request: CalibrationRequest,
    sender: mpsc::Sender<WorkerMessage>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("capture-comparator-worker".into())
        .spawn(move || {
            let result = run_calibration(&request, &sender);
            match result {
                Ok(outcome) => {
                    let _ = sender.send(WorkerMessage::Complete(Box::new(outcome)));
                }
                Err(error) => {
                    let _ = sender.send(WorkerMessage::Failed(error));
                }
            }
        })
        .expect("failed to start capture worker")
}

pub fn build_report(
    request: &CalibrationRequest,
    outcome: &CalibrationOutcome,
    mappings: &[CardMapping],
) -> Result<ComparisonReport, String> {
    let mut cards = Vec::with_capacity(outcome.cards.len());
    for (capture, mapping) in outcome.cards.iter().zip(mappings) {
        if mapping.tile_index >= request.layout.card_count() {
            return Err("物理位置超出当前拼接模式".into());
        }
        let detection = capture.detection_for_rotation(mapping.rotation_degrees);
        let transform = detection
            .transform
            .clone()
            .ok_or_else(|| format!("{} 的定位标不足，无法计算位移", capture.device_name))?;
        let assigned_letter = letter_for_index(mapping.tile_index);
        let mut notes = detection.notes.clone();
        if detection.letter != Some(assigned_letter) {
            notes.push("物理位置由用户手动确认/修正".into());
        }
        if mapping.rotation_degrees != 0 {
            notes.push(format!("分析前顺时针校正 {}°", mapping.rotation_degrees));
        }
        cards.push(CardAnalysis {
            device_id: capture.device_id,
            device_name: capture.device_name.clone(),
            device_path: capture.device_path.clone(),
            native_size: capture.native_size,
            requested_size: capture.requested_size,
            assigned_tile: mapping.tile_index,
            assigned_letter,
            detected_letter: detection.letter,
            ocr_confidence: detection.ocr_confidence,
            rotation_degrees: mapping.rotation_degrees,
            marker_count: detection.marker_count,
            transform: Some(transform),
            temporal_jitter_x_px: capture.stability.jitter_x_px,
            temporal_jitter_y_px: capture.stability.jitter_y_px,
            notes,
        });
    }
    cards.sort_by_key(|card| card.assigned_tile);

    let transforms = cards
        .iter()
        .filter_map(|card| card.transform.as_ref())
        .collect::<Vec<_>>();
    if transforms.len() != cards.len() {
        return Err("至少一张采集卡没有有效的拉伸/偏移结果".into());
    }
    let median_stretch_x = median(
        &transforms
            .iter()
            .map(|transform| transform.stretch_x_pct)
            .collect::<Vec<_>>(),
    );
    let median_stretch_y = median(
        &transforms
            .iter()
            .map(|transform| transform.stretch_y_pct)
            .collect::<Vec<_>>(),
    );
    let median_offset_x = median(
        &transforms
            .iter()
            .map(|transform| transform.offset_x_px)
            .collect::<Vec<_>>(),
    );
    let median_offset_y = median(
        &transforms
            .iter()
            .map(|transform| transform.offset_y_px)
            .collect::<Vec<_>>(),
    );
    let max_stretch_deviation = transforms
        .iter()
        .flat_map(|transform| {
            [
                (transform.stretch_x_pct - median_stretch_x).abs(),
                (transform.stretch_y_pct - median_stretch_y).abs(),
            ]
        })
        .fold(0.0, f64::max);
    let max_offset = transforms
        .iter()
        .map(|transform| {
            let x = transform.offset_x_px - median_offset_x;
            let y = transform.offset_y_px - median_offset_y;
            (x * x + y * y).sqrt()
        })
        .fold(0.0, f64::max);

    Ok(ComparisonReport {
        schema_version: 1,
        generated_unix_ms: unix_time_millis(),
        source_screen: format!(
            "DXI {:08X} @ ({}, {})",
            request.screen.id, request.screen.x, request.screen.y
        ),
        source_size: request.screen.size,
        layout: LayoutReport {
            rows: request.layout.rows,
            cols: request.layout.cols,
            source_tile_width: request.screen.size.width / request.layout.cols as u32,
            source_tile_height: request.screen.size.height / request.layout.rows as u32,
        },
        cards,
        median_stretch_x_pct: median_stretch_x,
        median_stretch_y_pct: median_stretch_y,
        median_offset_x_px: median_offset_x,
        median_offset_y_px: median_offset_y,
        max_stretch_deviation_pct: max_stretch_deviation,
        max_offset_px: max_offset,
        notes: outcome.warnings.clone(),
    })
}

pub fn export_report(
    report: &ComparisonReport,
    outcome: &CalibrationOutcome,
    mappings: &[CardMapping],
) -> Result<PathBuf, String> {
    let folder = report_root().join(format!("compare-{}", report.generated_unix_ms));
    std::fs::create_dir_all(&folder).map_err(|error| format!("创建报告目录失败: {error}"))?;

    let json =
        serde_json::to_vec_pretty(report).map_err(|error| format!("序列化 JSON 失败: {error}"))?;
    std::fs::write(folder.join("comparison.json"), json)
        .map_err(|error| format!("写入 JSON 失败: {error}"))?;
    std::fs::write(folder.join("comparison.csv"), report_to_csv(report))
        .map_err(|error| format!("写入 CSV 失败: {error}"))?;

    if let Some(source) = &outcome.source_preview_png {
        std::fs::write(folder.join("dxi-source.png"), source)
            .map_err(|error| format!("写入 DXI 截图失败: {error}"))?;
    }
    for (index, (card, mapping)) in outcome.cards.iter().zip(mappings).enumerate() {
        let assigned = letter_for_index(mapping.tile_index);
        let file_name = format!(
            "card-{}-{}-{}.png",
            index + 1,
            assigned,
            sanitize_file_component(&card.device_name)
        );
        std::fs::write(folder.join(file_name), &card.preview_png)
            .map_err(|error| format!("写入采集卡截图失败: {error}"))?;
    }

    let manifest = json!({
        "report": "comparison.json",
        "csv": "comparison.csv",
        "sourcePreview": outcome.source_preview_png.as_ref().map(|_| "dxi-source.png"),
        "cardPreviews": outcome.cards.len(),
    });
    std::fs::write(
        folder.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|error| format!("序列化清单失败: {error}"))?,
    )
    .map_err(|error| format!("写入清单失败: {error}"))?;
    Ok(folder)
}

pub fn report_to_csv(report: &ComparisonReport) -> String {
    let mut csv = String::from(
        "assigned_letter,device,device_path,native_width,native_height,stretch_x_pct,stretch_y_pct,offset_x_px,offset_y_px,source_offset_x_px,source_offset_y_px,rotation,ocr_confidence,marker_count,jitter_x_px,jitter_y_px\n",
    );
    for card in &report.cards {
        let transform = card.transform.as_ref();
        csv.push_str(&format!(
            "{},{},{},{},{},{:.5},{:.5},{:.4},{:.4},{:.4},{:.4},{},{:.4},{},{:.4},{:.4}\n",
            card.assigned_letter,
            csv_escape(&card.device_name),
            csv_escape(&card.device_path),
            card.native_size.width,
            card.native_size.height,
            transform.map(|value| value.stretch_x_pct).unwrap_or(0.0),
            transform.map(|value| value.stretch_y_pct).unwrap_or(0.0),
            transform.map(|value| value.offset_x_px).unwrap_or(0.0),
            transform.map(|value| value.offset_y_px).unwrap_or(0.0),
            transform
                .map(|value| value.source_offset_x_px)
                .unwrap_or(0.0),
            transform
                .map(|value| value.source_offset_y_px)
                .unwrap_or(0.0),
            card.rotation_degrees,
            card.ocr_confidence,
            card.marker_count,
            card.temporal_jitter_x_px,
            card.temporal_jitter_y_px,
        ));
    }
    csv
}

fn run_calibration(
    request: &CalibrationRequest,
    sender: &mpsc::Sender<WorkerMessage>,
) -> Result<CalibrationOutcome, String> {
    progress(sender, "等待全屏标定图案稳定...");
    thread::sleep(Duration::from_millis(700));

    let mut warnings = Vec::new();
    let dxi_frame = match capture_dxi_frame(request.screen.id) {
        Ok(frame) => frame,
        Err(error) => {
            warnings.push(format!("DXI 回读失败（仍继续比较采集卡）: {error}"));
            RgbaImage::new(1, 1)
        }
    };
    let (source_preview_png, source_size) = if dxi_frame.width() > 1 {
        let white_ratio = bright_pixel_ratio(&dxi_frame);
        if white_ratio < 0.002 {
            warnings.push(format!(
                "DXI 回读中亮像素仅 {:.3}%，全屏图案可能未正常显示",
                white_ratio * 100.0
            ));
        }
        let preview = downscale_to(&dxi_frame, 1280);
        (
            Some(encode_png(&preview)?),
            PixelSize::new(dxi_frame.width(), dxi_frame.height()),
        )
    } else {
        (None, request.screen.size)
    };
    if source_size != request.screen.size {
        return Err(format!(
            "DXI 分辨率在校准期间发生变化（记录为 {}×{}，实际回读 {}×{}）",
            request.screen.size.width,
            request.screen.size.height,
            source_size.width,
            source_size.height
        ));
    }

    let mut cards = Vec::with_capacity(request.cards.len());
    for (index, card) in request.cards.iter().enumerate() {
        progress(
            sender,
            format!(
                "读取采集卡 {}/{} · {}",
                index + 1,
                request.cards.len(),
                card.device.name
            ),
        );
        let mode = card.mode();
        let frames = capture_card_frames(card.device.id, mode, 3)
            .map_err(|error| format!("{}: {error}", card.device.name))?;
        let native = frames
            .first()
            .map(|frame| PixelSize::new(frame.width(), frame.height()))
            .ok_or_else(|| format!("{} 没有返回帧", card.device.name))?;
        if native != PixelSize::new(mode.width, mode.height) {
            return Err(format!(
                "{} 实际返回 {}×{}，与选择的 {}×{} 不一致",
                card.device.name, native.width, native.height, mode.width, mode.height
            ));
        }
        let analysis_frames = frames
            .iter()
            .map(downscale_for_analysis)
            .collect::<Vec<_>>();
        let latest = analysis_frames
            .last()
            .ok_or_else(|| format!("{} 没有可分析帧", card.device.name))?;
        let best = detect_best_orientation(latest);
        let mut orientations = Vec::with_capacity(4);
        let mut orientation_previews = Vec::with_capacity(4);
        for rotation in [0_u16, 90, 180, 270] {
            orientations.push(detect_orientation(latest, rotation));
            let upright = crate::analysis::rotate_frame(latest, rotation);
            orientation_previews.push(encode_png(&downscale_to(&upright, 640))?);
        }
        let best_orientation_index = orientations
            .iter()
            .position(|candidate| candidate.rotation_degrees == best.rotation_degrees)
            .unwrap_or(0);

        let mut x_offsets = Vec::new();
        let mut y_offsets = Vec::new();
        for frame in &analysis_frames {
            let detection = detect_orientation(frame, best.rotation_degrees);
            if let Some(transform) = detection.transform {
                x_offsets.push(transform.offset_x_px);
                y_offsets.push(transform.offset_y_px);
            }
        }
        let stability = stability_for_offsets(&x_offsets, &y_offsets);
        let preview = downscale_to(latest, 640);
        cards.push(CardCaptureOutcome {
            device_id: card.device.id,
            device_name: card.device.name.clone(),
            device_path: card.device.path.clone(),
            requested_size: PixelSize::new(mode.width, mode.height),
            native_size: native,
            preview_png: encode_png(&preview)?,
            orientation_previews,
            orientations,
            best_orientation_index,
            stability,
        });
    }

    Ok(CalibrationOutcome {
        cards,
        source_preview_png,
        warnings,
    })
}

fn progress(sender: &mpsc::Sender<WorkerMessage>, message: impl Into<String>) {
    let _ = sender.send(WorkerMessage::Progress(message.into()));
}

fn bright_pixel_ratio(frame: &RgbaImage) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let bright = frame
        .pixels()
        .filter(|pixel| pixel[0].max(pixel[1]).max(pixel[2]) >= 160)
        .count();
    bright as f64 / (frame.width() * frame.height()) as f64
}

fn downscale_to(frame: &RgbaImage, max_side: u32) -> RgbaImage {
    let longest = frame.width().max(frame.height());
    if longest <= max_side {
        return frame.clone();
    }
    let scale = max_side as f64 / longest as f64;
    image::imageops::resize(
        frame,
        (frame.width() as f64 * scale).round().max(1.0) as u32,
        (frame.height() as f64 * scale).round().max(1.0) as u32,
        image::imageops::FilterType::Triangle,
    )
}

fn sanitize_file_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    sanitized.chars().take(48).collect()
}

fn csv_escape(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn report_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("CardReaderComparator")
        .join("reports")
}

#[cfg(test)]
mod tests {
    use crate::{
        analysis::{OrientationDetection, Stability},
        model::{
            CaptureDeviceInfo, CaptureMode, Layout, ScreenInfo, SelectedCaptureCard,
            TransformMeasurement,
        },
    };

    use super::*;

    fn detection(
        letter: char,
        stretch_x: f64,
        stretch_y: f64,
        offset_x: f64,
        offset_y: f64,
    ) -> OrientationDetection {
        OrientationDetection {
            rotation_degrees: 0,
            letter: Some(letter),
            ocr_confidence: 0.98,
            marker_count: 4,
            transform: Some(TransformMeasurement {
                nominal_scale_x: 1920.0,
                nominal_scale_y: 1080.0,
                scale_x: 1920.0 * (1.0 + stretch_x / 100.0),
                scale_y: 1080.0 * (1.0 + stretch_y / 100.0),
                stretch_x_pct: stretch_x,
                stretch_y_pct: stretch_y,
                offset_x_px: offset_x,
                offset_y_px: offset_y,
                source_offset_x_px: offset_x / (1.0 + stretch_x / 100.0),
                source_offset_y_px: offset_y / (1.0 + stretch_y / 100.0),
                fit_rmse_px: 0.2,
            }),
            notes: Vec::new(),
        }
    }

    fn request_with_four_cards() -> CalibrationRequest {
        let device = CaptureDeviceInfo {
            id: 7,
            name: "Capture 4K".into(),
            path: r"\\?\usb#vid_0001&pid_0001".into(),
            modes: vec![CaptureMode {
                width: 1920,
                height: 1080,
                fps: 60,
                fourcc: "MJPEG".into(),
            }],
            probe_error: None,
        };
        CalibrationRequest {
            screen: ScreenInfo {
                id: 1,
                x: 0,
                y: 0,
                size: PixelSize::new(3840, 2160),
                primary: true,
                refresh_hz: 60,
            },
            layout: Layout::new(2, 2),
            cards: (0..4)
                .map(|_| SelectedCaptureCard {
                    device: device.clone(),
                    mode_index: 0,
                })
                .collect(),
        }
    }

    fn outcome_for(detections: Vec<OrientationDetection>) -> CalibrationOutcome {
        CalibrationOutcome {
            cards: detections
                .into_iter()
                .enumerate()
                .map(|(index, detection)| CardCaptureOutcome {
                    device_id: index,
                    device_name: format!("Capture {index}"),
                    device_path: format!(r"\\?\device#{index}"),
                    requested_size: PixelSize::new(1920, 1080),
                    native_size: PixelSize::new(1920, 1080),
                    preview_png: Vec::new(),
                    orientation_previews: vec![Vec::new()],
                    orientations: vec![detection],
                    best_orientation_index: 0,
                    stability: Stability {
                        jitter_x_px: 0.1,
                        jitter_y_px: 0.2,
                    },
                })
                .collect(),
            source_preview_png: None,
            warnings: vec!["synthetic".into()],
        }
    }

    #[test]
    fn builds_cross_card_report_from_mapped_letters() {
        let request = request_with_four_cards();
        let outcome = outcome_for(vec![
            detection('A', 0.10, -0.10, 1.0, 2.0),
            detection('B', 0.20, -0.20, 3.0, 4.0),
            detection('C', 0.30, -0.30, 5.0, 6.0),
            detection('D', 0.40, -0.40, 7.0, 8.0),
        ]);
        let mappings = (0..4)
            .map(|tile_index| CardMapping {
                tile_index,
                rotation_degrees: 0,
            })
            .collect::<Vec<_>>();
        let report = build_report(&request, &outcome, &mappings).unwrap();
        assert_eq!(report.cards.len(), 4);
        assert_eq!(
            report
                .cards
                .iter()
                .map(|card| card.assigned_letter)
                .collect::<String>(),
            "ABCD"
        );
        assert!((report.median_stretch_x_pct - 0.25).abs() < 1e-9);
        assert!((report.median_offset_x_px - 4.0).abs() < 1e-9);
        let csv = report_to_csv(&report);
        assert!(csv.contains("assigned_letter,device"));
        assert!(csv.contains(r#""Capture 0""#));
    }
}
