# VCable

VCableは、macOS上で任意個数の仮想オーディオデバイスを作成し、仮想・物理デバイス間を任意のチャンネル行列で接続するための実装です。

## 構成

- `VCableDriver.driver`: `coreaudiod` 内で動作するAudioServerPlugIn
- `vcabled`: Core Audioデバイスを列挙し、ルーティンググラフとリアルタイムI/Oを管理するRustデーモン
- `vcablectl`: Unixソケット経由でデバイスとルートを管理するCLI
- `vcable-core`: 循環検出、チャンネル行列、SPSCリングバッファ
- `vcable-coreaudio`: Core Audio FFI、IOProc、線形リサンプル、クロックドリフト補正

HALドライバは音声ループバックとCore Audioオブジェクトの公開だけを担当します。物理デバイス間のミックス、分岐、チャンネル変換、異なるクロック間のリサンプルは`vcabled`が担当します。

## 実装済み機能

- 実行時の仮想デバイス作成・削除と永続化
- デバイスごとの安定したUID
- 1〜64チャンネル、8 kHz〜768 kHz
- 最大4096個の同時仮想デバイス
- Float32 interleaved PCMループバック
- 複数入力のミックスと複数出力への分岐
- output×input形式の任意チャンネル行列
- 異サンプルレート間の線形リサンプル
- リングバッファ充填率を使ったクロックドリフト補正
- 循環ルートの明示的拒否
- 使用中デバイス削除の明示的拒否
- underrun、overrun、format errorメトリクス
- 0600権限の長さ付きUnixソケットプロトコル

上限超過、未対応フォーマット、存在しないエンドポイントなどはエラーになります。別デバイスや別フォーマットへの暗黙の切り替えは行いません。

## 必要環境

- macOS 13以降
- Xcode
- Rust stable
- `xcodebuildmcp`

開発環境ではarm64ビルドを検証しています。Xcodeプロジェクト自体はmacOSのarm64/x86_64を対象にできます。

## ビルドと検査

すべての検査を実行します。

```sh
./scripts/check.sh
```

個別に実行する場合:

```sh
CARGO_BUILD_RUSTC_WRAPPER= cargo test --workspace
CARGO_BUILD_RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings

xcodebuildmcp macos build \
  --project-path "$PWD/native/VCableDriver/VCableDriver.xcodeproj" \
  --scheme VCableDriverTests \
  --configuration Debug \
  --derived-data-path "$PWD/.build/xcode" \
  --arch arm64

.build/xcode/Build/Products/Debug/VCableDriverTests \
  .build/xcode/Build/Products/Debug/VCableDriver.driver/Contents/MacOS/VCableDriver
```

ネイティブ統合テストはドライバを`dlopen`し、偽のCore Audioホストから以下を実行します。

- 動的デバイス作成・削除
- デバイス／ストリームプロパティ検証
- StartIO／StopIO
- タイムスタンプ取得
- 書き込んだFloat32サンプルのループバック一致
- 永続化とプロパティ変更通知

## HALドライバのインストール

インストールはシステム全体の音声スタックを変更します。内容を確認してから、ビルド済みバンドルを明示的に指定してください。

```sh
sudo ./scripts/install-driver.sh \
  "$PWD/.build/xcode/Build/Products/Debug/VCableDriver.driver"
```

スクリプトは次を満たさない場合に中止します。

- rootで実行されている
- 指定元が有効な`.driver`バンドルである
- 署名検証に成功する
- `/Library/Audio/Plug-Ins/HAL/VCableDriver.driver` がまだ存在しない

インストール後はmacOSを再起動してください。既存バンドルを暗黙に上書きする処理はありません。

アンインストールは対象を削除せず、Audio Plug-Ins外の退避ディレクトリへ移動します。

```sh
sudo ./scripts/uninstall-driver.sh
```

既存版を更新する場合は、アンインストールで旧版を退避した後、新しいバンドルを明示してインストールし、macOSを再起動します。

```sh
sudo ./scripts/uninstall-driver.sh
sudo ./scripts/install-driver.sh \
  "$PWD/.build/xcode/Build/Products/Debug/VCableDriver.driver"
```

単純な上書き更新は行いません。

製品配布ではアドホック署名ではなくDeveloper ID署名とnotarizationを設定してください。

## デーモンの起動

ソケットと状態ファイルは省略できません。利用する場所を明示して起動します。

```sh
mkdir -p "$HOME/Library/Application Support/VCable"

target/debug/vcabled \
  --socket "$HOME/Library/Application Support/VCable/control.sock" \
  --state "$HOME/Library/Application Support/VCable/routes.state"
```

別のターミナルで同じソケットを指定します。

```sh
VCABLE_SOCKET="$HOME/Library/Application Support/VCable/control.sock"

target/debug/vcablectl --socket "$VCABLE_SOCKET" ping
target/debug/vcablectl --socket "$VCABLE_SOCKET" status
```

## 仮想デバイス管理

2入力・2出力、48 kHzのデバイスを作成します。

```sh
target/debug/vcablectl --socket "$VCABLE_SOCKET" \
  create chat "VCable Chat" 2 2 48000
```

作成後のUIDは`dev.vcable.device.chat`です。削除時はIDを指定します。

```sh
target/debug/vcablectl --socket "$VCABLE_SOCKET" delete chat
```

I/O実行中、またはルートから参照中のデバイスは削除できません。Core Audioによる受動的なクライアント登録だけでは削除を妨げません。

## ルート管理

最初に`status`でエンドポイントIDを確認します。

同じチャンネル数同士では、行列を省略すると恒等行列になります。gainは0.001 dB単位です。

```sh
target/debug/vcablectl --socket "$VCABLE_SOCKET" connect \
  chat-to-speakers \
  audio:dev.vcable.device.chat:input \
  audio:BuiltInSpeakerDevice:output \
  0
```

チャンネル数が異なる場合は、output行×input列の順に係数を明示します。係数は1,000,000を1.0として指定します。次はステレオをモノラルへ-6.02 dBずつで合成する1×2行列です。

```sh
target/debug/vcablectl --socket "$VCABLE_SOCKET" connect \
  stereo-to-mono SOURCE_ENDPOINT SINK_ENDPOINT 0 500000,500000
```

ルート削除と正常終了:

```sh
target/debug/vcablectl --socket "$VCABLE_SOCKET" disconnect chat-to-speakers
target/debug/vcablectl --socket "$VCABLE_SOCKET" shutdown
```

`shutdown`はルーターを停止してからソケットを削除します。

## リアルタイム動作

音声コールバックでは、ヒープ確保、Mutex、ファイルI/O、IPC、ログ出力を行いません。ルート変更時に必要なリングバッファ、行列、リサンプル状態を事前確保します。

異なるクロックの接続では、各ルートにSPSCリングバッファを置き、出力先のコールバックがpullします。公称サンプルレート比に加えてリング充填率から±1000 ppm以内で比率を補正します。バッファ不足は無音として定義し、`underruns`へ記録します。別ルートへ切り替える処理はありません。
