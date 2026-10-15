# onchip — 設計書 v0.1（Step 1）

> Video frames stay on-chip. Decode, filter, encode — no round trip.

| 項目 | 内容 |
|---|---|
| リポジトリ | `github.com/kajisho5/onchip` |
| 種別 | 研究プロトタイプ（Research prototype） |
| ライセンス | MIT OR Apache-2.0（Rust慣例のデュアル） |
| 言語 | Rust（edition 2024） |
| 対象OS | macOS 14+（Apple Silicon優先、Intel Macは非目標）／Windows 11（D3D12対応GPU） |
| 作成日 | 2026-10-15 |

---

## 1. 位置づけ

### 1.1 何であるか
- デコード → GPUフィルタグラフ → エンコード の全工程で、**フレームがGPUアクセス可能メモリから一度も出ない**動画処理エンジンの研究実装。
- フィルタは **wgpu + WGSL の単一コードベース**で、Mac（Metal）／Win（D3D12）両方で同じシェーダが動く。
- コーデックは **OS標準に委譲**（Mac＝VideoToolbox、Win＝Media Foundation）。自前実装しない。

### 1.2 何でないか（非目標 / Non-goals）
- FFmpegの代替・互換を謳わない。対応コーデック・コンテナ数で勝負しない。
- コーデック（H.264/HEVC/AV1/ProRes等）の自前実装は**永久に**やらない。
- タイムライン編集・GUI・プレビュー再生は対象外。
- Linux対応は初期スコープ外（Vulkan Video + VA-API経由で将来可能だが、Issue化のみ）。
- 音声処理は初期スコープ外（パススルーのみ）。

### 1.3 研究課題（Research question）
> 「デコード〜エンコード間のRAM↔GPU往復を完全に排除し、かつフィルタを単一シェーダコードでMac/Win共通化した場合、FFmpegのCPUフィルタ経路およびHWフィルタ経路に対して、スループットと色精度の両面で優位に立てるか」

### 1.4 仮説（Hypothesis）
- H1：4K/10bit素材の scale + overlay + 色変換 において、FFmpeg CPUフィルタ経路比で**3倍以上**のスループット。
- H2：FFmpegのHWフィルタ経路（scale_vt / scale_cuda等）と**同等以上**のスループット（HWフィルタはそもそも機能が少ないため、機能面で勝つ）。
- H3：出力の色差（ΔE2000）は、FFmpegの同等処理と比べて**平均0.5未満**（＝実務上同一）。

数値は仮置き。M4のベンチで検証し、負けたら負けたと書く。

---

## 2. 事実と推測の整理（設計の前提）

| # | 内容 | 区分 |
|---|---|---|
| F1 | FFmpegはVideoToolbox / NVENC / NVDEC / QSV / AMF / D3D11VA に対応済み。HWデコード/エンコード自体に新規性はない | 事実 |
| F2 | FFmpegのフィルタ群は大半がCPU実装。GPUフィルタ（scale_vt, scale_cuda, overlay_vulkan等）はバックエンドごとに別実装で、機能が揃っていない | 事実 |
| F3 | Apple SiliconはUnified Memory。独立したVRAMは存在しない。「VRAM」ではなく「GPUアクセス可能メモリ」と呼ぶ | 事実 |
| F4 | VideoToolboxのデコード出力は IOSurface 裏付けの CVPixelBuffer。CVMetalTextureCache 経由で MTLTexture にゼロコピーで変換できる | 事実 |
| F5 | VTCompressionSession は CVPixelBuffer を入力に取る。IOSurface裏付けの CVPixelBuffer にMetalで描画すれば、エンコード入力もゼロコピー | 事実 |
| F6 | Media Foundation のHWデコーダMFTは DXGI Device Manager 経由で D3D11 テクスチャ（NV12等）を出力できる | 事実 |
| F7 | wgpu は Metal / D3D12 / Vulkan を抽象化し、`wgpu-hal` 経由で外部由来のネイティブテクスチャをラップできる | 事実（APIの詳細は実装時に要確認） |
| S1 | Win側で MF(D3D11) → wgpu(D3D12) のゼロコピー受け渡しは、共有NTハンドル or D3D11On12 で可能なはず。MFTが自前確保するテクスチャに共有フラグを付けられない場合、**GPU内コピー**（D3D11→共有テクスチャ）にフォールバックする。GPU内コピーはRAMを経由しないため「on-chip」の定義は満たす | 推測（最大の技術リスク） |
| S2 | wgpuでYUVプレーナ（NV12/P010）を直接扱うのは制約がある。Y面とUV面を別テクスチャとして扱い、シェーダで合成する方式が現実的 | 推測 |
| U1 | `onchip` の crates.io 上の空き状況 | 事実：未使用（2026-10-15 `cargo info onchip` で not found を確認） |

