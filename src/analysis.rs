use std::collections::VecDeque;

use image::{RgbaImage, imageops};

use crate::{
    model::{AxisFit, Point, Rect, TransformMeasurement, median, standard_deviation},
    pattern::{glyph_template, marker_anchors},
};

const MARKER_WHITE_THRESHOLD: u8 = 150;
const MAX_ANALYSIS_SIDE: u32 = 1600;
const MIN_OCR_CONFIDENCE: f64 = 0.58;

#[derive(Clone, Debug)]
pub struct OrientationDetection {
    pub rotation_degrees: u16,
    pub letter: Option<char>,
    pub ocr_confidence: f64,
    pub marker_count: usize,
    pub transform: Option<TransformMeasurement>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Stability {
    pub jitter_x_px: f64,
    pub jitter_y_px: f64,
}

#[derive(Clone, Debug)]
struct Component {
    area: u64,
    bbox: Rect,
    centroid: Point,
}

#[derive(Clone, Debug)]
struct MarkerMatch {
    observed_center: Point,
    expected_center: Point,
    component: Component,
}

pub fn detect_best_orientation(frame: &RgbaImage) -> OrientationDetection {
    let rotations = [0_u16, 90, 180, 270];
    let mut best = detect_orientation(frame, 0);

    for rotation in rotations.into_iter().skip(1) {
        let candidate = detect_orientation(frame, rotation);
        if detection_score(&candidate) > detection_score(&best) {
            best = candidate;
        }
    }

    best
}

pub fn detect_orientation(frame: &RgbaImage, rotation_degrees: u16) -> OrientationDetection {
    let rotated = rotate_frame(frame, rotation_degrees);
    let (analysis_image, scale) = analysis_image(&rotated);
    let (width, height) = analysis_image.dimensions();
    let mut notes = Vec::new();

    if width < 48 || height < 48 {
        return OrientationDetection {
            rotation_degrees,
            letter: None,
            ocr_confidence: 0.0,
            marker_count: 0,
            transform: None,
            notes: vec!["采集画面太小，无法识别标定图案".into()],
        };
    }

    let gray = grayscale(&analysis_image);
    let mask = threshold_mask(&gray, MARKER_WHITE_THRESHOLD);
    let components = connected_components(&mask, width, height);
    let expected_side = marker_anchors()[0].normalized_side * width.min(height) as f64;
    let marker_candidates = marker_candidates(&components, expected_side);
    let marker_matches = match_markers(&marker_candidates, width, height);
    let (fitted, transform) = fit_transform(&marker_matches, width, height, scale);
    let (letter, ocr_confidence, ocr_note) =
        recognize_letter(&mask, width, height, fitted.as_ref(), &marker_matches);

    if marker_matches.len() < 4 {
        notes.push(format!(
            "仅找到 {}/4 个定位标；缺失定位标会降低缩放测量可信度",
            marker_matches.len()
        ));
    }
    if let Some(note) = ocr_note {
        notes.push(note);
    }

    OrientationDetection {
        rotation_degrees,
        letter,
        ocr_confidence,
        marker_count: marker_matches.len(),
        transform,
        notes,
    }
}

pub fn stability_for_offsets(x_offsets: &[f64], y_offsets: &[f64]) -> Stability {
    let center_x = median(x_offsets);
    let center_y = median(y_offsets);
    let jitter_x = standard_deviation(
        &x_offsets
            .iter()
            .map(|value| value - center_x)
            .collect::<Vec<_>>(),
    );
    let jitter_y = standard_deviation(
        &y_offsets
            .iter()
            .map(|value| value - center_y)
            .collect::<Vec<_>>(),
    );
    Stability {
        jitter_x_px: jitter_x,
        jitter_y_px: jitter_y,
    }
}

pub fn rotate_frame(frame: &RgbaImage, rotation_degrees: u16) -> RgbaImage {
    match rotation_degrees % 360 {
        90 => imageops::rotate90(frame),
        180 => imageops::rotate180(frame),
        270 => imageops::rotate270(frame),
        _ => frame.clone(),
    }
}

fn detection_score(detection: &OrientationDetection) -> f64 {
    let marker_score = detection.marker_count as f64 / 4.0;
    let transform_score = detection
        .transform
        .as_ref()
        .map(|transform| (1.0 - transform.fit_rmse_px / 20.0).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    detection.ocr_confidence * 0.65 + marker_score * 0.2 + transform_score * 0.15
}

fn analysis_image(frame: &RgbaImage) -> (RgbaImage, f64) {
    let longest = frame.width().max(frame.height());
    if longest <= MAX_ANALYSIS_SIDE {
        return (frame.clone(), 1.0);
    }
    let scale = MAX_ANALYSIS_SIDE as f64 / longest as f64;
    let width = (frame.width() as f64 * scale).round().max(1.0) as u32;
    let height = (frame.height() as f64 * scale).round().max(1.0) as u32;
    (
        imageops::resize(frame, width, height, imageops::FilterType::Triangle),
        scale,
    )
}

fn grayscale(image: &RgbaImage) -> Vec<u8> {
    image
        .pixels()
        .map(|pixel| {
            ((pixel[0] as u32 * 77 + pixel[1] as u32 * 150 + pixel[2] as u32 * 29) >> 8) as u8
        })
        .collect()
}

fn threshold_mask(gray: &[u8], threshold: u8) -> Vec<u8> {
    gray.iter()
        .map(|value| u8::from(*value >= threshold))
        .collect()
}

fn connected_components(mask: &[u8], width: u32, height: u32) -> Vec<Component> {
    let mut visited = vec![false; mask.len()];
    let mut components = Vec::new();
    let mut queue = VecDeque::new();

    for start in 0..mask.len() {
        if mask[start] == 0 || visited[start] {
            continue;
        }

        visited[start] = true;
        queue.push_back(start);
        let mut area = 0_u64;
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0_u32;
        let mut max_y = 0_u32;
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;

        while let Some(index) = queue.pop_front() {
            let x = index as u32 % width;
            let y = index as u32 / width;
            area += 1;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            sum_x += x as f64;
            sum_y += y as f64;

            let x0 = x.saturating_sub(1);
            let x1 = (x + 1).min(width - 1);
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(height - 1);
            for ny in y0..=y1 {
                for nx in x0..=x1 {
                    let next = (ny * width + nx) as usize;
                    if mask[next] != 0 && !visited[next] {
                        visited[next] = true;
                        queue.push_back(next);
                    }
                }
            }
        }

        if area >= 12 {
            let w = (max_x - min_x + 1) as f64;
            let h = (max_y - min_y + 1) as f64;
            components.push(Component {
                area,
                bbox: Rect::new(min_x as f64, min_y as f64, w, h),
                centroid: Point::new(sum_x / area as f64, sum_y / area as f64),
            });
        }
    }

    components
}

fn marker_candidates(components: &[Component], expected_side: f64) -> Vec<Component> {
    let expected_area = expected_side * expected_side;
    components
        .iter()
        .filter(|component| {
            let width = component.bbox.width;
            let height = component.bbox.height;
            let ratio = width / height.max(1.0);
            let fill = component.area as f64 / (width * height).max(1.0);
            component.area as f64 >= expected_area * 0.12
                && component.area as f64 <= expected_area * 5.5
                && width >= expected_side * 0.25
                && height >= expected_side * 0.25
                && width <= expected_side * 7.0
                && height <= expected_side * 7.0
                && (0.22..=4.5).contains(&ratio)
                && fill >= 0.42
        })
        .cloned()
        .collect()
}

fn match_markers(candidates: &[Component], width: u32, height: u32) -> Vec<MarkerMatch> {
    let anchors = marker_anchors();
    let min_side = width.min(height) as f64;
    let max_distance = min_side * 0.24;
    let mut pairs = Vec::new();

    for (anchor_index, anchor) in anchors.iter().enumerate() {
        let expected = Point::new(
            anchor.normalized_center.x * width as f64,
            anchor.normalized_center.y * height as f64,
        );
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let dx = candidate.centroid.x - expected.x;
            let dy = candidate.centroid.y - expected.y;
            let distance_sq = dx * dx + dy * dy;
            if distance_sq <= max_distance * max_distance {
                pairs.push((distance_sq, anchor_index, candidate_index, expected));
            }
        }
    }

    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut used_anchors = [false; 4];
    let mut used_candidates = vec![false; candidates.len()];
    let mut matches = Vec::new();
    for (_, anchor_index, candidate_index, expected) in pairs {
        if used_anchors[anchor_index] || used_candidates[candidate_index] {
            continue;
        }
        used_anchors[anchor_index] = true;
        used_candidates[candidate_index] = true;
        let component = candidates[candidate_index].clone();
        matches.push(MarkerMatch {
            observed_center: component.centroid,
            expected_center: expected,
            component,
        });
    }
    matches
}

fn fit_transform(
    markers: &[MarkerMatch],
    width: u32,
    height: u32,
    analysis_scale: f64,
) -> (Option<(AxisFit, AxisFit)>, Option<TransformMeasurement>) {
    if markers.len() < 2 {
        return (None, None);
    }

    let mut x_points = markers
        .iter()
        .map(|marker| {
            (
                marker.expected_center.x / width as f64,
                marker.observed_center.x,
            )
        })
        .collect::<Vec<_>>();
    let mut y_points = markers
        .iter()
        .map(|marker| {
            (
                marker.expected_center.y / height as f64,
                marker.observed_center.y,
            )
        })
        .collect::<Vec<_>>();
    let tolerance = width.min(height) as f64 * 0.012;
    let mut x_fit = fit_axis(&x_points);
    let mut y_fit = fit_axis(&y_points);

    if markers.len() >= 3
        && let (Some(x), Some(y)) = (&x_fit, &y_fit)
    {
        x_points
            .retain(|(input, observed)| (x.scale * input + x.offset - observed).abs() <= tolerance);
        y_points
            .retain(|(input, observed)| (y.scale * input + y.offset - observed).abs() <= tolerance);
        if x_points.len() >= 2 {
            x_fit = fit_axis(&x_points);
        }
        if y_points.len() >= 2 {
            y_fit = fit_axis(&y_points);
        }
    }

    match (x_fit, y_fit) {
        (Some(x), Some(y)) => {
            let nominal_x = width as f64 / analysis_scale;
            let nominal_y = height as f64 / analysis_scale;
            let scale_x = x.scale / analysis_scale;
            let scale_y = y.scale / analysis_scale;
            let offset_x_px = (x.offset + x.scale * 0.5 - (width as f64 * 0.5)) / analysis_scale;
            let offset_y_px = (y.offset + y.scale * 0.5 - (height as f64 * 0.5)) / analysis_scale;
            let scale_ratio_x = scale_x / nominal_x.max(1e-9);
            let scale_ratio_y = scale_y / nominal_y.max(1e-9);
            let transform = TransformMeasurement {
                nominal_scale_x: nominal_x,
                nominal_scale_y: nominal_y,
                scale_x,
                scale_y,
                stretch_x_pct: (scale_ratio_x - 1.0) * 100.0,
                stretch_y_pct: (scale_ratio_y - 1.0) * 100.0,
                offset_x_px,
                offset_y_px,
                source_offset_x_px: if scale_ratio_x.abs() > 1e-9 {
                    offset_x_px / scale_ratio_x
                } else {
                    0.0
                },
                source_offset_y_px: if scale_ratio_y.abs() > 1e-9 {
                    offset_y_px / scale_ratio_y
                } else {
                    0.0
                },
                fit_rmse_px: (x.rmse * x.rmse + y.rmse * y.rmse).sqrt() / analysis_scale,
            };
            (Some((x, y)), Some(transform))
        }
        _ => (None, None),
    }
}

fn fit_axis(points: &[(f64, f64)]) -> Option<AxisFit> {
    if points.len() < 2 {
        return None;
    }
    let count = points.len() as f64;
    let mean_x = points.iter().map(|point| point.0).sum::<f64>() / count;
    let mean_y = points.iter().map(|point| point.1).sum::<f64>() / count;
    let denominator = points
        .iter()
        .map(|point| {
            let dx = point.0 - mean_x;
            dx * dx
        })
        .sum::<f64>();
    if denominator.abs() < 1e-9 {
        return None;
    }
    let numerator = points
        .iter()
        .map(|point| (point.0 - mean_x) * (point.1 - mean_y))
        .sum::<f64>();
    let scale = numerator / denominator;
    let offset = mean_y - scale * mean_x;
    let mse = points
        .iter()
        .map(|(input, observed)| {
            let residual = observed - (scale * input + offset);
            residual * residual
        })
        .sum::<f64>()
        / count;

    Some(AxisFit {
        scale,
        offset,
        rmse: mse.sqrt(),
        points: points.len(),
    })
}

fn recognize_letter(
    mask: &[u8],
    width: u32,
    height: u32,
    fitted: Option<&(AxisFit, AxisFit)>,
    markers: &[MarkerMatch],
) -> (Option<char>, f64, Option<String>) {
    let glyph_rect = if let Some((x_fit, y_fit)) = fitted {
        predicted_glyph_rect(width, height, x_fit, y_fit)
    } else {
        let mut filtered_mask = mask.to_vec();
        remove_marker_areas(
            &mut filtered_mask,
            width,
            height,
            markers,
            width.min(height) as f64 * 0.045,
        );
        let components = connected_components(&filtered_mask, width, height);
        let Some(rect) = glyph_bounds_from_components(&components, width, height) else {
            return (
                None,
                0.0,
                Some("定位标之间的标定字无法分割，请先摆正采集画面".into()),
            );
        };
        rect
    };

    if glyph_rect.width < 30.0 || glyph_rect.height < 30.0 {
        return (
            None,
            0.0,
            Some("标定字太小或被裁切，OCR 无法可靠识别".into()),
        );
    }

    let mut scores = Vec::with_capacity(7);
    for glyph_index in 0..7 {
        let template = glyph_template(glyph_index);
        let mut mismatches = 0_u32;
        for (row, template_row) in template.iter().enumerate() {
            for col in 0..5 {
                let expected_on = template_row.as_bytes()[col] == b'1';
                let center_x = glyph_rect.x + (col as f64 + 0.5) * glyph_rect.width / 5.0;
                let center_y = glyph_rect.y + (row as f64 + 0.5) * glyph_rect.height / 7.0;
                let radius_x = glyph_rect.width / 5.0 * 0.24;
                let radius_y = glyph_rect.height / 7.0 * 0.24;
                let occupancy = occupancy_in_rect(
                    mask,
                    width,
                    height,
                    Rect::new(
                        center_x - radius_x,
                        center_y - radius_y,
                        radius_x * 2.0,
                        radius_y * 2.0,
                    ),
                );
                let observed_on = occupancy >= 0.36;
                if expected_on != observed_on {
                    mismatches += 1;
                }
            }
        }
        let agreement = 1.0 - mismatches as f64 / 35.0;
        scores.push((agreement, glyph_index));
    }
    scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (best_score, best_index) = scores[0];
    let second_score = scores.get(1).map(|entry| entry.0).unwrap_or(0.0);
    let confidence =
        (best_score * 0.82 + (best_score - second_score).max(0.0) * 1.8).clamp(0.0, 1.0);

    if confidence < MIN_OCR_CONFIDENCE {
        (
            None,
            confidence,
            Some(format!(
                "OCR 置信度不足（{:.0}%），已转入手动确认",
                confidence * 100.0
            )),
        )
    } else {
        (Some(char::from(b'A' + best_index as u8)), confidence, None)
    }
}

fn predicted_glyph_rect(width: u32, height: u32, x_fit: &AxisFit, y_fit: &AxisFit) -> Rect {
    let max_w: f64 = 0.66;
    let max_h: f64 = 0.66;
    let tile_aspect = width as f64 / height as f64;
    let normalized_width = max_w.min(max_h * (1.0 / tile_aspect) * 5.0 / 7.0);
    let normalized_height = normalized_width * tile_aspect * 7.0 / 5.0;
    let normalized_left = 0.5 - normalized_width * 0.5;
    let normalized_top = 0.5 - normalized_height * 0.5;
    let x0 = x_fit.scale * normalized_left + x_fit.offset;
    let y0 = y_fit.scale * normalized_top + y_fit.offset;
    Rect::new(
        x0,
        y0,
        x_fit.scale * normalized_width,
        y_fit.scale * normalized_height,
    )
}

fn remove_marker_areas(
    mask: &mut [u8],
    width: u32,
    height: u32,
    markers: &[MarkerMatch],
    radius: f64,
) {
    for marker in markers {
        let x0 = (marker.component.bbox.x - radius).max(0.0) as u32;
        let y0 = (marker.component.bbox.y - radius).max(0.0) as u32;
        let x1 = (marker.component.bbox.x + marker.component.bbox.width + radius).min(width as f64)
            as u32;
        let y1 = (marker.component.bbox.y + marker.component.bbox.height + radius)
            .min(height as f64) as u32;
        for y in y0..y1 {
            for x in x0..x1 {
                mask[(y * width + x) as usize] = 0;
            }
        }
    }
}

fn glyph_bounds_from_components(components: &[Component], width: u32, height: u32) -> Option<Rect> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut found = false;
    let frame_area = width as f64 * height as f64;

