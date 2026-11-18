//! UI ↔ バックエンドの橋。
//!
//! - デバイスごとに**専用のワーカースレッド**を 1 本持つ。HID の往復は全部そこで行い、
//!   UI スレッドは絶対にブロックしない。
//! - 変更は**楽観的**: UI の状態を即座に書き換えてから `Change` をワーカーへ送る。
//!   ワーカーは連続した変更のうち、後の値で完全に置き換わるものを間引く (ドラッグ対策)。
//!   失敗したらトーストを出して `read` で実機の値に戻す。
//! - 結果は futures の channel で UI へ戻る (アイドル時は一切起きない)。

use crate::rawinput;
use devhub_core::*;
use futures::channel::mpsc::UnboundedSender;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

pub const SCAN_EVERY: Duration = Duration::from_secs(2);
pub const BATTERY_EVERY: Duration = Duration::from_secs(60);

/// ワーカー → UI。
pub enum Msg {
    /// 列挙結果。`gen` は発行時の世代 — それより古い結果は捨てる。
    Scanned {
        gen: u64,
        res: Result<Vec<DeviceInfo>, String>,
    },
    Opened {
        id: DeviceId,
        info: DeviceInfo,
        caps: Box<Capabilities>,
    },
    OpenFailed {
        id: DeviceId,
        err: String,
        retry: bool,
    },
    State {
        id: DeviceId,
        state: Box<DeviceState>,
    },
    ReadFailed {
        id: DeviceId,
        err: String,
    },
    Applied {
        id: DeviceId,
        err: Option<String>,
    },
    /// 構成切替ジョブの結果 (切替+全読み直しの成否)。
    ProfileSwitched {
        id: DeviceId,
        err: Option<String>,
    },
    /// ライブ測定がデバイス側の失敗で止まった (UI の travel_on を戻す)。
    TravelStopped {
        id: DeviceId,
    },
    ActionDone {
        id: DeviceId,
        label: String,
        result: Result<ActionOutcome, String>,
    },
    Battery {
        id: DeviceId,
        battery: Battery,
    },
    Travel {
        id: DeviceId,
        depths: Vec<f32>,
        /// 生データ (`live_axis` 対応ドライバのみ、非対応では空)。
        axis: Vec<devhub_core::AxisSample>,
    },
    /// マウス磁気ボタンの押下深さ (`MouseCaps::live_buttons` と同じ並び)。
    Buttons {
        id: DeviceId,
        buttons: Vec<ButtonDepth>,
    },
    /// ファームウェア更新の確認結果 (失敗は Err で静かに捨てる)。
    FwResult {
        id: DeviceId,
        res: Result<devhub_update::UpdateStatus, String>,
    },
    /// 軸名の取得結果 (ベンダー軸 ID → 名称)。失敗は捨てる。
    AxisNames {
        id: DeviceId,
        names: std::collections::BTreeMap<u16, String>,
    },
    /// 実測レポートレート (Raw Input)。
    PollRate {
        id: DeviceId,
        stats: Option<rawinput::PollStats>,
    },
    /// DirectDrive 送信結果。Err のとき UI は実機プレビューを止めてトーストを出す。
    DirectDrive {
        id: DeviceId,
        err: Option<String>,
    },
    /// 停止要求は通ったが、本体の点灯を復元できなかった。
    DdStopFailed {
        id: DeviceId,
        err: String,
    },
    /// ライブ点灯色の読み戻し結果 (キー index → 実際に光っている色)。
    /// ライティング画面表示中だけワーカーが ~6Hz で送る。非対応では来ない。
    LiveLight {
        id: DeviceId,
        colors: std::collections::BTreeMap<usize, devhub_core::Rgb>,
    },
    Event(HubEvent),
}

