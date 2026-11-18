//! HID トランスポートの薄い層。
//!
//! レポート送受信を 1 か所に集め、各ドライバ (xsyd.rs / mousefit.rs) は
//! プロトコルの意味づけだけを書く。

use devhub_core::backend::{HubError, HubResult};

/// HID 列挙器を開く。Windows では特別な権限は要らない
/// (vendor-defined usage page のデバイスはユーザー権限で読み書きできる)。
pub fn open() -> HubResult<hidapi::HidApi> {
    hidapi::HidApi::new().map_err(|e| HubError::Io(format!("hidapi init: {e}")))
}

/// 1 デバイスとのレポートチャネル。
///
/// TODO(backend): 実装のたたき台。
/// - レポート ID 先頭の 64/65 バイト送受信 (`write` は先頭に report ID を置く)
/// - タイムアウト付き read
/// - 必要なら feature report (`send_feature_report` / `get_feature_report`)
///   — mouse.fit 系は feature report だけで通す機種が多い
#[allow(dead_code)]
pub struct Channel {
    inner: Inner,
    /// デバイスの IN レポート長 (report ID 込み)。0 なら 64 を仮定。
    pub in_report_len: usize,
    /// OUT レポート長。
    pub out_report_len: usize,
}

enum Inner {
    /// `HidDevice` は Send だが Sync ではない (内部バックエンドが非スレッドセーフ)。
    /// Mutex で包んで Sync にする。呼び出しはドライバのワーカー 1 本からなので競合しない。
    Hid(std::sync::Mutex<hidapi::HidDevice>),
    #[cfg(test)]
    Mock(std::sync::Mutex<MockIo>),
}

/// テスト用の送受信スタブ: send は記録、recv はキューから順に返す。
#[cfg(test)]
#[derive(Default)]
pub struct MockIo {
    pub sent: Vec<Vec<u8>>,
    pub replies: std::collections::VecDeque<Vec<u8>>,
    fail_send: Option<Vec<u8>>,
}

#[allow(dead_code)]
impl Channel {
    pub fn open(dev: hidapi::HidDevice, in_len: usize, out_len: usize) -> Self {
        Self {
            inner: Inner::Hid(std::sync::Mutex::new(dev)),
            in_report_len: in_len.max(1),
            out_report_len: out_len.max(1),
        }
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        Self {
            inner: Inner::Mock(std::sync::Mutex::new(MockIo::default())),
            in_report_len: 64,
            out_report_len: 64,
        }
    }

    /// テスト用: recv が順に返す応答を積む。
    #[cfg(test)]
    pub fn mock_reply(&self, reply: &[u8]) {
        if let Inner::Mock(m) = &self.inner {
            m.lock()
                .unwrap_or_else(|p| p.into_inner())
                .replies
                .push_back(reply.to_vec());
        }
    }

    /// テスト用: send に来た全 body。
    #[cfg(test)]
    pub fn mock_sent(&self) -> Vec<Vec<u8>> {
        match &self.inner {
            Inner::Mock(m) => m.lock().unwrap_or_else(|p| p.into_inner()).sent.clone(),
            _ => vec![],
        }
    }

    /// テスト用: ヘッダが一致した次の送信だけ I/O エラーにする。
    #[cfg(test)]
    pub fn mock_fail_send(&self, prefix: &[u8]) {
        if let Inner::Mock(m) = &self.inner {
            m.lock().unwrap_or_else(|p| p.into_inner()).fail_send = Some(prefix.to_vec());
        }
    }

    /// `body` を report ID 0 先頭の OUT レポートとして送る。
    /// パディングは 0 埋め。
    pub fn send(&self, body: &[u8]) -> HubResult<()> {
        match &self.inner {
            Inner::Hid(dev) => {
                let mut buf = vec![0u8; self.out_report_len];
                let n = body.len().min(buf.len() - 1);
                buf[1..1 + n].copy_from_slice(&body[..n]);
                dev.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .write(&buf)
                    .map_err(|e| HubError::Io(format!("hid write: {e}")))?;
                Ok(())
            }
            #[cfg(test)]
            Inner::Mock(m) => {
                let mut io = m.lock().unwrap_or_else(|p| p.into_inner());
                io.sent.push(body.to_vec());
                if io
                    .fail_send
                    .as_ref()
                    .is_some_and(|prefix| body.starts_with(prefix))
                {
                    io.fail_send = None;
                    return Err(HubError::Io("mock: write failed".into()));
                }
                Ok(())
            }
        }
    }

