# M1 構築プロンプト（Windows）— Claude Code 用

リポジトリ直下で Claude Code を起動し、次の一文を渡す：

> `docs/prompts/M1-windows.md` を読んで、書いてある通りに M1 を完了させてください。

---

あなたは `onchip` のリードエンジニアです。`docs/DESIGN.md` を唯一の仕様書として、M1
（Windows の on-chip 経路）を完了させてください。M0 は完了済みで、`docs/VERIFICATION-M0.md`
に何が検証済みかが書いてあります。設計書に書かれていないことを勝手に追加せず、矛盾を見つけたら
作業を止めて報告してください。

## 0. 絶対ルール

- コーデック（H.264/HEVC 等）を自前実装しない。デコード/エンコードは Media Foundation の
  ハードウェア MFT に委譲する。
- 実行時依存として FFmpeg / libav を使わない。FFmpeg は `onchip bench` から外部バイナリとして
  呼ぶだけ。
- 「on-chip」の定義：デコーダ出力からエンコーダ入力まで、非圧縮ピクセルデータがシステム RAM に
  書き戻されないこと。GPU 内コピー（D3D11→D3D12 間の `CopySubresourceRegion`、
  `copy_buffer_to_texture` 等）は許容。
- `VRAM` という語をコード・ドキュメント・コミットメッセージで使わない。
- `windows` / `wgpu` / `wgpu-hal` の API は記憶で書かず、`cargo doc --open` または docs.rs の
  該当バージョン（`Cargo.lock` を見る）を参照してシグネチャを確認してから使う。確認した URL と
  バージョンを `docs/INTEROP-WIN.md` の表に残す。
- テスト素材・ロゴ・ファイル名に実在の企業名・人名・案件名を使わない。素材は
  `bench/fixtures/gen.py` で生成する。