---

## 3. アーキテクチャ

### 3.1 パイプライン

```
 [入力ファイル]
      │ (CPU: デマックス。圧縮ビットストリームのみ扱う。ここだけRAM)
      ▼
 ┌──────────────┐
 │  Demuxer     │  mp4/mov → 圧縮サンプル（NAL等）
 └──────┬───────┘
        ▼
 ┌──────────────┐   Mac: VideoToolbox → CVPixelBuffer(IOSurface)
 │  Decoder     │   Win: Media Foundation → ID3D11Texture2D
 └──────┬───────┘
        ▼  ★ここから先、フレームはGPUメモリを出ない
 ┌──────────────┐   ネイティブテクスチャを wgpu::Texture にラップ
 │  Import      │   (ゼロコピー / 不可ならGPU内コピー)
 └──────┬───────┘
        ▼
 ┌──────────────┐   WGSL シェーダ群。Mac/Win 共通コード
 │ Filter Graph │   yuv2rgb → scale → overlay → rgb2yuv ...
 └──────┬───────┘
        ▼
 ┌──────────────┐   wgpu::Texture → ネイティブテクスチャへ戻す
 │  Export      │
 └──────┬───────┘
        ▼
 ┌──────────────┐   Mac: VTCompressionSession
 │  Encoder     │   Win: MF HW Encoder MFT
 └──────┬───────┘
        ▼  (CPU: 圧縮ビットストリーム)
 ┌──────────────┐
 │  Muxer       │  → mp4/mov
 └──────────────┘
```

**on-chipの定義**：デコーダ出力からエンコーダ入力まで、**非圧縮ピクセルデータがシステムRAMに書き戻されない**こと。GPU→GPUコピーは許容。圧縮ビットストリームはサイズが小さいためCPU経由でよい。

### 3.2 クレート構成（Cargo workspace）

```
onchip/
├── Cargo.toml                  # workspace
├── crates/
│   ├── onchip-core/            # 共通型・フィルタグラフ・WGSLシェーダ・ジョブ定義
│   ├── onchip-backend-mac/     # VideoToolbox + Metal interop（cfg(target_os="macos")）
│   ├── onchip-backend-win/     # Media Foundation + D3D12 interop（cfg(target_os="windows")）
│   ├── onchip-cli/             # `onchip` バイナリ
│   └── onchip-bench/           # ベンチハーネス（FFmpeg比較・ΔE計測）
├── shaders/                    # WGSL（core から include_str!）
├── bench/
│   ├── fixtures/               # テスト素材の生成スクリプト（素材本体はLFSにも置かない。生成で作る）
│   └── results/                # ベンチ結果 JSON + README に転記
├── docs/
│   ├── DESIGN.md               # この文書
│   ├── INTEROP-MAC.md          # Mac interop の調査メモ
│   └── INTEROP-WIN.md          # Win interop の調査メモ
├── ROADMAP.md
└── .github/workflows/ci.yml    # macos-14 / windows-latest
```

### 3.3 Backend トレイト（core が要求するもの）

