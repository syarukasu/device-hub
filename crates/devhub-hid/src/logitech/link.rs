//! HID++ feature discovery and matching of vendor input/output reports.

use crate::transport::Channel;
use devhub_core::backend::{HubError, HubResult};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LONG_REPORT: u8 = 0x11;
const SHORT_REPORT: u8 = 0x10;
const REPORT_LEN: usize = 20;
pub(super) const DEVICE_INFO: u16 = 0x0003;
const DEVICE_NAME: u16 = 0x0005;

pub(super) struct Link {
    io: Channel,
    slot: u8,
    software_id: u8,
    features: BTreeMap<u16, u8>,
    pub model: u16,
    pub unit: [u8; 4],
}

impl Link {
    pub fn bind(device: hidapi::HidDevice, wireless: bool, model: u16) -> HubResult<Self> {
        let mut link = Self::new(Channel::open(device, REPORT_LEN, REPORT_LEN));
        let slots: &[u8] = if wireless {
            &[1, 2, 3, 4, 5, 6]
        } else {
            &[0xff]
        };
        let mut other_model = false;
        for &slot in slots {
            link.slot = slot;
            let ping = link.exchange(0, 1, &[0, 0, 0xa5], Duration::from_millis(260));
            // The echo distinguishes a HID++ 2.0 ping from a HID++ 1.0 receiver error.
            if !matches!(ping, Ok(ref data) if data.len() >= 3 && data[0] >= 2 && data[2] == 0xa5) {
                continue;
            }
            let Some(index) = link.feature_index(DEVICE_INFO)? else {
                continue;
            };
            let response = link.command(index, 0, &[])?;
            if !has_model(&response, model) {
                other_model = true;
                continue;
            }
            link.unit = response[1..5].try_into().expect("validated device info");
            link.model = model;
            link.features.insert(DEVICE_INFO, index);
            return Ok(link);
        }
        if other_model {
            Err(HubError::Unsupported(
                "レシーバーに接続された Logicool マウス",
            ))
        } else {
            Err(HubError::Disconnected)
        }
    }

    fn new(io: Channel) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        Self {
            io,
            slot: 0xff,
            software_id: 3 + (seed % 9) as u8,
            features: BTreeMap::new(),
            model: 0,
            unit: [0; 4],
        }
    }

    pub fn discover(&mut self, features: &[u16]) -> HubResult<()> {
        for &feature in features {
            if let Some(index) = self.feature_index(feature)? {
                self.features.insert(feature, index);
            }
        }
        Ok(())
    }

    fn feature_index(&mut self, feature: u16) -> HubResult<Option<u8>> {
        let result = self.command(0, 0, &feature.to_be_bytes())?;
        Ok(result.first().copied().filter(|&index| index != 0))
    }

    pub fn has(&self, feature: u16) -> bool {
        self.features.contains_key(&feature)
    }

    pub fn call(&mut self, feature: u16, function: u8, args: &[u8]) -> HubResult<Vec<u8>> {
        let index = *self
            .features
            .get(&feature)
            .ok_or(HubError::Unsupported("HID++ 機能"))?;
        self.command(index, function, args)
    }

    pub fn confirm_unit(&mut self) -> HubResult<()> {
        // A different mouse could be paired to the same receiver while the app is running.
        let info = self.call(DEVICE_INFO, 0, &[])?;
        if !has_model(&info, self.model) || info[1..5] != self.unit {
            return Err(HubError::Disconnected);
        }
        Ok(())
    }

    pub fn read_name(&mut self) -> HubResult<String> {
        let len = usize::from(self.call(DEVICE_NAME, 0, &[])?[0]);
        if !(1..=64).contains(&len) {
            return Err(protocol("HID++ 機種名の長さ"));
        }
        let mut name = Vec::with_capacity(len);
        while name.len() < len {
            let offset = name.len() as u8;
            let chunk = self.call(DEVICE_NAME, 1, &[offset])?;
            name.extend_from_slice(&chunk[..chunk.len().min(len - name.len())]);
        }
        String::from_utf8(name).map_err(|_| protocol("HID++ 機種名の文字コード"))
    }

    pub fn firmware_at(&mut self, receiver: bool) -> Option<String> {
        let current = self.slot;
        if receiver {
            self.slot = 0xff;
        }
        let firmware = (|| {
            let index = self.feature_index(DEVICE_INFO).ok()??;
            let count = self.command(index, 0, &[]).ok()?[0].min(16);
            // Main firmware is kind 0 and bootloader is kind 1.
            let mut loader = None;
            for i in 0..count {
                let Ok(reported) = self.command(index, 1, &[i]) else {
                    continue;
                };
                if reported.len() < 8 || reported[0] > 1 {
                    continue;
                }
                let label = std::str::from_utf8(&reported[1..4]).ok()?;
                let version = format!("{label} {:02X}.{:02X}", reported[4], reported[5]);
                if reported[0] == 0 {
                    return Some(version);
                }
                loader = Some(version);
            }
            loader
        })();
        self.slot = current;
        firmware
    }

    fn command(&mut self, feature: u8, function: u8, args: &[u8]) -> HubResult<Vec<u8>> {
        self.exchange(feature, function, args, Duration::from_millis(450))
    }

    fn exchange(
        &mut self,
        feature: u8,
        function: u8,
        args: &[u8],
        timeout: Duration,
    ) -> HubResult<Vec<u8>> {
        if function > 15 || args.len() > REPORT_LEN - 4 {
            return Err(HubError::Invalid("HID++ リクエストのサイズ".into()));
        }
        // Leave space for notifications (0), the kernel (1), and G HUB (0x0d).
        self.software_id = if self.software_id >= 11 {
            3
        } else {
            self.software_id + 1
        };
        let address = (function << 4) | self.software_id;
        let mut body = [0; REPORT_LEN - 1];
        body[..3].copy_from_slice(&[self.slot, feature, address]);
        body[3..3 + args.len()].copy_from_slice(args);
        self.io.send_report(LONG_REPORT, &body).map_err(io_error)?;

        let deadline = Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let raw = self
                .io
                .recv_optional(remaining.as_millis().max(1) as i32)
                .map_err(io_error)?
                .ok_or(HubError::Disconnected)?;
            if let Some(reply) = decode_reply(&raw, self.slot, feature, address) {
                return reply.map(<[u8]>::to_vec).map_err(|code| {
                    protocol(format!(
                        "HID++ index 0x{feature:02x} / fn {function}: 0x{code:02x}"
                    ))
                });
            }
        }
        Err(HubError::Disconnected)
    }
}