    /// 番号付きレポート: `body` を report ID `rid` の OUT レポートとして送る
    /// (buf[0]=rid)。レポート番号を持つデバイス (WalkPlay の 0x4B 等) 用。
    pub fn send_report(&self, rid: u8, body: &[u8]) -> HubResult<()> {
        match &self.inner {
            Inner::Hid(dev) => {
                let mut buf = vec![0u8; self.out_report_len];
                buf[0] = rid;
                let n = body.len().min(buf.len() - 1);
                buf[1..1 + n].copy_from_slice(&body[..n]);
                dev.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .write(&buf)
                    .map_err(|e| HubError::Io(format!("hid write: {e}")))?;
                Ok(())
            }
            #[cfg(test)]
            Inner::Mock(m) => {
                let mut f = vec![rid];
                f.extend_from_slice(body);
                m.lock().unwrap_or_else(|p| p.into_inner()).sent.push(f);
                Ok(())
            }
        }
    }

    /// `timeout_ms` 以内の IN レポート 1 件。番号付きなら report ID を含む。
    pub fn recv(&self, timeout_ms: i32) -> HubResult<Vec<u8>> {
        self.recv_optional(timeout_ms)?
            .ok_or_else(|| HubError::Protocol("read timeout".into()))
    }

    /// タイムアウトを I/O エラーと区別して返す。
    pub fn recv_optional(&self, timeout_ms: i32) -> HubResult<Option<Vec<u8>>> {
        match &self.inner {
            Inner::Hid(dev) => {
                let mut buf = vec![0u8; self.in_report_len];
                let n = dev
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .read_timeout(&mut buf, timeout_ms)
                    .map_err(|e| HubError::Io(format!("hid read: {e}")))?;
                // unnumbered report は buf[0] がデータ先頭。numbered なら buf[0] が report ID。
                // 判別は呼び側のプロトコルが担うので生のまま返す。
                Ok((n != 0).then(|| buf[..n].to_vec()))
            }
            #[cfg(test)]
            Inner::Mock(m) => Ok(m
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .replies
                .pop_front()),
        }
    }

    /// 64B feature report を送る (report ID 0)。mouse.fit 系の全コマンドに使う。
    pub fn send_feature(&self, body: &[u8; 64]) -> HubResult<()> {
        match &self.inner {
            Inner::Hid(dev) => {
                let mut buf = [0u8; 65];
                buf[1..].copy_from_slice(body);
                dev.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .send_feature_report(&buf)
                    .map_err(|e| HubError::Io(format!("hid feature write: {e}")))?;
                Ok(())
            }
            #[cfg(test)]
            Inner::Mock(m) => {
                m.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .sent
                    .push(body.to_vec());
                Ok(())
            }
        }
    }

    /// 64B feature report を読む (report ID 0)。返り値は report ID 込みの 65B。
    /// `get_feature_report` は要求→応答の往復がカーネル側で 1 トランザクションなので
    /// タイムアウトは OS 依存 (通常 ~1s)。失敗時は Err を返し呼び側がリトライする。
    pub fn get_feature(&self) -> HubResult<[u8; 65]> {
        match &self.inner {
            Inner::Hid(dev) => {
                let mut buf = [0u8; 65];
                let n = dev
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_feature_report(&mut buf)
                    .map_err(|e| HubError::Io(format!("hid feature read: {e}")))?;
                if n < 2 {
                    return Err(HubError::Protocol("empty feature report".into()));
                }
                Ok(buf)
            }
            #[cfg(test)]
            Inner::Mock(m) => {
                let mut buf = [0u8; 65];
                if let Some(r) = m
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .replies
                    .pop_front()
                {
                    let n = r.len().min(64);
                    buf[1..1 + n].copy_from_slice(&r[..n]);
                }
                Ok(buf)
            }
        }
    }
}