enum Job {
    Open,
    Read,
    Apply(Change),
    /// 構成 (オンボードプロファイル) 切替 — 切替後に全設定を読み直す。
    Profile(u8),
    Action {
        id: String,
        label: String,
        file: Option<PathBuf>,
    },
    Battery,
    Travel(bool),
    /// 実測レポートレートを 1 回取る。
    PollRate,
    /// DirectDrive フレーム 1 枚送信 (キー index → 色、ドライバが LED セルへ展開)。
    DirectDrive {
        cells: std::collections::BTreeMap<usize, devhub_core::Rgb>,
    },
    /// DirectDrive を抜けてファーム側点灯に戻す。
    DdStop,
    /// ライブ点灯色ポーリングの ON/OFF (ライティング画面表示中のみ ON)。
    LiveLight(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Opening,
    Ready,
    Failed(String),
}

/// 開いているデバイス 1 台。
pub struct Session {
    pub id: DeviceId,
    pub info: DeviceInfo,
    pub caps: Option<Capabilities>,
    pub state: Option<DeviceState>,
    pub status: Status,
    /// 無線の一時切断や同じパスでの差し直しは次の列挙で開き直す。
    retry_open: bool,
    /// 書き込み待ちの変更数 (ヘッダの同期インジケーター)。
    pub pending: usize,
    /// 構成切替+全読み直しの最中 — 構成内の編集は受け付けない。
    pub switching_profile: bool,
    /// 直近の構成切替が失敗した (app が拾って保留中の復元を破棄する)。
    pub switch_failed: bool,
    /// ワーカーへ投入したジョブの通し番号 (読み込み世代の判定に使う)。
    submit_seq: u64,
    /// 最後に投げた読み込み (Read/Open/Profile) の時点の `submit_seq`。
    /// これより後に投入した未確定の変更は `Msg::State` 着信時に重ねる。
    read_seq: u64,
    /// ワーカーへ送った未確定の変更 (FIFO, `Applied` で先頭から確定していく)。
    pending_changes: VecDeque<(u64, Change)>,
    /// 実行中の操作 (ラベル)。
    pub running: Option<String>,
    /// ストロークテストの深さ (mm)。
    pub travel: Vec<f32>,
    /// ストローク計測の生データ (ADC・状態)。非対応ドライバでは空。
    pub axis: Vec<devhub_core::AxisSample>,
    /// このセッションで観測した最大深さ (mm)。バーの正規化に使う
    /// (公称全ストロークより実測底が浅いため)。
    pub travel_max: f32,
    pub travel_on: bool,
    /// ライティングの「実機でプレビュー」中か (DirectDrive フレームを流す)。
    pub dd_on: bool,
    /// ライティング画面を表示中 — ワーカーが [5,3] で実機の点灯色をポーリングする。
    live_lit: bool,
    /// 最新のライブ点灯色 (キー index → 色)。空 = 未取得/非対応。
    pub live_rgb: std::collections::BTreeMap<usize, devhub_core::Rgb>,
    /// マウス磁気ボタンの押下深さ (`live_buttons`)。
    pub buttons: Vec<ButtonDepth>,
    /// 長い操作の進捗 (ラベル, 0..1)。
    pub progress: Option<(String, Option<f32>)>,
    /// バッテリー低下を通知済みか。
    pub low_battery_warned: bool,
    /// ユーザー起点の読み込み — 完了したらトーストを出す (裏の prefetch では出さない)。
    pub read_notify: bool,
    /// 実測の input report レート。Raw Input で計測、UI が定期的に取得。
    /// 平均 Hz・区間レート・ピーク・間隔統計・移動量上限を含む (docs/POLLING_RATE.md)。
    pub poll: Option<rawinput::PollStats>,
    tx: Sender<Job>,
}

impl Session {
    pub fn ready(&self) -> bool {
        self.status == Status::Ready && self.caps.is_some() && self.state.is_some()
    }
}

pub struct Hub {
    pub backend: Arc<dyn HubBackend>,
    pub demo: bool,
    pub devices: Vec<DeviceInfo>,
    pub sessions: HashMap<DeviceId, Session>,
    /// 初回の scan が返ったか。
    pub scanned: bool,
    /// 列挙の世代 — 発行ごとに増え、古い結果は捨てる (バックエンド切替対策)。
    scan_gen: u64,
    /// ファームウェア更新の最新の確認結果 (DeviceId ごと)。UI のバッジ/カード用。
    pub fw_status: HashMap<DeviceId, devhub_update::UpdateStatus>,
    /// 永続した指紋・最終確認時刻・dismiss (`%APPDATA%\DeviceHub\firmware.json`)。
    pub fw: crate::fwstate::FwState,
    /// 自動確認するか (prefs)。false なら check_firmware は何もしない。
    pub fw_enabled: bool,
    /// Windows 通知にも出すか (prefs)。
    pub fw_native: bool,
    /// 「今すぐ確認」で飛ばしたチェック — 結果を必ずトーストで報告する。
    fw_pending: HashSet<DeviceId>,
    out: UnboundedSender<Msg>,
    scanning: Arc<AtomicBool>,
}

/// 実機バックエンドを開く。`DEVHUB_DEMO=1` か、実機が開けなければデモ。
pub fn connect_backend() -> (Arc<dyn HubBackend>, bool) {
    let force_demo = std::env::var_os("DEVHUB_DEMO").is_some();
    if !force_demo {
        match devhub_hid::connect() {
            Ok(b) => return (b, false),
            Err(e) => eprintln!("[devhub] HID を開けませんでした ({e}) — デモで起動します"),
        }
    }
    (Arc::new(devhub_core::demo::DemoHub::new()), true)
}

impl Hub {
    pub fn new(backend: Arc<dyn HubBackend>, out: UnboundedSender<Msg>) -> Self {
        let demo = backend.is_demo();
        let tx = out.clone();
        backend.watch(Box::new(move |ev| {
            let _ = tx.unbounded_send(Msg::Event(ev));
        }));
        Self {
            backend,
            demo,
            devices: vec![],
            sessions: HashMap::new(),
            scanned: false,
            scan_gen: 0,
            fw_status: HashMap::new(),
            fw: crate::fwstate::FwState::load(),
            fw_enabled: true,
            fw_native: false,
            fw_pending: HashSet::new(),
            out,
            scanning: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 実機が 1 台も無いとき、デモに切り替える (設定画面のボタン)。
    pub fn switch_backend(&mut self, backend: Arc<dyn HubBackend>) {
        self.sessions.clear();
        self.devices.clear();
        self.demo = backend.is_demo();
        self.backend = backend;
        self.scanned = false;
        let tx = self.out.clone();
        self.backend.watch(Box::new(move |ev| {
            let _ = tx.unbounded_send(Msg::Event(ev));
        }));
        self.scan();
    }

    /// 裏スレッドで列挙する (多重起動しない)。
    pub fn scan(&mut self) {
        if self.scanning.swap(true, Ordering::AcqRel) {
            return;
        }
        self.scan_gen += 1;
        let gen = self.scan_gen;
        let b = self.backend.clone();
        let out = self.out.clone();
        let flag = self.scanning.clone();
        std::thread::spawn(move || {
            let r = b.scan().map_err(|e| e.to_string());
            flag.store(false, Ordering::Release);
            let _ = out.unbounded_send(Msg::Scanned { gen, res: r });
        });
    }

    pub fn session(&self, id: &DeviceId) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn session_mut(&mut self, id: &DeviceId) -> Option<&mut Session> {
        self.sessions.get_mut(id)
    }

    /// セッションを開く (既に開いていれば何もしない)。
    /// `notify` = ユーザーが画面を見ている (読み込み完了をトーストで知らせる)。
    pub fn open(&mut self, id: &DeviceId, notify: bool) {
        if let Some(s) = self.sessions.get_mut(id) {
            if notify && !s.ready() {
                s.read_notify = true;
            }
            return;
        }
        let Some(info) = self.devices.iter().find(|d| d.id == *id).cloned() else {
            return;
        };
        let (tx, rx) = mpsc::channel::<Job>();
        let backend = self.backend.clone();
        let out = self.out.clone();
        let did = id.clone();
        std::thread::Builder::new()
            .name(format!("devhub-{}", info.name))
            .spawn(move || worker(backend, did, rx, out))
            .ok();
        let supported = info.supported;
        let _ = tx.send(Job::Open);
        self.sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                info,
                caps: None,
                state: None,
                status: if supported {
                    Status::Opening
                } else {
                    Status::Failed("このデバイスのドライバはまだ実装されていません".into())
                },
                pending: 0,
                running: None,
                travel: vec![],
                axis: vec![],
                travel_max: 0.0,
                travel_on: false,
                dd_on: false,
                live_lit: false,
                live_rgb: Default::default(),
                buttons: vec![],
                progress: None,
                low_battery_warned: false,
                read_notify: notify,
                switching_profile: false,
                retry_open: false,
                switch_failed: false,
                submit_seq: 0,
                read_seq: 0,
                pending_changes: VecDeque::new(),
                poll: None,
                tx,
            },
        );
    }

    /// 変更を UI に即反映し、ワーカーへ送る。
    pub fn apply(&mut self, id: &DeviceId, change: Change) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        // 構成切替+再読の最中は編集を受け付けない (UI 側でもガードする)。
        if s.switching_profile {
            return;
        }
        if let Change::Profile(p) = change {
            // 構成切替は専用ジョブ — 切替と全読み直しが終わるまで状態を変えない。
            let ok = s
                .caps
                .as_ref()
                .and_then(|c| c.profiles.as_ref())
                .is_some_and(|pc| pc.count > 1 && p < pc.count);
            if !ok {
                return;
            }
            s.switching_profile = true;
            s.switch_failed = false;
            if s.tx.send(Job::Profile(p)).is_ok() {
                // 切替ジョブは内部で全読み直しする — ここまでの変更は読み直しに含まれる。
                s.read_seq = s.submit_seq;
                s.pending += 1;
            } else {
                s.switching_profile = false;
            }
            return;
        }
        if let Some(st) = s.state.as_mut() {
            apply_local(st, &change);
        }
        if s.tx.send(Job::Apply(change.clone())).is_ok() {
            s.submit_seq += 1;
            s.pending_changes.push_back((s.submit_seq, change));
            s.pending += 1;
        }
    }

    /// ユーザーが「再読み込み」を押した — 完了トーストを出す。
    pub fn reload(&mut self, id: &DeviceId) {
        self.reload_inner(id, true);
    }

    /// 全セッションへ DirectDrive 停止を送る (アプリ終了時 — 実機が
    /// 最後のフレームのまま固まらないように)。
    pub fn dd_stop_all(&self) {
        for s in self.sessions.values() {
            let _ = s.tx.send(Job::DdStop);
        }
    }