    for component in components {
        let center = component.bbox.center();
        if component.area as f64 > frame_area * 0.18
            || center.x < width as f64 * 0.04
            || center.x > width as f64 * 0.96
            || center.y < height as f64 * 0.04
            || center.y > height as f64 * 0.96
        {
            continue;
        }
        found = true;
        min_x = min_x.min(component.bbox.x);
        min_y = min_y.min(component.bbox.y);
        max_x = max_x.max(component.bbox.x + component.bbox.width);
        max_y = max_y.max(component.bbox.y + component.bbox.height);
    }

    found.then(|| Rect::new(min_x, min_y, max_x - min_x, max_y - min_y))
}

fn occupancy_in_rect(mask: &[u8], width: u32, height: u32, rect: Rect) -> f64 {
    let x0 = rect.x.floor().max(0.0) as u32;
    let y0 = rect.y.floor().max(0.0) as u32;
    let x1 = (rect.x + rect.width.max(0.0))
        .ceil()
        .clamp(0.0, width as f64) as u32;
    let y1 = (rect.y + rect.height.max(0.0))
        .ceil()
        .clamp(0.0, height as f64) as u32;
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let mut on = 0_u64;
    let mut total = 0_u64;
    for y in y0..y1 {
        for x in x0..x1 {
            total += 1;
            on += u64::from(mask[(y * width + x) as usize] != 0);
        }
    }
    on as f64 / total.max(1) as f64
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use crate::{
        model::{Layout, PixelSize},
        pattern::render_pattern,
    };

    use super::*;

    fn marker_component(x: f64, y: f64, side: f64) -> Component {
        Component {
            area: (side * side) as u64,
            bbox: Rect::new(x - side / 2.0, y - side / 2.0, side, side),
            centroid: Point::new(x, y),
        }
    }

    #[test]
    fn recognizes_all_rendered_letters() {
        let pattern = render_pattern(PixelSize::new(1200, 900), Layout::new(2, 2)).unwrap();
        for (index, tile) in pattern.tiles.iter().enumerate() {
            let mut tile_image: RgbaImage = RgbaImage::new(
                tile.source_rect.width as u32,
                tile.source_rect.height as u32,
            );
            image::imageops::replace(
                &mut tile_image,
                &image::imageops::crop_imm(
                    &pattern.image,
                    tile.source_rect.x as u32,
                    tile.source_rect.y as u32,
                    tile.source_rect.width as u32,
                    tile.source_rect.height as u32,
                )
                .to_image(),
                0,
                0,
            );
            let detection = detect_orientation(&tile_image, 0);
            assert_eq!(
                detection.letter,
                Some(tile.letter),
                "tile {index}: {detection:?}"
            );
            assert_eq!(detection.marker_count, 4);
            let transform = detection.transform.unwrap();
            assert!(transform.stretch_x_pct.abs() < 1.0);
            assert!(transform.stretch_y_pct.abs() < 1.0);
            assert!(transform.offset_x_px.abs() < 1.0);
            assert!(transform.offset_y_px.abs() < 1.0);
        }
    }

    #[test]
    fn detects_translation_before_scaling_order_representation() {
        let pattern = render_pattern(PixelSize::new(600, 700), Layout::new(1, 1)).unwrap();
        let tile = &pattern.tiles[0];
        let source: RgbaImage = image::imageops::crop_imm(
            &pattern.image,
            tile.source_rect.x as u32,
            tile.source_rect.y as u32,
            tile.source_rect.width as u32,
            tile.source_rect.height as u32,
        )
        .to_image();
        let mut image = RgbaImage::new(600, 700);
        for y in 0_u32..700 {
            for x in 0_u32..600 {
                let source_x = 300.0 + (x as f64 - 7.0 - 300.0) / 1.05;
                let source_y = 350.0 + (y as f64 - 4.0 - 350.0) / 1.05;
                if source_x >= 0.0
                    && source_y >= 0.0
                    && source_x < source.width() as f64
                    && source_y < source.height() as f64
                {
                    let pixel = source.get_pixel(source_x.round() as u32, source_y.round() as u32);
                    image.put_pixel(x, y, *pixel);
                }
            }
        }
        let detected = detect_orientation(&image, 0);
        assert_eq!(detected.letter, Some('A'));
        let metrics = detected.transform.expect("transform");
        assert!((metrics.stretch_x_pct - 5.0).abs() < 1.5, "{metrics:?}");
        assert!((metrics.stretch_y_pct - 5.0).abs() < 1.5, "{metrics:?}");
        assert!((metrics.offset_x_px - 7.0).abs() < 1.5, "{metrics:?}");
        assert!((metrics.offset_y_px - 4.0).abs() < 1.5, "{metrics:?}");
        assert!(
            (metrics.source_offset_x_px - 7.0 / 1.05).abs() < 1.5,
            "{metrics:?}"
        );
    }

    #[test]
    fn detects_quarter_turn_from_card_orientation() {
        let pattern = render_pattern(PixelSize::new(640, 560), Layout::new(1, 1)).unwrap();
        let tile = &pattern.tiles[0];
        let tile_image: RgbaImage = image::imageops::crop_imm(
            &pattern.image,
            tile.source_rect.x as u32,
            tile.source_rect.y as u32,
            tile.source_rect.width as u32,
            tile.source_rect.height as u32,
        )
        .to_image();
        for (input_rotation, correction) in [(90, 270), (180, 180), (270, 90)] {
            let rotated = rotate_frame(&tile_image, input_rotation);
            let detected = detect_best_orientation(&rotated);
            assert_eq!(detected.letter, Some('A'), "input {input_rotation}");
            assert_eq!(detected.rotation_degrees, correction);
        }
    }

    #[test]
    fn marker_greedy_matching_uses_nearest_anchor() {
        let candidates = vec![
            marker_component(100.0, 100.0, 20.0),
            marker_component(900.0, 100.0, 20.0),
            marker_component(100.0, 900.0, 20.0),
            marker_component(900.0, 900.0, 20.0),
        ];
        let matches = match_markers(&candidates, 1000, 1000);
        assert_eq!(matches.len(), 4);
        assert!(matches.iter().all(|matched| matched.component.area == 400));
    }

    #[test]
    fn black_frame_has_no_letter() {
        let frame = RgbaImage::from_pixel(640, 480, Rgba([0, 0, 0, 255]));
        let detected = detect_orientation(&frame, 0);
        assert_eq!(detected.letter, None);
        assert_eq!(detected.marker_count, 0);
    }
}