```rust
pub trait VideoBackend {
    type Decoder: Decoder;
    type Encoder: Encoder;
    fn probe(&self) -> BackendInfo;            // GPU名・対応コーデック・UMAかどうか
    fn open_decoder(&self, stream: &StreamInfo) -> Result<Self::Decoder>;
    fn open_encoder(&self, cfg: &EncoderConfig) -> Result<Self::Encoder>;
}

pub trait Decoder {
    /// 圧縮サンプルを投入し、デコード済みフレームを wgpu::Texture として返す
    fn push(&mut self, packet: Packet) -> Result<()>;
    fn pull(&mut self) -> Result<Option<GpuFrame>>;
}

pub trait Encoder {
    fn push(&mut self, frame: GpuFrame) -> Result<()>;
    fn pull(&mut self) -> Result<Option<Packet>>;
}

pub struct GpuFrame {
    pub planes: Vec<wgpu::Texture>,   // NV12なら [Y, UV] の2面
    pub format: PixelFormat,          // Nv12 / P010 / Rgba16Float ...
    pub color: ColorSpec,             // primaries / transfer / matrix / range
    pub pts: i64,
    pub native: NativeHandle,         // 元のネイティブ資源を保持（寿命管理）
}
```

### 3.4 フレーム表現と色管理

- デコード直後：`Nv12`（8bit）／`P010`（10bit）。Y面＝R8/R16、UV面＝Rg8/Rg16 の2テクスチャ。
- フィルタグラフ内部：`Rgba16Float`、**リニア光**。YUV→RGB変換時に transfer（BT.709 / sRGB / PQ / HLG）を外し、rgb2yuv で戻す。
- `ColorSpec` はフレームに常に同伴させ、フィルタは入出力の ColorSpec を宣言する（不一致はグラフ構築時にエラー）。
- 初期対応：BT.709 limited/full、BT.2020 PQ（M3以降）。クロマ位置は BT.709 既定（left）固定、他はM3以降。
- **色の正しさがFFmpegと一致しないと速くても無価値**。ΔE計測をCIに入れる（§6）。

### 3.5 フィルタグラフ

- 宣言的ジョブ定義（JSON / YAML）→ DAG → wgpu コマンドバッファ。
- 1フレーム1コマンドバッファ。中間バッファは出力解像度ごとにプール。
- 初期フィルタ（M1〜M3で実装する最小セット）：

| ノード | 入力 | 出力 | 備考 |
|---|---|---|---|
| `source` | — | yuv | デコーダ |
| `yuv2rgb` | yuv | rgba16f linear | matrix/range/transfer 解決 |
| `scale` | rgba16f | rgba16f | bilinear / bicubic / lanczos3 |
| `overlay` | rgba16f ×2 | rgba16f | 位置・不透明度・プリマルチ |
| `rgb2yuv` | rgba16f linear | yuv | エンコーダ要求フォーマットへ |
| `sink` | yuv | — | エンコーダ |

- ジョブ定義例：

```json
{
  "input": "in.mov",
  "output": { "path": "out.mp4", "codec": "hevc", "bitrate": "20M", "color": "bt709" },
  "graph": [
    { "id": "src", "op": "source" },
    { "id": "rgb", "op": "yuv2rgb", "in": ["src"] },
    { "id": "sc",  "op": "scale", "in": ["rgb"], "width": 1920, "height": 1080, "filter": "lanczos3" },
    { "id": "logo","op": "image", "path": "logo.png" },
    { "id": "ov",  "op": "overlay", "in": ["sc", "logo"], "x": 32, "y": 32, "opacity": 0.9 },
    { "id": "yuv", "op": "rgb2yuv", "in": ["ov"] },
    { "id": "out", "op": "sink", "in": ["yuv"] }
  ]
}
```

### 3.6 CLI

```
onchip run job.json                     # ジョブ実行
onchip run in.mov -o out.mp4 --scale 1920x1080 --overlay logo.png@32,32   # ショートハンド
onchip probe                            # GPU・バックエンド・対応コーデック表示
onchip bench --against ffmpeg --fixture 4k-p010 --graph scale-overlay    # ベンチ
onchip verify out.mp4 --ref ffmpeg_out.mp4   # PSNR / SSIM / ΔE2000 比較
```