    /// 本体側の変更通知など裏の読み直し (`notify=false` = トーストなし)。
    fn reload_inner(&mut self, id: &DeviceId, notify: bool) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.read_notify = notify;
            if matches!(s.status, Status::Failed(_)) {
                s.status = Status::Opening;
                send_open(s);
            } else {
                send_read(s);
            }
        }
    }

    pub fn run_action(&mut self, id: &DeviceId, action: &str, label: &str, file: Option<PathBuf>) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.running = Some(label.to_string());
            let _ = s.tx.send(Job::Action {
                id: action.into(),
                label: label.into(),
                file,
            });
        }
    }

    /// 実測レポートレートを 1 回取る (UI の定期ポーリング用)。
    pub fn poll_rate(&self, id: &DeviceId) {
        if let Some(s) = self.sessions.get(id) {
            let _ = s.tx.send(Job::PollRate);
        }
    }

    pub fn poll_battery(&self) {
        for s in self.sessions.values() {
            let has_battery = s.caps.as_ref().is_some_and(|c| c.battery);
            if s.ready() && (has_battery || s.info.battery.is_some()) {
                let _ = s.tx.send(Job::Battery);
            }
        }
    }

    pub fn set_travel(&mut self, id: &DeviceId, on: bool) {
        if let Some(s) = self.sessions.get_mut(id) {
            if s.travel_on != on {
                s.travel_on = on;
                if !on {
                    s.travel.clear();
                    s.axis.clear();
                    s.buttons.clear();
                }
                let _ = s.tx.send(Job::Travel(on));
            }
        }
    }

    /// ライティング「実機でプレビュー」の ON/OFF。OFF で DdStop を送って
    /// ファーム側のアニメーションに戻す。
    pub fn set_dd(&mut self, id: &DeviceId, on: bool) {
        if let Some(s) = self.sessions.get_mut(id) {
            if s.dd_on != on {
                s.dd_on = on;
                if !on {
                    let _ = s.tx.send(Job::DdStop);
                }
            }
        }
    }

    /// ライブ点灯色ポーリング: `id` のセッションだけ ON、他は全部 OFF。
    /// `None` で全停止。ライティング画面を出ている間だけ UI が呼ぶ。
    pub fn live_light(&mut self, id: Option<&DeviceId>) {
        for s in self.sessions.values_mut() {
            // caps に live_readback があるエリアが無いデバイスには送らない
            // (非対応ドライバは即 Unsupported を返すが、caps だけ見る原則に揃える)。
            let supported = s
                .caps
                .as_ref()
                .is_some_and(|c| c.lighting.iter().any(|a| a.live_readback));
            let on = Some(&s.id) == id && supported;
            if s.live_lit != on {
                s.live_lit = on;
                if !on {
                    s.live_rgb.clear();
                }
                let _ = s.tx.send(Job::LiveLight(on));
            }
        }
    }

    /// DirectDrive フレーム 1 枚をワーカーへ送る (dd_on 中のみ)。
    /// ワーカーは連続したフレームを間引く。
    pub fn dd_frame(
        &self,
        id: &DeviceId,
        cells: std::collections::BTreeMap<usize, devhub_core::Rgb>,
    ) {
        if let Some(s) = self.sessions.get(id) {
            if s.dd_on && !s.switching_profile && s.ready() {
                let _ = s.tx.send(Job::DirectDrive { cells });
            }
        }
    }

    /// firmware.json のキー — 機種でまとめる (同じ機種が複数台でも 1 件)。
    fn fw_key(info: &DeviceInfo) -> String {
        if info.model.is_empty() {
            info.id.0.clone()
        } else {
            info.model.clone()
        }
    }

    /// 配布元を 1 台分確認する (HTTP は専用スレッド — UI/HID ワーカーは止めない)。
    /// `force` = 「今すぐ確認」で 24h の間引きを飛ばす。確認を飛ばしたなら true。
    pub fn check_firmware(&mut self, id: &DeviceId, force: bool) -> bool {
        if !self.fw_enabled {
            return false;
        }
        let Some(s) = self.sessions.get(id) else {
            return false;
        };
        let Some(feed) = s.caps.as_ref().and_then(|c| c.firmware_feed.clone()) else {
            return false;
        };
        let key = Self::fw_key(&s.info);
        if !self.fw.due(&key, force) {
            return false;
        }
        self.fw.touch(&key);
        self.fw.save();
        let last_fp = self
            .fw
            .devices
            .get(&key)
            .map(|e| e.fingerprint.clone())
            .unwrap_or_default();
        let cur_fw = s.info.firmware.clone();
        let out = self.out.clone();
        let did = id.clone();
        if force {
            self.fw_pending.insert(id.clone());
        }
        std::thread::spawn(move || {
            let res = devhub_update::check(&feed, cur_fw.as_deref(), &last_fp).map_err(|e| e.0);
            let _ = out.unbounded_send(Msg::FwResult { id: did, res });
        });
        true
    }

    /// 「今すぐ確認」— 開いている全セッションの feed を確認する。飛んだ件数。
    pub fn check_all_firmware(&mut self, force: bool) -> usize {
        let ids: Vec<DeviceId> = self.sessions.keys().cloned().collect();
        ids.iter()
            .filter(|id| self.check_firmware(id, force))
            .count()
    }

    /// 更新カード/バッジを閉じる (同じ指紋では再通知しない)。
    pub fn fw_dismiss(&mut self, id: &DeviceId) {
        let Some(st) = self.fw_status.get_mut(id) else {
            return;
        };
        st.new_available = false;
        let fp = st.fingerprint.clone();
        if let Some(s) = self.sessions.get(id) {
            let key = Self::fw_key(&s.info);
            self.fw.devices.entry(key).or_default().dismissed = fp;
            self.fw.save();
        }
    }

    /// ワーカーからの結果を取り込む。戻り値: トーストに出す (文, エラーか)。
    pub fn handle(&mut self, msg: Msg) -> Option<(String, bool)> {
        match msg {
            Msg::Scanned { gen, .. } if gen != self.scan_gen => {
                // バックエンド切替などで遅れて届いた古い列挙は捨てる。
                None
            }
            Msg::Scanned {
                res: Ok(mut list), ..
            } => {
                self.scanned = true;
                // 抜かれたデバイスのセッションを閉じる。
                self.sessions
                    .retain(|id, _| list.iter().any(|d| d.id == *id));
                for d in &mut list {
                    // 前回の一覧/セッションのバッテリー値を引き継ぐ (scan は battery を持たない)。
                    if d.battery.is_none() {
                        d.battery = self
                            .devices
                            .iter()
                            .find(|o| o.id == d.id)
                            .and_then(|o| o.battery)
                            .or_else(|| self.sessions.get(&d.id).and_then(|s| s.info.battery));
                    }
                    if let Some(s) = self.sessions.get_mut(&d.id) {
                        // asleep は scan が知らない — セッション (ドライバ) の値を一覧へ。
                        // 逆方向 (d→s) で上書きすると 2 秒ごとに asleep が消える。
                        d.asleep = s.info.asleep;
                        // ドングル内側機種の確定名も同様 (列挙は受信機名を返す)。
                        if s.info.model != d.model {
                            d.name = s.info.name.clone();
                            d.model = s.info.model.clone();
                        }
                        if d.battery.is_none() {
                            d.battery = s.info.battery;
                        }
                    }
                }
                self.devices = list;
                // バッテリー/通知を得るため、対応デバイスのセッションを裏で開いておく
                // (画面は開かない。open() は既開なら何もしない)。
                let ids: Vec<DeviceId> = self
                    .devices
                    .iter()
                    .filter(|d| d.supported)
                    .map(|d| d.id.clone())
                    .collect();
                for id in ids {
                    match self.sessions.get_mut(&id) {
                        None => self.open(&id, false),
                        Some(s) if s.retry_open && !matches!(s.status, Status::Opening) => {
                            s.status = Status::Opening;
                            send_open(s);
                        }
                        _ => {}
                    }
                }
                None
            }
            Msg::Scanned { res: Err(e), .. } => {
                self.scanned = true;
                Some((format!("デバイスの列挙に失敗: {e}"), true))
            }
            Msg::Opened { id, info, caps } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    if let Some(d) = self.devices.iter_mut().find(|d| d.id == id) {
                        d.asleep = info.asleep;
                        // ドングル内側機種の確定で name/model が変わりうる
                        // (EID 検出 — mousefit8::maybe_detect)。
                        d.name = info.name.clone();
                        d.model = info.model.clone();
                        if info.battery.is_some() {
                            d.battery = info.battery;
                        }
                    }
                    s.info = info;
                    s.caps = Some(*caps);
                    s.retry_open = false;
                    send_read(s);
                    // 軸名を裏で取る (ベンダー API、失敗しても何も変わらない)。
                    if let (Some(bid), Some(pc)) = (
                        s.info.board_id,
                        s.caps
                            .as_ref()
                            .and_then(|c| c.keyboard.as_ref())
                            .and_then(|k| k.perf.as_ref()),
                    ) {
                        if !pc.switch_ids.is_empty() {
                            let (vid, pid) = (s.info.vid, s.info.pid);
                            let out = self.out.clone();
                            let did = id.clone();
                            std::thread::spawn(move || {
                                if let Ok(names) =
                                    devhub_update::sparklink_axis_names(bid, vid, pid)
                                {
                                    let _ = out.unbounded_send(Msg::AxisNames { id: did, names });
                                }
                            });
                        }
                    }
                }
                // 開き終わったら更新を確認 (24h 以内なら中で跳ぶ)。
                self.check_firmware(&id, false);
                None
            }
            Msg::PollRate { id, stats } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.poll = stats;
                }
                None
            }
            Msg::AxisNames { id, names } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    if let Some(pc) = s
                        .caps
                        .as_mut()
                        .and_then(|c| c.keyboard.as_mut())
                        .and_then(|k| k.perf.as_mut())
                    {
                        for (i, id2) in pc.switch_ids.iter().enumerate() {
                            if let Some(n) = names.get(id2) {
                                if let Some(slot) = pc.switch_types.get_mut(i) {
                                    *slot = format!("{n} (軸 {id2})");
                                }
                            }
                        }
                    }
                }
                None
            }
            Msg::OpenFailed { id, err, retry } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.status = Status::Failed(err);
                    s.retry_open = retry;
                    if retry {
                        s.state = None;
                        s.info.asleep = s.info.transport == Transport::Wireless24G;
                    }
                }
                None
            }
            Msg::State { id, state } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    // 読み込みの発行より後に投入した未確定の変更を重ねる — 古い
                    // 読み出しで楽観的な変更を巻き戻さない (それらはまだ実行中)。
                    let mut st = *state;
                    for (seq, c) in &s.pending_changes {
                        if *seq > s.read_seq {
                            apply_local(&mut st, c);
                        }
                    }
                    s.state = Some(st);
                    s.status = Status::Ready;
                    if s.read_notify {
                        s.read_notify = false;
                        return Some((format!("{} の設定を読み込みました", s.info.name), false));
                    }
                }
                None
            }
            Msg::ReadFailed { id, err } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.read_notify = false;
                    if s.state.is_none() {
                        s.status = Status::Failed(err.clone());
                    }
                }
                Some((format!("読み込みに失敗: {err}"), true))
            }
            Msg::DirectDrive { id, err } => {
                let s = self.sessions.get_mut(&id)?;
                if let Some(e) = err {
                    // 失敗したら実機プレビューを止めて実機点灯をファームへ戻し、状態を読み直す。
                    s.dd_on = false;
                    let _ = s.tx.send(Job::DdStop);
                    send_read(s);
                    return Some((format!("実機プレビューに失敗: {e}"), true));
                }
                None
            }
            Msg::DdStopFailed { id, err } => {
                self.sessions.get(&id)?;
                Some((format!("DirectDrive 停止後の点灯復元に失敗: {err}"), true))
            }
            Msg::LiveLight { id, colors } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    if s.live_lit {
                        s.live_rgb = colors;
                    }
                }
                None
            }
            Msg::TravelStopped { id } => {
                let s = self.sessions.get_mut(&id)?;
                if !s.travel_on {
                    return None; // ユーザーが既に止めた後の失敗
                }
                s.travel_on = false;
                s.travel.clear();
                s.axis.clear();
                Some(("ライブ測定がデバイス側で停止しました".into(), true))
            }
            Msg::Applied { id, err } => {
                let s = self.sessions.get_mut(&id)?;
                s.pending = s.pending.saturating_sub(1);
                s.pending_changes.pop_front();
                if let Some(e) = err {
                    // 実機の値へ戻す。
                    send_read(s);
                    return Some((format!("書き込みに失敗: {e}"), true));
                }
                None
            }
            Msg::ProfileSwitched { id, err } => {
                let s = self.sessions.get_mut(&id)?;
                s.switching_profile = false;
                s.pending = s.pending.saturating_sub(1);
                if let Some(e) = err {
                    // 切替か直後の読み直しに失敗 — どの構成の値か分からないので
                    // 状態を捨てて読み直す (古い値を別構成へ書き戻さない)。
                    s.state = None;
                    s.switch_failed = true;
                    send_read(s);
                    return Some((format!("構成の切替に失敗: {e}"), true));
                }
                None
            }
            Msg::ActionDone { id, label, result } => {
                let s = self.sessions.get_mut(&id)?;
                s.running = None;
                match result {
                    Ok(o) => {
                        if o.reload {
                            send_read(s);
                        }
                        Some((
                            o.message
                                .unwrap_or_else(|| format!("{label}: 完了しました")),
                            false,
                        ))
                    }
                    Err(e) => Some((format!("{label}: {e}"), true)),
                }
            }
            Msg::Battery { id, battery } => {
                let mut warn = None;
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.info.battery = Some(battery);
                    if let Some(p) = battery.percent {
                        if p <= 15 && !battery.charging && !s.low_battery_warned {
                            s.low_battery_warned = true;
                            warn = Some((
                                format!("{} のバッテリー残量が {p}% です", s.info.name),
                                true,
                            ));
                        }
                        if p > 30 {
                            s.low_battery_warned = false;
                        }
                    }
                }
                if warn.is_some() {
                    if let Some(d) = self.devices.iter_mut().find(|d| d.id == id) {
                        d.battery = Some(battery);
                    }
                    return warn;
                }
                if let Some(d) = self.devices.iter_mut().find(|d| d.id == id) {
                    d.battery = Some(battery);
                }
                None
            }
            Msg::Travel { id, depths, axis } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    if s.travel_on {
                        s.travel_max = depths.iter().fold(s.travel_max, |a, &b| a.max(b));
                        s.travel = depths;
                        s.axis = axis;
                    }
                }
                None
            }
            Msg::Buttons { id, buttons } => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    if s.travel_on {
                        s.buttons = buttons;
                    }
                }
                None
            }
            Msg::FwResult { id, res } => {
                let manual = self.fw_pending.remove(&id);
                let mut st = match res {
                    Ok(st) => st,
                    Err(e) => {
                        // 手動確認の失敗は報告、自動確認の失敗は静かに捨てる。
                        return manual.then(|| (format!("ファームウェアの確認に失敗: {e}"), true));
                    }
                };
                let s = self.sessions.get(&id)?;
                let key = Self::fw_key(&s.info);
                let name = s.info.name.clone();
                // 同じ指紋を以前 dismiss していれば今回も出さない。
                let dismissed =
                    self.fw.devices.get(&key).is_some_and(|e| {
                        e.dismissed == st.fingerprint && !st.fingerprint.is_empty()
                    });
                let entry = self.fw.devices.entry(key).or_default();
                if !st.fingerprint.is_empty() {
                    entry.fingerprint = st.fingerprint.clone();
                }
                self.fw.save();
                st.new_available = st.new_available && !dismissed;
                let fresh = st.new_available;
                self.fw_status.insert(id, st);
                if !fresh {
                    return manual.then(|| (format!("{name} のファームウェアは最新です"), false));
                }
                if self.fw_native {
                    let _ = notify_rust::Notification::new()
                        .summary("Device HUB")
                        .body(&format!("{name} の新しいファームウェアが公開されています"))
                        .show();
                }
                Some((
                    format!("{name} の新しいファームウェアが公開されています"),
                    false,
                ))
            }
            Msg::Event(ev) => {
                match ev {
                    HubEvent::DevicesChanged => self.scan(),
                    HubEvent::StateChanged(id) => self.reload_inner(&id, false),
                    HubEvent::Battery(id, b) => {
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.info.battery = Some(b);
                        }
                        if let Some(d) = self.devices.iter_mut().find(|d| d.id == id) {
                            d.battery = Some(b);
                        }
                    }
                    HubEvent::Info(id, info) => {
                        // ドライバが掴んだ休眠/バッテリーの変化をセッションと一覧へ。
                        let mut woke = false;
                        // EID 検出などで機種 (name/model) が確定・変更された → caps も変わる
                        // ので Job::Open で info+caps を取り直す。
                        let mut renamed = false;
                        if let Some(s) = self.sessions.get_mut(&id) {
                            // 休眠→覚醒の遷移: 寝ている間に読まれた空の状態を読み直す。
                            woke = s.info.asleep && !info.asleep;
                            renamed = s.info.model != info.model || s.info.name != info.name;
                            s.info.asleep = info.asleep;
                            if info.battery.is_some() {
                                s.info.battery = info.battery;
                            }
                            if renamed {
                                s.info.name = info.name.clone();
                                s.info.model = info.model.clone();
                                let _ = s.tx.send(Job::Open);
                            }
                        }
                        if let Some(d) = self.devices.iter_mut().find(|d| d.id == id) {
                            d.asleep = info.asleep;
                            if renamed {
                                d.name = info.name.clone();
                                d.model = info.model.clone();
                            }
                            if info.battery.is_some() {
                                d.battery = info.battery;
                            }
                        }
                        if woke {
                            self.reload_inner(&id, false);
                        }
                    }
                    HubEvent::Progress {
                        id,
                        label,
                        fraction,
                        done,
                    } => {
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.progress = if done { None } else { Some((label, fraction)) };
                        }
                    }
                }
                None
            }
        }
    }
}