fn io_error(err: HubError) -> HubError {
    match err {
        HubError::Io(_) => HubError::Disconnected,
        _ => err,
    }
}

fn has_model(info: &[u8], expected: u16) -> bool {
    // Model IDs follow the transport mask (BT/LE, wireless PID, USB PID).
    info.get(7..13)
        .is_some_and(|ids| ids.chunks_exact(2).any(|id| id == expected.to_be_bytes()))
        && info[1..5] != [0; 4]
        && info[1..5] != [0xff; 4]
}

fn decode_reply(data: &[u8], slot: u8, feature: u8, address: u8) -> Option<Result<&[u8], u8>> {
    let len = match data.first()? {
        &SHORT_REPORT => 7,
        &LONG_REPORT => REPORT_LEN,
        _ => return None,
    };
    if data.len() < len || data[1] != slot {
        return None;
    }
    match data[2..6] {
        [f, a, ..] if f == feature && a == address => Some(Ok(&data[4..len])),
        [0xff | 0x8f, f, a, code] if f == feature && a == address => Some(Err(code)),
        _ => None,
    }
}

pub(super) fn protocol(reason: impl Into<String>) -> HubError {
    HubError::Protocol(reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x3_receiver_ignores_other_slots_clients_and_notifications() {
        let io = Channel::mock();
        // The final frame is the response to 0x1004.GetStatus from the receiver's slot 1.
        for packet in [
            [0x11, 2, 7, 0x14, 94, 8, 0],
            [0x11, 1, 7, 0x1d, 94, 8, 0],
            [0x11, 1, 7, 0x10, 94, 8, 0],
            [0x11, 1, 7, 0x14, 94, 8, 0],
        ] {
            let mut report = packet.to_vec();
            report.resize(REPORT_LEN, 0);
            io.mock_reply(&report);
        }

        let mut link = Link::new(io);
        link.slot = 1;
        link.software_id = 3;
        assert_eq!(link.command(7, 1, &[]).unwrap()[..3], [94, 8, 0]);
        assert_eq!(link.io.mock_sent()[0][..4], [0x11, 1, 7, 0x14]);
    }

    #[test]
    fn hidpp_errors_match_the_original_function_and_client() {
        let nack = [0x10, 1, 0x8f, 7, 0x14, 0x06, 0];
        assert_eq!(decode_reply(&nack, 1, 7, 0x14), Some(Err(6)));
        assert_eq!(decode_reply(&nack, 1, 7, 0x15), None);
    }
}
