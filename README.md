# Card Reader Comparator

一个面向 Windows 的轻量采集卡比较工具。它使用 DXGI 接管一台 DXI 输出屏幕，通过 Media Foundation 读取多张采集卡，并以已知标定图案测量每张卡相对于完整 DXI 画面的横向/纵向拉伸、中心偏移和短时抖动。

本版本只关注画面位置漂移，不用压缩伪影评价采集卡画质。

## 功能

- 枚举 DXGI 显示器与 Media Foundation 视频采集设备。
- 显示稳定的设备路径/设备号，并允许切换采集格式。
- 1–7 张采集卡，支持行列整数分区的拼接模式，例如 `2×2`、`1×4`、`4×1`。
- 开始前校验 DXI 单分区与采集卡汇报分辨率是否为整数比例，不满足时直接弹窗拒绝。
- 在所选 DXI 屏幕上创建无边框、置顶的全屏标定层，按从左到右、从上到下显示大号 A–G。
- 每个分区带四个定位标；OCR 失败时进入显式的手动确认流程。
- 支持 0°/90°/180°/270° 旋转校正和物理位置重排，适用于盲插、交叉接线。
- 报告 X/Y 轴伸缩百分比、画面中心偏移、折算回源坐标的等效偏移、拟合误差和三帧时序抖动。
- 导出 JSON、CSV、DXI 回读截图与每张采集卡的截图。

## 使用

1. 在 Windows 10/11 上准备一块用于输出的 DXI 屏幕，以及 1–7 张可被系统识别的视频采集设备。
2. 选择 DXI 屏幕、采集卡和拼接模式。每一行采集结果会显示设备号与设备路径末尾，便于区分同型号卡。
3. 点击“校验配置并开始”。整数比例不成立时，程序会显示“错误的拼接模式”弹窗。
4. 所选 DXI 屏幕会被全屏标定层接管。标定期间按 `Esc` 可退出全屏层。
5. 读取完成后，检查每张卡的 OCR 字母、预览、旋转方向与物理位置。OCR 未能可靠识别时，使用卡片上的两个按钮手动修正。
6. 点击“确认并计算”查看结果；点击“导出报告”写出 JSON、CSV 和 PNG。

建议把主程序放在**另一台控制显示器**上。主程序与标定层位于同一屏幕时，全屏层会遮住确认界面。

报告默认保存在：

```text
%LOCALAPPDATA%\CardReaderComparator\reports\compare-<timestamp>\
```

## 构建

依赖 Rust stable、Windows 10/11 SDK 与 MSVC Build Tools。建议在 Visual Studio Developer PowerShell 中构建：

```powershell
cargo build --release
cargo test
```

生成文件：

```text
target\release\card-reader-comparator.exe
```

本机开发环境未将 MSVC 工具加入默认 `PATH` 时，可先从 VS 安装目录调用 `VsDevCmd.bat`，再运行 `cargo build`。

## 拼接与比例约束

设 DXI 屏幕为 `W×H`，拼接为 `cols×rows`，每张卡汇报 `w×h`。本版本要求：

```text
W = cols × tile_w
H = rows × tile_h
tile_w % w == 0
tile_h % h == 0
```

当前版本要求所有比较卡的输出分辨率一致。支持矩形整数分区，不宣称支持任意异形拼接、透视校正或非整数缩放。

## 算法摘要

标定图案由 5×7 块字和四个角落定位标组成。分析流程为：

1. 自动尝试四个直角方向，利用块字形状选择最可信的方向。
2. 通过连通域面积、长宽比与填充率筛选定位标，再按期望象限做一对一匹配。
3. 对 X、Y 分别拟合 `observed = scale × normalized_position + offset`，并剔除明显离群定位标。
4. 将 `scale` 与采集卡填满画面的名义比例比较，得到横向/纵向伸缩百分比。
5. 将仿射解释为围绕画面中心的“先缩放后平移”；同时给出 `source_offset = center_offset / scale_ratio`，即代数等价的“先平移后缩放”表示。
6. 对三帧结果计算中心位置样本标准差，作为短时抖动。

静态单帧只能观测一个一维仿射关系，无法从代数上判定“硬件先拉伸还是先偏移”的因果顺序。两种表示是同一关系的不同分解，因此报告同时给出两种比例，而不伪造不可观测的因果顺序。

更详细说明见 [docs/algorithm.md](docs/algorithm.md)。

## 本版本限制

- 仅支持 Windows。
- 采集卡损失、色彩误差、延迟和压缩质量不在比较范围内。
- 透视、轻微旋转、非轴对称形变不在拟合模型内。
- 所有卡必须使用相同输出分辨率；最多 7 张。
- 全屏标定层依赖所选 DXI 屏幕的 DXGI Desktop Duplication 可用性。

## 开发

主要模块：

```text
src/capture.rs   DXGI 屏幕枚举/回读 + Media Foundation 采集
src/overlay.rs   全屏无边框标定窗口与 GDI 位图绘制
src/pattern.rs   5×7 块字、四角定位标与 BGRA 标定图
src/analysis.rs  连通域、OCR、四方向识别与轴向仿射拟合
src/workflow.rs  后台采集、映射、比较报告与导出
src/ui.rs        GPUI 主界面
```

参考范围限定为 KDock 的 DXGI 抓屏思路与 OBS 的 Windows Capture Device 工作流；项目没有修改 KDock 的任何文件。