// ------------------------------------------------------------------ ワーカー

/// 読み込み/再オープンを要求する — 世代を記録するので、それ以降に投入した
/// 未確定の変更は `Msg::State` 着信時に確定状態へ重ねる (巻き戻さない)。
fn send_read(s: &mut Session) {
    s.read_seq = s.submit_seq;
    let _ = s.tx.send(Job::Read);
}

fn send_open(s: &mut Session) {
    s.read_seq = s.submit_seq;
    let _ = s.tx.send(Job::Open);
}

/// 連続するジョブだけを間引く。途中の読み込み・操作や別の変更は先行する書き込みを
/// 前提にすることがある。省いた Apply も UI の書き込み待ち件数には含まれる。
fn coalesce_jobs(jobs: impl IntoIterator<Item = Job>) -> (Vec<Job>, usize) {
    let mut kept = Vec::new();
    let mut skipped_applies = 0;
    for job in jobs {
        let replace = match (kept.last(), &job) {
            (Some(Job::Apply(prev)), Job::Apply(next))
                if !matches!((prev, next), (Change::PeqAll { .. }, Change::Peq { .. }))
                    && prev.same_target(next) =>
            {
                skipped_applies += 1;
                true
            }
            (Some(Job::DirectDrive { .. }), Job::DirectDrive { .. }) => true,
            _ => false,
        };
        if replace {
            kept.pop();
        }
        kept.push(job);
    }
    (kept, skipped_applies)
}

