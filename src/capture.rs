use std::{collections::BTreeMap, io::Cursor, thread, time::Duration};

use image::{ImageFormat, RgbaImage, imageops};
use nokhwa::{
    Camera,
    pixel_format::RgbFormat,
    query,
    utils::{
        ApiBackend, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    },
};

use crate::model::{CaptureDeviceInfo, CaptureMode, PixelSize, ScreenInfo};

pub fn enumerate_screens() -> Result<Vec<ScreenInfo>, String> {
    let screens =
        screenshots::Screen::all().map_err(|error| format!("枚举 DXI 屏幕失败: {error}"))?;
    let mut result = Vec::with_capacity(screens.len());
    for screen in screens {
        let info = screen.display_info;
        let captured_size = match screen.capture() {
            Ok(image) => PixelSize::new(image.width(), image.height()),
            Err(_) => PixelSize::new(
                (info.width as f32 * info.scale_factor).round() as u32,
                (info.height as f32 * info.scale_factor).round() as u32,
            ),
        };
        result.push(ScreenInfo {
            id: info.id,
            x: info.x,
            y: info.y,
            size: captured_size,
            primary: info.is_primary,
            refresh_hz: info.frequency.round().max(0.0) as u32,
        });
    }
    result.sort_by_key(|screen| (!screen.primary, screen.x, screen.y));
    Ok(result)
}

pub fn enumerate_capture_devices() -> Result<Vec<CaptureDeviceInfo>, String> {
    let cameras = query(ApiBackend::MediaFoundation)
        .map_err(|error| format!("Media Foundation 设备枚举失败: {error}"))?;
    let mut devices = Vec::with_capacity(cameras.len());

    for (ordinal, camera_info) in cameras.into_iter().enumerate() {
        let index = camera_info.index().as_index().unwrap_or(ordinal as u32);
        let name = camera_info
            .human_name()
            .trim()
            .trim_matches('\0')
            .to_string();
        let path = camera_info.misc().trim().trim_matches('\0').to_string();
        let mut probe_error = None;
        let mut modes = Vec::new();
        let requested =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
        match Camera::new(CameraIndex::Index(index), requested) {
            Ok(mut camera) => {
                let current = camera.camera_format();
                if let Ok(formats) = camera.compatible_camera_formats() {
                    modes = normalize_modes(formats.into_iter().map(|format| CaptureMode {
                        width: format.width(),
                        height: format.height(),
                        fps: format.frame_rate(),
                        fourcc: format.format().to_string(),
                    }));
                }
                if modes.is_empty() {
                    modes.push(CaptureMode {
                        width: current.width(),
                        height: current.height(),
                        fps: current.frame_rate(),
                        fourcc: current.format().to_string(),
                    });
                }
            }
            Err(error) => {
                probe_error = Some(format!("无法读取采集格式: {error}"));
            }
        }

        devices.push(CaptureDeviceInfo {
            id: index as usize,
            name: if name.is_empty() {
                format!("采集设备 {ordinal}")
            } else {
                name
            },
            path,
            modes,
            probe_error,
        });
    }

    devices.sort_by_key(|device| device.name.to_lowercase());
    Ok(devices)
}

pub fn capture_card_frames(
    device_id: usize,
    mode: &CaptureMode,
    frame_count: usize,
) -> Result<Vec<RgbaImage>, String> {
    let format = mode
        .fourcc
        .parse::<FrameFormat>()
        .map_err(|error| format!("不支持采集格式 {}: {error}", mode.fourcc))?;
    let exact = nokhwa::utils::CameraFormat::new(
        Resolution::new(mode.width, mode.height),
        format,
        mode.fps,
    );
    let accepted_formats = [format];
    let requested =
        RequestedFormat::with_formats(RequestedFormatType::Exact(exact), &accepted_formats);
    let mut camera = Camera::new(CameraIndex::Index(device_id as u32), requested)
        .map_err(|error| format!("打开采集卡失败: {error}"))?;
    camera
        .open_stream()
        .map_err(|error| format!("启动采集流失败: {error}"))?;

    // Drivers often deliver one or two stale buffers after stream start.
    for _ in 0..2 {
        let _ = camera.frame();
        thread::sleep(Duration::from_millis(35));
    }

    let mut frames = Vec::with_capacity(frame_count);
    for _ in 0..frame_count.max(1) {
        let buffer = camera
            .frame()
            .map_err(|error| format!("读取采集帧失败: {error}"))?;
        let rgb = buffer
            .decode_image::<RgbFormat>()
            .map_err(|error| format!("解码采集帧失败: {error}"))?;
        frames.push(image::DynamicImage::ImageRgb8(rgb).to_rgba8());
        thread::sleep(Duration::from_millis(45));
    }
    let _ = camera.stop_stream();
    Ok(frames)
}

pub fn capture_dxi_frame(screen_id: u32) -> Result<RgbaImage, String> {
    let screens =
        screenshots::Screen::all().map_err(|error| format!("枚举 DXI 屏幕失败: {error}"))?;
    let screen = screens
        .into_iter()
        .find(|screen| screen.display_info.id == screen_id)
        .ok_or_else(|| "所选 DXI 屏幕已断开".to_string())?;
    let frame = screen
        .capture()
        .map_err(|error| format!("DXGI 桌面复制失败: {error}"))?;
    let (width, height) = frame.dimensions();
    RgbaImage::from_raw(width, height, frame.into_raw())
        .ok_or_else(|| "DXGI 帧尺寸无效".to_string())
}

pub fn downscale_for_analysis(frame: &RgbaImage) -> RgbaImage {
    const MAX_SIDE: u32 = 1920;
    let longest = frame.width().max(frame.height());
    if longest <= MAX_SIDE {
        return frame.clone();
    }
    let scale = MAX_SIDE as f64 / longest as f64;
    imageops::resize(
        frame,
        (frame.width() as f64 * scale).round() as u32,
        (frame.height() as f64 * scale).round() as u32,
        imageops::FilterType::Triangle,
    )
}

pub fn encode_png(frame: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(frame.clone())
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|error| format!("PNG 编码失败: {error}"))?;
    Ok(bytes.into_inner())
}

fn normalize_modes(iter: impl Iterator<Item = CaptureMode>) -> Vec<CaptureMode> {
    let mut unique = BTreeMap::new();
    for mode in iter {
        unique.insert(
            (mode.width, mode.height, mode.fps, mode.fourcc.clone()),
            mode,
        );
    }
    let mut modes = unique.into_values().collect::<Vec<_>>();
    modes.sort_by_key(|mode| {
        (
            std::cmp::Reverse(mode.width * mode.height),
            std::cmp::Reverse(mode.fps),
            mode.fourcc.clone(),
        )
    });
    modes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_deduplication_keeps_largest_first() {
        let modes = normalize_modes(
            [
                CaptureMode {
                    width: 1280,
                    height: 720,
                    fps: 60,
                    fourcc: "MJPEG".into(),
                },
                CaptureMode {
                    width: 1920,
                    height: 1080,
                    fps: 30,
                    fourcc: "MJPEG".into(),
                },
                CaptureMode {
                    width: 1920,
                    height: 1080,
                    fps: 30,
                    fourcc: "MJPEG".into(),
                },
            ]
            .into_iter(),
        );
        assert_eq!(modes.len(), 2);
        assert_eq!(modes[0].width, 1920);
    }
}