---

## 4. プラットフォーム別 interop 設計

### 4.1 macOS（VideoToolbox + Metal）

| 工程 | 手段 | コピー |
|---|---|---|
| デコード | `VTDecompressionSession`、出力 `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` / `...10BiPlanar...` | — |
| Import | `CVMetalTextureCacheCreateTextureFromImage` で Y/UV 各面を `MTLTexture` 化 → `wgpu-hal::metal` でラップ | **ゼロ** |
| Export | `CVPixelBufferPool` から IOSurface裏付け CVPixelBuffer を確保 → その MTLTexture を wgpu でラップし rgb2yuv の出力先にする | **ゼロ** |
| エンコード | `VTCompressionSession` に上記 CVPixelBuffer を投入 | — |

- 使用クレート候補：`objc2` / `objc2-core-video` / `objc2-video-toolbox` / `objc2-metal`（実装時に最新版を確認）
- Apple Silicon = UMA のため、万一ゼロコピーに失敗しても「RAM↔GPU往復」の概念自体が薄い。**Macは楽な方**。M2 に回す理由。

### 4.2 Windows（Media Foundation + D3D12）

| 工程 | 手段 | コピー |
|---|---|---|
| デコード | HWデコーダMFT（`MFT_ENUM_FLAG_HARDWARE`）、`IMFDXGIDeviceManager` で D3D11 デバイスを渡し、出力を `ID3D11Texture2D`(NV12/P010) で受ける | — |
| Import | **案A**：D3D11 テクスチャを `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` 付きで確保し `OpenSharedHandle` で D3D12 側へ。**案B**：MFTの出力テクスチャを D3D11 内で共有テクスチャへ `CopySubresourceRegion`（GPU内コピー）→ D3D12 | 案A: ゼロ／案B: GPU内1回 |
| 同期 | D3D11/D3D12 間は `ID3D11Fence` ⇄ `ID3D12Fence` の共有フェンス | — |
| Export | D3D12 で確保した共有テクスチャ（NV12）に rgb2yuv を書き、D3D11 側で開いて `IMFSample` に包む | ゼロ or GPU内1回 |
| エンコード | HWエンコーダMFT（NVIDIA/AMD/Intel の MFT。同じ DXGI Device Manager を渡す） | — |

- 使用クレート：`windows`（Microsoft公式）。`wgpu-hal::dx12` でラップ。
- **S1 がこの計画最大のリスク**。M1 の最初に interop 単体の PoC を切り出して検証し、結果を `docs/INTEROP-WIN.md` に残す。Win を先行させるのは「一番難しい所で早く転ぶ」ため（fail fast）。

### 4.3 共通：wgpu 側

- `wgpu` の `hal` フィーチャで `Device::create_texture_from_hal` を使う（存在・シグネチャは実装時に要確認）。
- プレーナ対応がない前提で、Y/UV を独立テクスチャとして扱う（S2）。
- フィルタ実行はコンピュートシェーダ（WGSL）。レンダーパイプラインは使わない（頂点が無駄）。

---

## 5. マイルストーン

| M | 内容 | 完了条件 |
|---|---|---|
| **M0** | リポ骨組み・CI・`onchip probe`・ジョブ定義パーサ・WGSLで scale/overlay/色変換（PNG入力で単体テスト） | CIがMac/Win両方で緑 |
| **M1** | **Win** 経路：MF decode → D3D12 → MF encode。interop PoC（§4.2 案A/B）→ 本実装。H.264/HEVC、NV12/P010 | PIX か GPUView で RAM書き戻しゼロ確認。案A/Bどちらで通ったかを `docs/INTEROP-WIN.md` に記録 |
| **M2** | **Mac** ゼロコピー経路：VT decode → wgpu → VT encode | 4K素材で RAM書き戻しゼロを Instruments（Metal System Trace）で確認したスクショを docs に残す |
| **M3** | 色管理の完成：BT.2020 PQ、full/limited、`onchip verify` の ΔE2000 | FFmpeg同等処理との ΔE 平均 < 0.5 |
| **M4** | ベンチ公開：`onchip bench` と結果 README。負けも含めて公開 | README にグラフ、再現手順、素材生成スクリプト |
| M5+ | Linux（Vulkan Video）、AV1、ProRes(Mac)、音声、ffmpeg-skill系からの呼び出し | ROADMAP に Issue 化 |