fn worker(
    backend: Arc<dyn HubBackend>,
    id: DeviceId,
    rx: mpsc::Receiver<Job>,
    out: UnboundedSender<Msg>,
) {
    let mut driver: Option<Arc<dyn DeviceDriver>> = None;
    let mut vidpid: Option<(u16, u16)> = None;
    let mut travel = false;
    // ライブ点灯色ポーリング (ライティング画面表示中のみ)。~33ms 間隔 —
    // 実測で 9 チャンクの往復は ~3.4ms (litpoll)。エフェクトの動きが滑らかに見える程度。
    let mut live_lit = false;
    let mut live_at = std::time::Instant::now();
    let send = |m: Msg| out.unbounded_send(m).is_ok();
    loop {
        let first = if travel || live_lit {
            // travel 中は深さ優先の 33ms、点灯色ポーリングも同じ 33ms 周期。
            let wait = 33;
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(j) => Some(j),
                Err(_) => return,
            }
        };
        let (kept, skipped) = coalesce_jobs(first.into_iter().chain(rx.try_iter()));
        for _ in 0..skipped {
            if !send(Msg::Applied {
                id: id.clone(),
                err: None,
            }) {
                return;
            }
        }
        for job in kept {
            let ok = match job {
                Job::Open => match backend.driver(&id) {
                    Ok(d) => {
                        let info = d.info();
                        let caps = d.caps();
                        vidpid = Some((info.vid, info.pid));
                        driver = Some(d);
                        send(Msg::Opened {
                            id: id.clone(),
                            info,
                            caps: Box::new(caps),
                        })
                    }
                    Err(e) => send(Msg::OpenFailed {
                        id: id.clone(),
                        err: e.to_string(),
                        retry: matches!(e, HubError::Disconnected),
                    }),
                },
                Job::Read => match driver.as_ref().map(|d| d.read()) {
                    Some(Ok(st)) => send(Msg::State {
                        id: id.clone(),
                        state: Box::new(st),
                    }),
                    Some(Err(e)) => {
                        if matches!(e, HubError::Disconnected) {
                            backend.invalidate(&id);
                            driver = None; // 古いハンドルを捨て、再オープンで取り直す。
                            send(Msg::OpenFailed {
                                id: id.clone(),
                                err: e.to_string(),
                                retry: true,
                            })
                        } else {
                            send(Msg::ReadFailed {
                                id: id.clone(),
                                err: e.to_string(),
                            })
                        }
                    }
                    None => true,
                },
                Job::Apply(c) => {
                    let (err, dead) = match &driver {
                        Some(d) => match d.apply(&c) {
                            Ok(()) => (None, false),
                            Err(e) => (
                                Some(e.to_string()),
                                matches!(e, devhub_core::backend::HubError::Disconnected),
                            ),
                        },
                        None => (Some("デバイスが開かれていません".into()), false),
                    };
                    if dead {
                        backend.invalidate(&id);
                        driver = None;
                        send(Msg::OpenFailed {
                            id: id.clone(),
                            err: HubError::Disconnected.to_string(),
                            retry: true,
                        });
                    }
                    send(Msg::Applied {
                        id: id.clone(),
                        err,
                    })
                }
                // 構成切替: 書き込みに成功したら全設定を読み直してから報告する。
                Job::Profile(p) => {
                    let (err, dead) = match &driver {
                        Some(d) => match d.apply(&Change::Profile(p)) {
                            Ok(()) => match d.read() {
                                Ok(st) => {
                                    send(Msg::State {
                                        id: id.clone(),
                                        state: Box::new(st),
                                    });
                                    (None, false)
                                }
                                Err(e) => (
                                    Some(e.to_string()),
                                    matches!(e, devhub_core::backend::HubError::Disconnected),
                                ),
                            },
                            Err(e) => (
                                Some(e.to_string()),
                                matches!(e, devhub_core::backend::HubError::Disconnected),
                            ),
                        },
                        None => (Some("デバイスが開かれていません".into()), false),
                    };
                    if dead {
                        backend.invalidate(&id);
                        driver = None;
                    }
                    send(Msg::ProfileSwitched {
                        id: id.clone(),
                        err,
                    })
                }
                Job::Action {
                    id: aid,
                    label,
                    file,
                } => {
                    let (result, dead) = match &driver {
                        Some(d) => match d.run_action(&aid, file.as_deref()) {
                            Ok(o) => (Ok(o), false),
                            Err(e) => (
                                Err(e.to_string()),
                                matches!(e, devhub_core::backend::HubError::Disconnected),
                            ),
                        },
                        None => (Err("デバイスが開かれていません".into()), false),
                    };
                    if dead {
                        backend.invalidate(&id);
                        driver = None;
                    }
                    send(Msg::ActionDone {
                        id: id.clone(),
                        label,
                        result,
                    })
                }
                Job::Battery => match driver.as_ref().map(|d| d.battery()) {
                    Some(Ok(b)) => send(Msg::Battery {
                        id: id.clone(),
                        battery: b,
                    }),
                    Some(Err(HubError::Disconnected)) => {
                        backend.invalidate(&id);
                        driver = None;
                        send(Msg::OpenFailed {
                            id: id.clone(),
                            err: HubError::Disconnected.to_string(),
                            retry: true,
                        })
                    }
                    _ => true,
                },
                Job::PollRate => {
                    // Raw Input の到達レート (プロセス内計測 — ドライバ不要)。
                    send(Msg::PollRate {
                        id: id.clone(),
                        stats: vidpid.and_then(|(v, p)| rawinput::stats(v, p)),
                    })
                }
                Job::Travel(on) => {
                    travel = on;
                    true
                }
                Job::DirectDrive { cells } => {
                    let err = match &driver {
                        Some(d) => d.direct_drive(0, &cells).err().map(|e| e.to_string()),
                        None => Some("デバイスが開かれていません".into()),
                    };
                    send(Msg::DirectDrive {
                        id: id.clone(),
                        err,
                    })
                }
                Job::DdStop => {
                    let result = driver.as_ref().map(|d| d.direct_drive_stop(0));
                    match result {
                        Some(Err(e)) if !matches!(e, HubError::Unsupported(_)) => {
                            // 終了時は UI が先に閉じている可能性がある。
                            send(Msg::DdStopFailed {
                                id: id.clone(),
                                err: e.to_string(),
                            })
                        }
                        _ => true,
                    }
                }
                Job::LiveLight(on) => {
                    live_lit = on;
                    // 即座に 1 回読めるよう過去時刻にしておく。
                    live_at = std::time::Instant::now()
                        .checked_sub(Duration::from_millis(33))
                        .unwrap_or_else(std::time::Instant::now);
                    true
                }
            };
            if !ok {
                return;
            }
        }
        // 点灯色ポーリング — 非対応ドライバは即 Unsupported を返すので
        // 失敗しても次の tick でまた試すだけ (HID は叩かれない)。
        if live_lit && live_at.elapsed() >= Duration::from_millis(33) {
            live_at = std::time::Instant::now();
            if let Some(d) = &driver {
                if let Ok(colors) = d.live_lighting(0) {
                    if !send(Msg::LiveLight {
                        id: id.clone(),
                        colors,
                    }) {
                        return;
                    }
                }
            }
        }
        if travel {
            if let Some(d) = &driver {
                // マウス磁気ボタン → live_buttons。キーボードは live_axis
                // (生データ) 優先、非対応なら live_travel。
                if let Ok(buttons) = d.live_buttons() {
                    if !send(Msg::Buttons {
                        id: id.clone(),
                        buttons,
                    }) {
                        return;
                    }
                    continue;
                }
                let got = match d.live_axis() {
                    Ok(a) => Ok((a.iter().map(|s| s.route).collect::<Vec<_>>(), a)),
                    Err(_) => d.live_travel().map(|v| (v, Vec::new())),
                };
                match got {
                    Ok((depths, axis)) => {
                        if !send(Msg::Travel {
                            id: id.clone(),
                            depths,
                            axis,
                        }) {
                            return;
                        }
                    }
                    Err(_) => {
                        travel = false;
                        send(Msg::TravelStopped { id: id.clone() });
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ 楽観的な反映

/// `Change` を UI 側の状態へ反映する (ドライバの書き込みと同じ意味)。
pub fn apply_local(s: &mut DeviceState, change: &Change) {
    match change.clone() {
        Change::Setting { id, value } => {
            s.settings.insert(id, value);
        }
        Change::Profile(p) => s.profile.active = p,
        Change::ProfileName { index, name } => {
            if let Some(n) = s.profile.names.get_mut(index as usize) {
                *n = name;
            }
        }
        Change::DpiStages { stages, active } => {
            if let Some(m) = s.mouse.as_mut() {
                m.stages = stages;
                m.active_stage = active.min(m.stages.len().saturating_sub(1));
            }
        }
        Change::ActiveStage(a) => {
            if let Some(m) = s.mouse.as_mut() {
                m.active_stage = a;
            }
        }
        Change::Button { slot, action } => {
            if let Some(b) = s.mouse.as_mut().and_then(|m| m.buttons.get_mut(slot)) {
                *b = action;
            }
        }
        Change::Macro { slot, def } => {
            let list = if let Some(m) = s.mouse.as_mut() {
                &mut m.macros
            } else if let Some(k) = s.keyboard.as_mut() {
                &mut k.macros
            } else {
                return;
            };
            if slot < list.len() {
                list[slot] = def;
            }
        }
        Change::Lighting { area, state } => {
            s.lighting.insert(area, state);
        }
        Change::KeyMap { layer, key, action } => {
            if let Some(slot) = s
                .keyboard
                .as_mut()
                .and_then(|k| k.keymap.get_mut(layer as usize))
                .and_then(|l| l.get_mut(key))
            {
                *slot = action;
            }
        }
        Change::KeyMapReset { .. } => {
            // 既定値は UI が知らない。ワーカーの後に読み直す (Applied 後に Read を送る)。
        }
        Change::KeyPerf { keys, perf } => {
            if let Some(k) = s.keyboard.as_mut() {
                for i in keys {
                    if let Some(p) = k.perf.get_mut(i) {
                        *p = perf;
                    }
                }
            }
        }
        Change::Advanced {
            key,
            layer,
            binding,
        } => {
            if let Some(k) = s.keyboard.as_mut() {
                k.advanced.retain(|a| !(a.key == key && a.layer == layer));
                if let Some(b) = binding {
                    k.advanced.push(AdvancedKey {
                        key,
                        layer,
                        binding: b,
                    });
                }
            }
        }
        Change::Peq { band, value } => {
            if let Some(b) = s.peq.as_mut().and_then(|p| p.get_mut(band)) {
                *b = value;
            }
        }
        Change::PeqAll { bands } => {
            if s.peq.is_some() {
                s.peq = Some(bands);
            }
        }
        Change::Adaptive(curve) => {
            if let Some(m) = s.mouse.as_mut().filter(|m| m.adaptive.is_some()) {
                m.adaptive = Some(curve);
            }
        }
    }
}

// ------------------------------------------------------------------ 差分

/// `cur` を `target` に揃えるための Change 列 (caps の範囲内だけ)。
/// 元に戻す / やり直し、プリセットの適用、ファイルからの読み込みで共用する。
pub fn diff_changes(caps: &Capabilities, cur: &DeviceState, target: &DeviceState) -> Vec<Change> {
    let mut out: Vec<Change> = vec![];
    for (id, v) in &target.settings {
        if caps.setting(id).is_some() && cur.settings.get(id) != Some(v) {
            out.push(Change::Setting {
                id: id.clone(),
                value: v.clone(),
            });
        }
    }
    if let (Some(t), Some(c), Some(mc)) = (&target.mouse, &cur.mouse, &caps.mouse) {
        if (t.stages != c.stages || t.active_stage != c.active_stage)
            && !t.stages.is_empty()
            && t.stages.len() <= mc.max_stages
        {
            if t.stages == c.stages {
                out.push(Change::ActiveStage(t.active_stage));
            } else {
                out.push(Change::DpiStages {
                    stages: t.stages.clone(),
                    active: t.active_stage,
                });
            }
        }
        for (i, b) in t.buttons.iter().enumerate().take(mc.buttons.len()) {
            if c.buttons.get(i) != Some(b) {
                out.push(Change::Button {
                    slot: i,
                    action: b.clone(),
                });
            }
        }
        for (i, d) in t.macros.iter().enumerate() {
            if i < c.macros.len() && c.macros.get(i) != Some(d) {
                out.push(Change::Macro {
                    slot: i,
                    def: d.clone(),
                });
            }
        }
    }
    if let (Some(t), Some(c), Some(kc)) = (&target.keyboard, &cur.keyboard, &caps.keyboard) {
        for (l, layer) in t.keymap.iter().enumerate().take(kc.layers.len()) {
            for (i, a) in layer.iter().enumerate().take(kc.layout.len()) {
                if c.keymap.get(l).and_then(|x| x.get(i)) != Some(a) {
                    out.push(Change::KeyMap {
                        layer: l as u8,
                        key: i,
                        action: *a,
                    });
                }
            }
        }
        // 同じ設定のキーはまとめて 1 件にする (書き込み回数を減らす)。
        let mut groups: Vec<(KeyPerf, Vec<usize>)> = vec![];
        for (i, pf) in t.perf.iter().enumerate().take(kc.layout.len()) {
            if c.perf.get(i) != Some(pf) {
                match groups.iter_mut().find(|g| g.0 == *pf) {
                    Some(g) => g.1.push(i),
                    None => groups.push((*pf, vec![i])),
                }
            }
        }
        for (pf, keys) in groups {
            out.push(Change::KeyPerf { keys, perf: pf });
        }
        for a in &c.advanced {
            if !t
                .advanced
                .iter()
                .any(|b| b.key == a.key && b.layer == a.layer)
            {
                out.push(Change::Advanced {
                    key: a.key,
                    layer: a.layer,
                    binding: None,
                });
            }
        }
        for a in &t.advanced {
            if !c.advanced.contains(a) {
                out.push(Change::Advanced {
                    key: a.key,
                    layer: a.layer,
                    binding: Some(a.binding.clone()),
                });
            }
        }
        for (i, d) in t.macros.iter().enumerate() {
            if i < c.macros.len() && c.macros.get(i) != Some(d) {
                out.push(Change::Macro {
                    slot: i,
                    def: d.clone(),
                });
            }
        }
    }
    for (a, l) in &target.lighting {
        if caps.lighting.iter().any(|x| x.id == *a) && cur.lighting.get(a) != Some(l) {
            out.push(Change::Lighting {
                area: *a,
                state: l.clone(),
            });
        }
    }
    if let (Some(t), Some(c), Some(pc)) = (&target.peq, &cur.peq, &caps.peq) {
        if t != c && t.len() == pc.bands as usize {
            out.push(Change::PeqAll { bands: t.clone() });
        }
    }
    if let (Some(t), Some(c), Some(_)) = (
        target.mouse.as_ref().and_then(|m| m.adaptive.as_ref()),
        cur.mouse.as_ref().and_then(|m| m.adaptive.as_ref()),
        caps.mouse.as_ref().and_then(|m| m.adaptive_dpi.as_ref()),
    ) {
        if t != c {
            out.push(Change::Adaptive(t.clone()));
        }
    }
    // 構成の切替自体はここでは出さない — `Job::Profile` で切替+再読をひとまとめにする。
    // (切替前の差分を切替後の構成へ書かないため)。名前だけは通常の書き込みで戻せる。
    if caps.profiles.as_ref().is_some_and(|pc| pc.renamable) {
        for (i, n) in target.profile.names.iter().enumerate() {
            if cur.profile.names.get(i) != Some(n) {
                out.push(Change::ProfileName {
                    index: i as u8,
                    name: n.clone(),
                });
            }
        }
    }
    out
}

/// 現在の `DeviceState` 全体を `Change` 列にする (プロファイルの「今の状態を記録」)。
/// キーマップ・ボタン・マクロ・高度キーは含まない — 全書き換えは書き込み数が
/// 多く EEPROM も摩耗するので、スカラー設定・DPI・性能・ライティング・EQ に絞る。
pub fn snapshot_changes(caps: &Capabilities, st: &DeviceState) -> Vec<Change> {
    let mut out = vec![];
    for (id, v) in &st.settings {
        if caps.setting(id).is_some() {
            out.push(Change::Setting {
                id: id.clone(),
                value: v.clone(),
            });
        }
    }
    if let (Some(m), Some(mc)) = (&st.mouse, &caps.mouse) {
        if !m.stages.is_empty() && m.stages.len() <= mc.max_stages {
            out.push(Change::DpiStages {
                stages: m.stages.clone(),
                active: m.active_stage,
            });
        }
    }
    if let (Some(k), Some(kc)) = (&st.keyboard, &caps.keyboard) {
        // 同じ設定のキーはまとめて 1 件にする (diff_changes と同じく)。
        let mut groups: Vec<(devhub_core::KeyPerf, Vec<usize>)> = vec![];
        for (i, pf) in k.perf.iter().enumerate().take(kc.layout.len()) {
            match groups.iter_mut().find(|g| g.0 == *pf) {
                Some(g) => g.1.push(i),
                None => groups.push((*pf, vec![i])),
            }
        }
        for (perf, keys) in groups {
            out.push(Change::KeyPerf { keys, perf });
        }
    }
    for (a, l) in &st.lighting {
        if caps.lighting.iter().any(|x| x.id == *a) {
            out.push(Change::Lighting {
                area: *a,
                state: l.clone(),
            });
        }
    }
    if let (Some(t), Some(pc)) = (&st.peq, &caps.peq) {
        if t.len() == pc.bands as usize {
            out.push(Change::PeqAll { bands: t.clone() });
        }
    }
    if let (Some(t), Some(_)) = (
        st.mouse.as_ref().and_then(|m| m.adaptive.as_ref()),
        caps.mouse.as_ref().and_then(|m| m.adaptive_dpi.as_ref()),
    ) {
        out.push(Change::Adaptive(t.clone()));
    }
    out
}

/// 読み込んだ `DeviceState` (プリセット/エクスポート JSON) がこのデバイスに適用できるか。
/// `Err` = 別機種向けなどで適用不可。`Ok(Some(msg))` = 警告付きで適用できる。
pub fn state_compatible(caps: &Capabilities, st: &DeviceState) -> Result<Option<String>, String> {
    if st.keyboard.is_some() && caps.keyboard.is_none() {
        return Err("キーボード向けの設定ファイルです".into());
    }
    if st.mouse.is_some() && caps.mouse.is_none() {
        return Err("マウス向けの設定ファイルです".into());
    }
    if let Some(bands) = &st.peq {
        match &caps.peq {
            Some(pc) if bands.len() == pc.bands as usize => {}
            Some(_) => return Err("EQ のバンド数が合いません".into()),
            None => return Err("EQ 対応デバイスの設定ファイルです".into()),
        }
    }
    // そのままでは使えない項目を数える (diff_changes が実際に落とす分)。
    let mut ignored = 0usize;
    for id in st.settings.keys() {
        if caps.setting(id).is_none() {
            ignored += 1;
        }
    }
    for a in st.lighting.keys() {
        if !caps.lighting.iter().any(|x| x.id == *a) {
            ignored += 1;
        }
    }
    if let (Some(k), Some(kc)) = (&st.keyboard, &caps.keyboard) {
        ignored += k.keymap.len().saturating_sub(kc.layers.len()) * kc.layout.len();
        for layer in k.keymap.iter().take(kc.layers.len()) {
            ignored += layer.len().saturating_sub(kc.layout.len());
        }
    }
    if ignored > 0 {
        Ok(Some(format!("{ignored} 件の未対応設定は無視されます")))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod worker_queue_tests {
    use super::*;

    fn band(gain_db: f32) -> PeqBand {
        PeqBand {
            freq: 1000,
            q: 1.,
            gain: gain_db,
            filter: PeqFilter::Peak,
        }
    }

    #[test]
    fn one_band_edit_preserves_the_rest_of_a_queued_eq_preset() {
        let (jobs, skipped) = coalesce_jobs([
            Job::Apply(Change::PeqAll {
                bands: vec![band(3.), band(5.)],
            }),
            Job::Apply(Change::Peq {
                band: 0,
                value: band(7.),
            }),
        ]);
        assert_eq!(skipped, 0);
        assert!(matches!(jobs.as_slice(), [
            Job::Apply(Change::PeqAll { bands }),
            Job::Apply(Change::Peq { band: 0, value }),
        ] if bands[1].gain == 5. && value.gain == 7.));
    }

    #[test]
    fn consecutive_writes_are_coalesced_without_crossing_read_or_profile_switch() {
        let stage = |i| Job::Apply(Change::ActiveStage(i));
        let (jobs, skipped) = coalesce_jobs([
            stage(0),
            stage(1),
            Job::Read,
            stage(2),
            Job::Apply(Change::Profile(1)),
            stage(3),
        ]);
        assert_eq!(skipped, 1);
        assert!(matches!(
            jobs.as_slice(),
            [
                Job::Apply(Change::ActiveStage(1)),
                Job::Read,
                Job::Apply(Change::ActiveStage(2)),
                Job::Apply(Change::Profile(1)),
                Job::Apply(Change::ActiveStage(3)),
            ]
        ));
    }

    #[test]
    fn direct_drive_frames_respect_stop_and_coalesce_within_a_run() {
        let frame = |i| Job::DirectDrive {
            cells: [(i, Rgb::new(0, 0, 0))].into(),
        };
        let (jobs, skipped) = coalesce_jobs([frame(0), frame(1), Job::DdStop, frame(2)]);
        assert_eq!(skipped, 0);
        assert!(matches!(jobs.as_slice(), [
            Job::DirectDrive { cells: before }, Job::DdStop, Job::DirectDrive { cells: after },
        ] if before.contains_key(&1) && after.contains_key(&2)));
    }

    #[test]
    fn profile_switch_job_is_never_coalesced_into_writes() {
        // 構成切替は専用ジョブ — 直前・直後の書き込みと間引きで混ざらない。
        let stage = |i| Job::Apply(Change::ActiveStage(i));
        let (jobs, skipped) = coalesce_jobs([stage(0), Job::Profile(2), stage(1)]);
        assert_eq!(skipped, 0);
        assert!(matches!(
            jobs.as_slice(),
            [
                Job::Apply(Change::ActiveStage(0)),
                Job::Profile(2),
                Job::Apply(Change::ActiveStage(1)),
            ]
        ));
    }

    fn demo_kb() -> (Arc<dyn DeviceDriver>, Capabilities, DeviceState) {
        let hub = devhub_core::demo::DemoHub::new();
        let dev = hub
            .scan()
            .unwrap()
            .into_iter()
            .find(|d| d.kind == DeviceKind::Keyboard)
            .unwrap();
        let driver = hub.driver(&dev.id).unwrap();
        let caps = driver.caps();
        let state = driver.read().unwrap();
        (driver, caps, state)
    }

    #[test]
    fn demo_keeps_settings_separate_per_profile() {
        // 構成ごとに中身が分かれる (実機と同じ) — P1 への undo が P2 を壊さないための前提。
        let (driver, _, _) = demo_kb();
        driver
            .apply(&Change::KeyMap {
                layer: 0,
                key: 0,
                action: KeyAction::Code(0x1234),
            })
            .unwrap();
        driver.apply(&Change::Profile(1)).unwrap();
        let p1 = driver.read().unwrap();
        assert_eq!(p1.profile.active, 1);
        assert_ne!(
            p1.keyboard.as_ref().unwrap().keymap[0][0],
            KeyAction::Code(0x1234),
            "構成 1 に構成 0 の変更が漏れている"
        );
        driver.apply(&Change::Profile(0)).unwrap();
        let p0 = driver.read().unwrap();
        assert_eq!(
            p0.keyboard.as_ref().unwrap().keymap[0][0],
            KeyAction::Code(0x1234),
            "構成 0 の変更が失われた"
        );
    }

    #[test]
    fn profile_switch_blocks_edits_until_the_reread_lands() {
        let backend: Arc<dyn HubBackend> = Arc::new(devhub_core::demo::DemoHub::new());
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
        let mut hub = Hub::new(backend, tx);
        hub.scan();
        let mut kb = None;
        let t = std::time::Instant::now();
        while kb.is_none() && t.elapsed() < Duration::from_secs(3) {
            match rx.try_recv() {
                Ok(m) => {
                    if let Msg::Scanned { res: Ok(ds), .. } = &m {
                        kb = ds
                            .iter()
                            .find(|d| d.kind == DeviceKind::Keyboard)
                            .map(|d| d.id.clone());
                    }
                    hub.handle(m);
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let id = kb.expect("demo keyboard");
        hub.open(&id, false);
        assert!(drain_until(
            &mut hub,
            &mut rx,
            Duration::from_secs(5),
            |h| h.session(&id).is_some_and(|s| s.ready())
        ));

        // 構成切替を送る → 切替+再読が終わるまで editing 不可・state は旧構成のまま。
        hub.apply(&id, Change::Profile(1));
        assert!(hub.session(&id).unwrap().switching_profile);
        assert_eq!(
            hub.session(&id)
                .unwrap()
                .state
                .as_ref()
                .unwrap()
                .profile
                .active,
            0
        );
        let pending = hub.session(&id).unwrap().pending;
        hub.apply(
            &id,
            Change::ProfileName {
                index: 0,
                name: "x".into(),
            },
        );
        assert_eq!(
            hub.session(&id).unwrap().pending,
            pending,
            "切替中の編集がワーカーへ流れた"
        );
        assert!(drain_until(
            &mut hub,
            &mut rx,
            Duration::from_secs(5),
            |h| {
                h.session(&id).is_some_and(|s| {
                    !s.switching_profile
                        && s.state.as_ref().is_some_and(|st| st.profile.active == 1)
                })
            }
        ));
    }

    fn drain_until(
        hub: &mut Hub,
        rx: &mut futures::channel::mpsc::UnboundedReceiver<Msg>,
        wait: Duration,
        pred: impl Fn(&Hub) -> bool,
    ) -> bool {
        let t = std::time::Instant::now();
        while t.elapsed() < wait {
            match rx.try_recv() {
                Ok(m) => {
                    hub.handle(m);
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
            if pred(hub) {
                return true;
            }
        }
        false
    }

    #[test]
    fn diff_changes_emits_profile_names_but_never_a_switch() {
        let (_, caps, cur) = demo_kb();
        let mut target = cur.clone();
        target.profile.names = vec!["A".into(), "B".into(), "C".into(), "D".into()];
        target.profile.active = 2;
        let changes = diff_changes(&caps, &cur, &target);
        assert!(
            changes.iter().all(|c| !matches!(c, Change::Profile(_))),
            "構成切替が差分として出た — 切替前の差分を切替後の構成へ書いてしまう"
        );
        assert_eq!(
            changes
                .iter()
                .filter(|c| matches!(c, Change::ProfileName { .. }))
                .count(),
            4,
            "プロファイル名の差分が出ていない"
        );
    }

    #[test]
    fn state_replays_writes_issued_after_the_read_started() {
        // 読み込みの発行より後に投入した変更は、古い State 着信で巻き戻されない。
        let backend: Arc<dyn HubBackend> = Arc::new(devhub_core::demo::DemoHub::new());
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
        let mut hub = Hub::new(backend, tx);
        hub.scan();
        let mut kb = None;
        let t = std::time::Instant::now();
        while kb.is_none() && t.elapsed() < Duration::from_secs(3) {
            if let Ok(m) = rx.try_recv() {
                if let Msg::Scanned { res: Ok(ds), .. } = &m {
                    kb = ds
                        .iter()
                        .find(|d| d.kind == DeviceKind::Keyboard)
                        .map(|d| d.id.clone());
                }
                hub.handle(m);
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let id = kb.expect("demo keyboard");
        hub.open(&id, false);
        assert!(drain_until(
            &mut hub,
            &mut rx,
            Duration::from_secs(5),
            |h| h.session(&id).is_some_and(|s| s.ready())
        ));

        // 変更前の状態 (読み込みがこの値を返したとする)。
        let stale = hub.backend.driver(&id).unwrap().read().unwrap();
        hub.apply(
            &id,
            Change::ProfileName {
                index: 0,
                name: "RENAMED".into(),
            },
        );
        // 古い読み出しが届いても、確定待ちの変更は維持される。
        hub.handle(Msg::State {
            id: id.clone(),
            state: Box::new(stale),
        });
        assert_eq!(
            hub.session(&id)
                .unwrap()
                .state
                .as_ref()
                .unwrap()
                .profile
                .names[0],
            "RENAMED"
        );
        // 確定したあとに届く新しい読み出しはそのまま反映される。
        drain_until(&mut hub, &mut rx, Duration::from_secs(5), |h| {
            h.session(&id).is_some_and(|s| s.pending_changes.is_empty())
        });
    }

    #[test]
    fn state_compatible_rejects_other_devices_and_flags_unknown_keys() {
        let (_, caps, st) = demo_kb();
        // キーボードの状態をマウス caps へ → 適用不可。
        let mouse_caps = devhub_core::demo::mouse_caps(false, false);
        assert!(state_compatible(&mouse_caps, &st).is_err());
        // 同じ caps → そのまま使える。
        assert_eq!(state_compatible(&caps, &st).unwrap(), None);
        // 不明な設定 id は警告 (無視される) として数える。
        let mut extra = st.clone();
        extra
            .settings
            .insert("nonexistent".into(), SettingValue::Bool(true));
        assert!(matches!(state_compatible(&caps, &extra), Ok(Some(_))));
        // EQ 非対応デバイスへ EQ データ → 適用不可。
        let mut peq = st.clone();
        peq.peq = Some(vec![PeqBand {
            freq: 1000,
            q: 1.,
            gain: 0.,
            filter: PeqFilter::Peak,
        }]);
        assert!(state_compatible(&caps, &peq).is_err());
    }
}
