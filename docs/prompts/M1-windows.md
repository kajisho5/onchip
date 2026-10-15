# M1 検証プロンプト（Windows）— Claude Code 用

リポジトリ直下で Claude Code を起動し、次の一文を渡す：

> `docs/prompts/M1-windows.md` を読んで、書いてある通りに M1 を検証・修正して完了させてください。

---

あなたは `onchip` のリードエンジニアです。M1（Windows の on-chip 経路）の**コードは書かれているが、
実機で一度も動いていない**。あなたの仕事は、この Windows 機で動かし、壊れている所を直し、証拠を
残すこと。仕様書は `docs/DESIGN.md`、現状は `docs/VERIFICATION-M1.md` と `docs/INTEROP-WIN.md`。

## 0. 絶対ルール
- コーデックを自前実装しない。デコード/エンコードは Media Foundation の MFT に委譲する。
- 実行時に FFmpeg / libav を使わない（`onchip bench` / `onchip verify` が外部バイナリとして呼ぶのは可）。
- 「on-chip」の定義：デコーダ出力からエンコーダ入力まで非圧縮ピクセルを RAM に書き戻さない。
  GPU 内コピーは可。
- `VRAM` と書かない。
- `windows` / `wgpu` の API は記憶で書かず、`cargo doc --open` か docs.rs（`Cargo.lock` のバージョン）で
  確認する。確認した API と URL を `docs/INTEROP-WIN.md` の表に追記する。
- 素材は `bench/fixtures/gen.py` の合成素材のみ。実在名を使わない。
- 報告前に検証コマンドを**実際に実行して出力を貼る**。推測で「通った」と書かない。
- core（ジョブ定義・グラフ検証・シェーダの色計算）の挙動は変えない。変える必要が出たら理由を
  書いて止まる。

## 1. 手順
1. `docs/VERIFICATION-M1.md` の「Steps to run on Windows」を上から順に実行する。
2. 失敗したら、`crates/onchip-backend-win/src/` の該当箇所を直す。直す時はエラーの HRESULT と
   MFT 名を必ずログに残す（`eprintln!` でよい）。推測で複数箇所を一度に変えない。
3. `interop_poc` が完走したら、その JSON 出力（plan A/B、MFT 名、async か）を
   `docs/INTEROP-WIN.md` の Outcome に貼る。
4. PIX for Windows で `interop_poc` をキャプチャし、readback（`Map` / `ReadFromSubresource` /
   readback heap 宛の Copy）が無いことを確認、スクショを `docs/img/pix-m1.png` に置く。
   PIX が使えない環境なら GPUView で代替し、その旨を書く。
5. `onchip run`（1080p → 720p + overlay、H.264）の出力が VLC で再生できることを確認する。
6. `onchip bench job.json --against ffmpeg --ffmpeg-encoder h264_mf` を 1080p と 4K（8bit）で回し、
   `bench/results/` の JSON を commit する。勝っても負けても載せる。
7. `scripts/update-readme-env.ps1` で README の検証環境を更新する。
8. `docs/VERIFICATION-M1.md` の Results 表を埋め、README のステータス表で M1 を「完了」にする。

## 2. 完了条件
- [ ] CI が macos-14 / windows-latest 両方で緑
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` が無警告
- [ ] `onchip probe --json` が HW MFT 一覧を出す
- [ ] `interop_poc` 完走、plan A/B を記録
- [ ] `onchip run` の H.264 出力が再生できる
- [ ] PIX（または GPUView）のスクショが `docs/img/` にある
- [ ] `docs/INTEROP-WIN.md` の Environment / Outcome / Proof が埋まっている
- [ ] `bench/results/` に onchip と ffmpeg の JSON が 1 組以上
- [ ] 検証コマンドの実行結果を報告に貼る

## 3. 報告形式
- 結論から。各手順の完了/未完了、未完了なら原因（ログ付き）。
- 「できるはず」で埋めない。確認していないことは「未確認」と書く。
