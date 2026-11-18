# Logicool HID++ 対応

| 機種 | 接続 | マウスの識別 | 確認状況 |
| --- | --- | --- | --- |
| PRO X3 SUPERSTRIKE | LIGHTSPEED `046d:c54f` | USB モデル ID `c0a9`、無線 ID `40be` | Windows で実機確認 |
| PRO X3 SUPERSTRIKE | USB `046d:c0a9` | 同上 | 実機未確認 |
| 初代 PRO Wireless | LIGHTSPEED `046d:c539` または USB `046d:c088` | USB モデル ID `c088`、無線 ID `4079` | 実機未確認 |

PRO X3 SUPERSTRIKE の **DPI** タブでは現在の感度を変更できます。
**性能**タブではポーリングレート、リフトオフ距離、左右の作動点、Rapid Trigger の有効化と感度、振動を変更できます。
「使用するプロファイル」で PC 設定と有効な本体プロファイルを切り替えられます。
本体プロファイルの編集中にフラッシュを書き換える操作、ボタン割り当て、マクロ、G HUB のプロファイル DB の一括取り込み、ファームウェア更新には対応しません。
DPI タブが編集するのは現在の感度 1 段です。

G HUB を終了してから Device HUB で設定します。
電源や接続の切り替えで PC モードの設定がリセットされる場合は、デバイスの**一般**タブで「現在の設定を起動用に保存」を実行します。
起動時と再検出時に、そのマウスの保存済み設定を復元します。
本体プロファイルを選択してから同じ操作を実行すると、PC 側の復元を解除します。

Windows で常駐させるには、リリースビルド後に以下を実行します。

```powershell
./scripts/install-resident.ps1 -Executable ./target/release/device-hub.exe
```

ログオン時に通知領域へ起動し、ウィンドウの × で通知領域に戻ります。
アイコンにマウスの電池残量を数字で表示し、ポイントすると機種名と充電状態も表示します。
Windows がアイコンを隠している場合は、タスクバーの「その他のシステム トレイ アイコン」で Device HUB を表示します。
アイコンをクリックするか、スタートメニューの「Device HUB」を選ぶと開きます。
終了するには通知領域アイコンの右クリックメニューを使います。

## プロトコル

実機の X3 レシーバーは `FF43:0302` の Output/Input コレクションを使用します。
`0x11` は 20 バイトで、レポート ID、接続スロット、機能インデックス、機能内コマンドとソフトウェア ID、16 バイトのデータの順です。
Root で機能インデックスを解決します。受信機自体を指す `0xff` と、実機が応答するペアリングスロット `0x01` は別です。
共有レシーバーでは `0x0003` のモデル ID と個体識別子を照合してから設定を送ります。

| 機能 | 読み取りと書き込み |
| --- | --- |
| `0x1004` 電池 | Fn 1 の先頭に残量の %。初代等は `0x1000` または `0x1001` を検出して使用 |
| `0x2202` DPI | Fn 1 で軸・LOD 対応を検出。Fn 2 のページ化された候補を展開。Fn 5 で現在値、Fn 6 で X/Y/LOD を一括書き込み。X3 の LOD は `1/2/3` |
| `0x2201` 従来 DPI | Fn 1 で候補、Fn 2 で現在値、Fn 3 で書き込み |
| `0x8061` 拡張ポーリング | Fn 1 と Fn 2 の最初の引数は有線 `0` / 無線 `1`。Fn 3 にレート番号 `0..6`（125〜8000 Hz）。X3 の無線経路では全 7 段階の変更と読み戻しを確認 |
| `0x8060` 従来ポーリング | Fn 0 で候補、Fn 1 で現在値、Fn 2 に周期（ms）を書き込み |
| `0x1b0c` 左右クリック | Fn 0 で範囲、Fn 2 で読み取り、Fn 1 で指定ボタンだけ更新。レベルは上位 6 ビット、Rapid Trigger のバイトの bit 0 は有効フラグ |
| `0x8100` 本体プロファイル | Fn 5 でディレクトリを読む。Fn 1/2 でモード変更/確認、Fn 3/4 で有効なプロファイルを選択/確認 |

実装の参照元は [Logitech HID++ 2.0](https://github.com/Logitech/cpg-docs/tree/master/hidpp20)、
[Solaar の設定定義](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/settings_templates.py)、
[OpenMouse の HID++ 実装](https://github.com/OpenMouse-Project/mouse-protocol/blob/main/src/drivers/logitech/hidpp.ts)、
[libratbag の初代 PRO Wireless 識別表](https://github.com/libratbag/libratbag/blob/master/data/devices/logitech-g-pro-wireless.device)。