M1 完了＝「研究として成立したか」の最初の判定点。ここで止めても成果になる。

---

## 6. 評価設計（M4）

### 6.1 比較対象
1. FFmpeg CPU経路：`-c:v hevc_videotoolbox` / `hevc_nvenc` でHWエンコしつつ、フィルタは `scale` + `overlay`（CPU）
2. FFmpeg HW経路（Mac）：`-hwaccel videotoolbox -vf scale_vt=...`（overlay はHW版がないため、できる範囲で）
3. FFmpeg HW経路（Win/NVIDIA）：`-hwaccel cuda -vf scale_cuda=...,overlay_cuda=...`
4. onchip

### 6.2 指標
- スループット：fps（壁時計）、1フレームあたり ms、CPU使用率
- 品質：出力同士の PSNR / SSIM、および**同一入力・同一ビットレート**での ΔE2000（デコードして比較）
- メモリ：RAM↔GPU転送量（Instruments / PIX の計測値）

### 6.3 素材
- 実素材は使わない。`bench/fixtures/gen.py` で合成（カラーバー・ズームパン・ノイズ・10bitグラデ）。架空のロゴPNGも生成。
- 解像度：1080p / 4K、8bit / 10bit、30fps、各30秒。

### 6.4 公開ルール
- 勝っても負けても数値を載せる。負けた項目は原因分析を Issue 化。

---

## 7. リスクと対策

| リスク | 影響 | 対策 |
|---|---|---|
| S1：Win の D3D11→D3D12 ゼロコピー不可 | on-chip の純度低下 | GPU内コピーで妥協し、定義（§3.1）に明記。M2冒頭でPoC |
| wgpu のネイティブ取り込みAPIが不安定 | 実装コスト増 | wgpu のバージョンをピン留め。必要なら `wgpu-hal` 直叩きに降りる |
| 色が FFmpeg と合わない | 価値ゼロ | ΔE を CI の必須チェックに。参照値は FFmpeg で生成して固定 |
| HWエンコーダの画質/レート制御差 | ベンチ比較の公平性 | 同一HWエンコーダ（VT / NVENC）を両者で使い、フィルタ部分だけを比較対象にする |
| 対応GPUが手元に限られる | Win検証範囲 | 手元GPUを README に明記。他GPUは Issue で募る |

---

## 8. リポジトリ運用（kajisho5 既存流儀に合わせる）

- `release-please` でバージョン管理（メジャーは自動で上げない）
- `ROADMAP.md`：planned 項目は Issue 化
- README：1行目はタグライン、デモGIF（Instruments のRAM転送ゼロ画面 or ベンチグラフ）、再現手順
- CI：`macos-14`（Apple Silicon）/ `windows-latest`。GPU不要のテスト（WGSLのPNG単体テスト、パーサ）はCIで、GPU必要なテストは手元で `cargo test -- --ignored`
- Issue テンプレ：`interop-report`（GPU名・OS・案A/Bどちらで通ったか）

---

## 9. 決定事項

1. M1 は **Win 先行**（2026-10-15 決定。最大リスク S1 を最初に潰す）
2. 初期コーデックは H.264 + HEVC（ProRes は M5）
3. ジョブ定義は JSON 固定（YAML は後回し）
4. Win 検証GPUは `onchip probe` の出力を README に自動転記（手入力しない）