- 完了報告の前に `cargo build --workspace`、`cargo test --workspace`、
  `cargo test -p onchip-core -- --ignored`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all --check` を実際に実行し、出力を貼る。推測で「通った」と書かない。
- 既存の core の挙動（ジョブ定義、グラフ検証、シェーダの色計算）を変えない。変える必要が
  出たら理由を書いて止まる。

## 1. 現状（M0 で出来ているもの）

- `onchip probe --json` は wgpu アダプタ情報と、`MFTEnumEx` によるハードウェア MFT 一覧
  （H.264 / HEVC × デコーダ / エンコーダ、NV12 / P010 対応の有無）を出す。
  `crates/onchip-backend-win/src/lib.rs` の `mf` モジュール。
- `onchip_core::backend` に `VideoBackend` / `Decoder` / `Encoder` トレイトと
  `StreamInfo` / `EncoderConfig` / `Packet` がある。これを実装する。
- `onchip_core::exec::Executor::run_frame(&GpuFrame) -> GpuFrame` は入力の
  `planes: [Y, CbCr]`（`R8Unorm` / `Rg8Unorm`、P010 なら `R16Unorm` / `Rg16Unorm`）を
  `TEXTURE_BINDING` 付きテクスチャとして受け取り、出力も同じ形の 2 面テクスチャ
  （`COPY_SRC | COPY_DST | TEXTURE_BINDING`）で返す。出力はストレージバッファから
  `copy_buffer_to_texture` で書かれる（GPU 内コピー）。
- `GpuFrame.native: NativeHandle::Opaque(Arc<dyn Any + Send + Sync>)` に元のネイティブ資源を
  保持できる。
- `crates/onchip-backend-win/examples/interop_poc.rs` は手順だけ書いたスタブ。

## 2. interop PoC（M1-1）— ここが本丸

`crates/onchip-backend-win/examples/interop_poc.rs` を実装し、本実装の前に単体で検証する。
`windows` クレートの feature を必要な分だけ `crates/onchip-backend-win/Cargo.toml` に足す
（`Win32_Graphics_Direct3D11`、`Win32_Graphics_Direct3D12`、`Win32_Graphics_Dxgi` 等）。

1. D3D11 デバイス（`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`）を作成し、`MFCreateDXGIDeviceManager`
   → `ResetDevice` で `IMFDXGIDeviceManager` に登録。`ID3D10Multithread::SetMultithreadProtected(true)`。
2. ハードウェア H.264 デコーダ MFT を `MFTEnumEx` で取得し、`MFT_MESSAGE_SET_D3D_MANAGER` で
   マネージャを渡す。入力メディアタイプ（H.264、解像度）と出力メディアタイプ（NV12）を設定。
   `bench/fixtures/fixture_bars_1080p_nv12.mp4` を `onchip-core` 側の最小 mp4 デマクサ
   （§3）で読み、Annex B に変換して投入。出力 `IMFSample` → `IMFMediaBuffer` →
   `IMFDXGIBuffer::GetResource` で `ID3D11Texture2D` と subresource index を取り出す。
3. **案A**：`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` が立っていなければ、呼び出し側が
   `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED` 付きで確保した NV12
   テクスチャを `MFCreateDXGISurfaceBuffer` で包んで出力サンプルとして渡す。
   `IDXGIResource1::CreateSharedHandle` → `ID3D12Device::OpenSharedHandle` でゼロコピー。
4. **案B**：MFT が自前確保する（フラグが立っている）場合、D3D11 内で共有フラグ付き NV12
   テクスチャへ `CopySubresourceRegion`（GPU 内コピー）し、それを D3D12 で開く。
5. 同期：`ID3D11Device5::CreateFence(D3D11_FENCE_FLAG_SHARED)` → `CreateSharedHandle` →
   `ID3D12Device::OpenSharedHandle` で共有フェンス。D3D11 側 `Signal`、D3D12 側 `Wait`、
   その逆も。
6. D3D12 テクスチャを wgpu に取り込む：`wgpu-hal` の dx12 バックエンドで
   `wgpu_hal::dx12::Device::texture_from_raw`（名前・シグネチャは `Cargo.lock` の
   wgpu-hal バージョンの docs.rs で確認）→ `wgpu::Device::create_texture_from_hal`。
   NV12 は D3D12 ではプレーン付き 1 リソースなので、wgpu 側では **プレーンごとに**
   `R8Unorm` / `Rg8Unorm` のビューが必要。`wgpu` が NV12 の plane view を直接サポートして
   いるか（`TextureAspect::Plane0/Plane1`、`Features::TEXTURE_FORMAT_NV12`）をまず docs.rs で
   確認し、サポートがあればそれを使う。なければ D3D12 側で各プレーンの `R8_UNORM` /
   `R8G8_UNORM` ビューを作り、別々の `wgpu::Texture` として取り込む。どちらを採ったかを
   INTEROP-WIN.md に書く。
7. 取り込んだ 2 面を `GpuFrame` にして `Executor::run_frame`（恒等グラフ：
   source → yuv2rgb → rgb2yuv → sink）を通す。出力は Executor が作る wgpu テクスチャ。
8. エンコード側：D3D12 で共有フラグ付き NV12 テクスチャを確保し、Executor の出力 2 面から
   `copy_texture_to_texture` で各プレーンへ GPU 内コピー（Executor が直接そこへ書く最適化は
   M1 の範囲外。動いてから）。D3D11 で開いて `MFCreateDXGISurfaceBuffer` → `IMFSample` →
   ハードウェア H.264 エンコーダ MFT へ。出力ビットストリームを `.h264`（Annex B）に書く。
9. **RAM 書き戻しゼロの証明**：PIX for Windows でキャプチャし、`Map` / `ReadFromSubresource` /
   readback heap 宛の `CopyResource` / `CopyTextureRegion` が一切ないことを確認する。
   PIX のスクショを `docs/img/pix-m1.png` に置き、確認手順を `docs/INTEROP-WIN.md` に書く。
   PIX が使えない環境なら GPUView（WPA）で代替し、その旨を書く。
10. 案A / 案B のどちらで通ったか、通らなかった場合は何が原因かを `docs/INTEROP-WIN.md` に
    事実として書く。推測は「推測」と明記する。

PoC が案A / 案B どちらも失敗した場合は、本実装に進まず報告して止まる。

## 3. 最小 mp4 デマクサ / マクサ（M1-2 前半）

`onchip-core` に `container` モジュールを追加（Windows 固有にしない。Mac の M2 でも使う）：

- デマクサ：`ftyp/moov/trak/mdia/minf/stbl`（`stsd` の `avc1`/`hvc1`/`hev1`、`stts`、`stss`、
  `stsc`、`stsz`、`stco`/`co64`、`ctts`）を読み、`StreamInfo`（コーデック、解像度、
  `extradata` = avcC/hvcC、タイムベース）と `Packet`（AVCC 長さプレフィックス形式のまま。
  Annex B 変換は MFT 投入直前にバックエンド側で行う）を返す。`colr` box があれば ColorSpec に
  反映、なければ BT.709 limited。音声トラックは無視。
- マクサ：映像 1 トラックの `mp4`（`avc1` / `hvc1`、`stss` にキーフレーム、`faststart` は不要）。
- 依存：`mp4` クレートを使ってよい（確認して採用。無理なら自前）。
- テスト：`bench/fixtures/gen.py` で作った mp4 を読んでパケット数・キーフレーム位置が
  `ffprobe -show_frames` と一致することを CPU テストで確認（FFmpeg は検証用の外部バイナリ）。

## 4. Windows 本実装（M1-2 後半）

- `onchip-backend-win` に `WinBackend` を実装し、`VideoBackend` を満たす。
  - `WinDecoder`：§2 の経路をそのまま。`pull()` が返す `GpuFrame.native` に
    `ID3D11Texture2D` / 共有ハンドル / フェンス値を持たせて寿命を管理。
  - `WinEncoder`：§2 の経路。`EncoderConfig` から平均ビットレート、GOP、プロファイルを設定。
  - Media Foundation **Source Reader / Sink Writer は使わない**（フレームが CPU に落ちる経路に
    入りやすい）。MFT を直接叩く。
- `onchip run in.mp4 -o out.mp4 --scale 1920x1080 --overlay logo_sample.png@32,32` が通る。
  `--codec h264|hevc` 対応。`job.output.codec == png` の経路は壊さない。
- 対応：H.264 / HEVC、NV12（8bit）。P010（10bit）は HW が対応していれば。
- Executor の出力テクスチャ → エンコーダ入力テクスチャは `copy_texture_to_texture`（GPU 内）。
- フレームプール：デコード出力・エンコード入力の共有テクスチャは N 枚（既定 4）でリングする。

## 5. ベンチ骨組み（M1-3、本番は M4）

- `onchip bench --against ffmpeg --fixture <name> --graph scale-overlay`：
  同じ素材・同じ HW エンコーダ（`h264_mf` / `hevc_mf`、または `h264_nvenc` 等。README に
  どれを使ったか書く）で FFmpeg を外部プロセスとして走らせ、onchip と並べて
  `onchip_bench::BenchResult` を `bench/results/` に出す。`tool` は `ffmpeg-cpu`。
- `onchip verify out.mp4 --ref ref.mp4`：両方を onchip 自身のデコーダでデコードし、
  フレームごとに PSNR / SSIM（SSIM は新規実装、グレースケール 8x8 ガウス窓でよい）。
  PNG 経路の verify は残す。

## 6. 完了条件（M1 クローズ）

以下をすべて満たしたら、`docs/INTEROP-WIN.md` と `docs/VERIFICATION-M1.md` を仕上げ、
README のステータス表で M1 を「完了」にし、`scripts/update-readme-env.ps1` で
検証環境を README に転記して報告する：

- [ ] CI が macos-14 / windows-latest 両方で緑
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` が無警告
- [ ] `onchip probe --json` が HW MFT 一覧を出す（M0 から継続）
- [ ] `onchip run` で 1080p/8bit の mp4 入力 → scale + overlay → H.264 mp4 出力ができ、
      出力が VLC / Windows の「映画 & テレビ」で再生できる
- [ ] 4K/10bit（P010）は、HW が対応していれば同上。非対応ならその旨を VERIFICATION-M1 に書く
- [ ] PIX（または GPUView）で RAM 書き戻しゼロを確認したスクショが `docs/img/` にある
- [ ] `docs/INTEROP-WIN.md` に案A/B の結果、使用 API のバージョン、参照 URL がある
- [ ] `bench/results/` に onchip と ffmpeg の JSON が 1 組以上（勝ち負け問わず）
- [ ] 上記の検証コマンドの実行結果を報告に貼る

## 7. 報告形式

- 結論から。各フェーズの完了/未完了、未完了なら原因（ログ付き）。
- 「できるはず」「たぶん」で埋めない。確認していないことは「未確認」と書く。
- 次に手元で実行すべきコマンドを、コピペできる形で列挙する。
