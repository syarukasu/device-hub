use super::link::{protocol, Link};
use devhub_core::backend::{HubError, HubResult};
use devhub_core::model::{Battery, ChoiceOpt};
use serde::{Deserialize, Serialize};

pub const DEVICE_NAME: u16 = 0x0005;
pub const BASIC_BATTERY: u16 = 0x1000;
pub const VOLTAGE_BATTERY: u16 = 0x1001;
pub const UNIFIED_BATTERY: u16 = 0x1004;
const BASIC_DPI: u16 = 0x2201;
const EXTENDED_DPI: u16 = 0x2202;
const BASIC_RATE: u16 = 0x8060;
const EXTENDED_RATE: u16 = 0x8061;
const ONBOARD: u16 = 0x8100;
const ANALOG: u16 = 0x1b0c;

pub const FEATURES: &[u16] = &[
    DEVICE_NAME,
    UNIFIED_BATTERY,
    BASIC_BATTERY,
    VOLTAGE_BATTERY,
    EXTENDED_DPI,
    BASIC_DPI,
    EXTENDED_RATE,
    BASIC_RATE,
    ONBOARD,
    ANALOG,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sensitivity {
    pub x: u16,
    pub y: Option<u16>,
    pub lod: Option<u8>,
}

pub struct Dpi {
    feature: u16,
    pub x: Vec<u16>,
    pub y: Option<Vec<u16>>,
    /// Wire values for low, medium and high, if supported.
    pub lod: Option<[u8; 3]>,
}

impl Dpi {
    pub fn discover(link: &mut Link) -> HubResult<Option<Self>> {
        if link.has(EXTENDED_DPI) {
            let caps = link.call(EXTENDED_DPI, 1, &[0])?;
            Ok(Some(Self {
                feature: EXTENDED_DPI,
                x: dpi_axis(link, EXTENDED_DPI, 0)?,
                y: if caps[2] & 1 != 0 {
                    Some(dpi_axis(link, EXTENDED_DPI, 1)?)
                } else {
                    None
                },
                lod: (caps[2] & 2 != 0).then_some(if link.model == 0xc0a9 {
                    [1, 2, 3]
                } else {
                    [0, 1, 2]
                }),
            }))
        } else if link.has(BASIC_DPI) {
            Ok(Some(Self {
                feature: BASIC_DPI,
                x: dpi_axis(link, BASIC_DPI, 0)?,
                y: None,
                lod: None,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn read(&self, link: &mut Link) -> HubResult<Sensitivity> {
        let extended = self.feature == EXTENDED_DPI;
        let response = link.call(self.feature, if extended { 5 } else { 2 }, &[0])?;
        if response.first() != Some(&0) || response.len() < if extended { 10 } else { 5 } {
            return Err(protocol("HID++ DPI の応答サイズまたはセンサー番号"));
        }
        let value = |offset| {
            let current = be(&response, offset);
            if current == 0 {
                be(&response, offset + 2)
            } else {
                current
            }
        };
        Ok(Sensitivity {
            x: value(1),
            y: self.y.as_ref().map(|_| value(5)),
            lod: self.lod.map(|_| response[9]),
        })
    }

    pub fn accepts(&self, value: Sensitivity) -> bool {
        self.x.binary_search(&value.x).is_ok()
            && match (&self.y, value.y) {
                (Some(allowed), Some(y)) => allowed.binary_search(&y).is_ok(),
                (None, None) => true,
                _ => false,
            }
            && match (self.lod, value.lod) {
                (Some(allowed), Some(level)) => allowed.contains(&level),
                (None, None) => true,
                _ => false,
            }
    }

    pub fn snap(&self, x: u32, y: Option<u32>, lod: Option<u8>) -> HubResult<Sensitivity> {
        let nearest = |values: &[u16], requested: u32| -> HubResult<u16> {
            let min = u32::from(values[0]);
            let max = u32::from(values[values.len() - 1]);
            if !(min..=max).contains(&requested) {
                return Err(HubError::Invalid(format!("DPI の範囲は {min}–{max}")));
            }
            Ok(*values
                .iter()
                .min_by_key(|&&d| u32::from(d).abs_diff(requested))
                .unwrap())
        };
        Ok(Sensitivity {
            x: nearest(&self.x, x)?,
            y: self
                .y
                .as_ref()
                .map(|axis| nearest(axis, y.unwrap_or(x)))
                .transpose()?,
            lod,
        })
    }

    pub fn write(&self, link: &mut Link, value: Sensitivity) -> HubResult<()> {
        if !self.accepts(value) {
            return Err(HubError::Invalid("DPI / リフトオフ設定".into()));
        }
        let x = value.x.to_be_bytes();
        if self.feature == EXTENDED_DPI {
            let y = value.y.unwrap_or(0).to_be_bytes();
            link.call(
                self.feature,
                6,
                &[0, x[0], x[1], y[0], y[1], value.lod.unwrap_or(0)],
            )?;
        } else {
            link.call(self.feature, 3, &[0, x[0], x[1]])?;
        }
        require_readback(self.read(link)? == value, "DPI / リフトオフ")
    }
}

fn dpi_axis(link: &mut Link, feature: u16, direction: u8) -> HubResult<Vec<u16>> {
    let mut bytes = Vec::new();
    for page in 0..16 {
        let ext = feature == EXTENDED_DPI;
        let data = link.call(feature, if ext { 2 } else { 1 }, &[0, direction, page])?;
        let header = if ext {
            &[0, direction, page][..]
        } else {
            &[0][..]
        };
        if !data.starts_with(header) || data.len() < header.len() + 2 {
            return Err(protocol("HID++ DPI 一覧のヘッダー"));
        }
        bytes.extend_from_slice(&data[header.len()..]);
        if !ext || bytes.ends_with(&[0, 0]) {
            return decode_dpi_values(&bytes);
        }
    }
    Err(protocol("HID++ DPI 一覧が終了しません"))
}

fn decode_dpi_values(bytes: &[u8]) -> HubResult<Vec<u16>> {
    let mut values = Vec::<u16>::new();
    let mut words = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
    while let Some(word) = words.next() {
        if word == 0 {
            break;
        }
        if word & 0xe000 == 0xe000 {
            let step = word & 0x1fff;
            let last = values.last().copied().unwrap_or(0);
            let end = words.next().unwrap_or(0);
            if last == 0 || step == 0 || end <= last || end >= 0xe000 || end - last < step {
                return Err(protocol("HID++ DPI 一覧に不正な範囲"));
            }
            values.extend((last + step..=end).step_by(usize::from(step)));
        } else if values.last().is_none_or(|&last| last < word) {
            values.push(word);
        } else {
            return Err(protocol("HID++ DPI 一覧の順序"));
        }
    }
    if values.is_empty() {
        return Err(protocol("HID++ DPI 一覧が空です"));
    }
    Ok(values)
}

pub struct ReportRate {
    feature: u16,
    connection: u8,
    pub options: Vec<ChoiceOpt>,
    rates: Vec<(u8, u16)>,
}

impl ReportRate {
    pub fn discover(link: &mut Link, wireless: bool) -> HubResult<Option<Self>> {
        let connection = u8::from(wireless);
        let (feature, rates) = if link.has(EXTENDED_RATE) {
            let flags = be(&link.call(EXTENDED_RATE, 1, &[connection])?, 0);
            let rates = [125, 250, 500, 1000, 2000, 4000, 8000]
                .into_iter()
                .enumerate()
                .filter(|(code, _)| flags & (1 << code) != 0)
                .map(|(code, hz)| (code as u8, hz))
                .collect::<Vec<_>>();
            (EXTENDED_RATE, rates)
        } else if link.has(BASIC_RATE) {
            let flags = link.call(BASIC_RATE, 0, &[])?[0];
            let rates = (1u8..=8)
                .rev()
                .filter(|ms| flags & (1 << (ms - 1)) != 0)
                .map(|ms| (ms, (1000 + u16::from(ms) / 2) / u16::from(ms)))
                .collect();
            (BASIC_RATE, rates)
        } else {
            return Ok(None);
        };
        if rates.is_empty() {
            return Ok(None);
        }
        let options = rates
            .iter()
            .map(|&(_, hz)| ChoiceOpt::new(hz.into(), format!("{hz} Hz")))
            .collect();
        Ok(Some(Self {
            feature,
            connection,
            options,
            rates,
        }))
    }

    pub fn read(&self, link: &mut Link) -> HubResult<u16> {
        let code = if self.feature == EXTENDED_RATE {
            // Function 2 returns the *wired* rate when the connection byte is zero.
            link.call(self.feature, 2, &[self.connection])?[0]
        } else {
            link.call(self.feature, 1, &[])?[0]
        };
        self.rates
            .iter()
            .find(|&&(key, _)| key == code)
            .map(|&(_, hz)| hz)
            .ok_or_else(|| protocol(format!("未対応のポーリング設定コード {code}")))
    }

    pub fn write(&self, link: &mut Link, hz: u16) -> HubResult<()> {
        let code = self
            .rates
            .iter()
            .find(|&&(_, rate)| rate == hz)
            .map(|&(code, _)| code)
            .ok_or_else(|| HubError::Invalid(format!("ポーリング {hz} Hz")))?;
        link.call(
            self.feature,
            if self.feature == EXTENDED_RATE { 3 } else { 2 },
            &[code],
        )?;
        require_readback(self.read(link)? == hz, "ポーリングレート")
    }
}

pub struct Onboard {
    pub sectors: Vec<u16>,
}

impl Onboard {
    pub fn discover(link: &mut Link) -> HubResult<Option<Self>> {
        if !link.has(ONBOARD) {
            return Ok(None);
        }
        let info = link.call(ONBOARD, 0, &[])?;
        if info.len() < 9 {
            return Err(protocol("HID++ オンボードの情報が短すぎます"));
        }
        let count = info[3].min(16);
        let mut directory = 0;
        let mut data = Self::read_directory(link, directory, 0)?;
        if data.starts_with(&[0, 0, 0, 0]) || data.starts_with(&[0xff; 4]) {
            directory = 0x100;
            data = Self::read_directory(link, directory, 0)?;
        }
        let mut sectors = Vec::new();
        for index in 0..count {
            if index != 0 && index % 4 == 0 {
                data = Self::read_directory(link, directory, index * 4)?;
            }
            let offset = usize::from(index % 4) * 4;
            let sector = be(&data, offset);
            if sector == 0xffff {
                break;
            }
            let low = sector as u8;
            if data[offset + 2] == 1 && (1..=16).contains(&low) && sector <= 0x0110 {
                sectors.push(sector);
            }
        }
        Ok(Some(Self { sectors }))
    }

    fn read_directory(link: &mut Link, sector: u16, offset: u8) -> HubResult<Vec<u8>> {
        let [hi, lo] = sector.to_be_bytes();
        let data = link.call(ONBOARD, 5, &[hi, lo, 0, offset])?;
        if data.len() != 16 {
            return Err(protocol("HID++ プロファイルディレクトリが短すぎます"));
        }
        Ok(data)
    }

    pub fn host(link: &mut Link) -> HubResult<bool> {
        match link.call(ONBOARD, 2, &[])?[0] {
            1 => Ok(false),
            2 => Ok(true),
            other => Err(protocol(format!("未対応のオンボードモード {other}"))),
        }
    }

    pub fn current(&self, link: &mut Link) -> HubResult<u16> {
        if Self::host(link)? {
            return Ok(0);
        }
        let data = link.call(ONBOARD, 4, &[])?;
        Ok(be(&data, 0))
    }

    pub fn select(&self, link: &mut Link, sector: u16) -> HubResult<()> {
        if sector != 0 && !self.sectors.contains(&sector) {
            return Err(HubError::Invalid(
                "有効な本体プロファイルを選択してください".into(),
            ));
        }
        let was_host = Self::host(link)?;
        let to_host = sector == 0;
        if was_host != to_host {
            link.call(ONBOARD, 1, &[if to_host { 2 } else { 1 }])?;
        }
        if to_host {
            return require_readback(Self::host(link)?, "PC モード");
        }
        let result = link
            .call(ONBOARD, 3, &sector.to_be_bytes())
            .and_then(|_| require_readback(self.current(link)? == sector, "本体プロファイル"));
        if result.is_err() && was_host {
            // Return to the original mode if the firmware rejected the new profile.
            let _ = link.call(ONBOARD, 1, &[2]);
        }
        result
    }

    pub fn choices(&self) -> Vec<ChoiceOpt> {
        std::iter::once(ChoiceOpt::new(0, "PC で設定"))
            .chain(self.sectors.iter().map(|&sector| {
                let kind = if sector > 0xff {
                    "既定プロファイル"
                } else {
                    "本体プロファイル"
                };
                ChoiceOpt::new(sector.into(), format!("{kind} {}", sector as u8))
            }))
            .collect()
    }
}

pub struct Analog {
    pub count: u8,
    pub max: [u8; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonTuning {
    pub levels: [u8; 3],
    pub rapid_trigger: bool,
}

impl Analog {
    pub fn discover(link: &mut Link) -> HubResult<Option<Self>> {
        if !link.has(ANALOG) {
            return Ok(None);
        }
        let data = link.call(ANALOG, 0, &[])?;
        if data.len() < 5 {
            return Err(protocol("HID++ クリック設定の情報が短すぎます"));
        }
        let count = data[1].min(2);
        let max = [data[2] >> 2, data[3] >> 2, data[4] >> 2];
        Ok((count > 0 && max[0] > 0 && max[1] > 0).then_some(Self { count, max }))
    }

    fn wire(&self, link: &mut Link, button: u8) -> HubResult<[u8; 4]> {
        let data = link.call(ANALOG, 2, &[button])?;
        if button >= self.count || data.first() != Some(&button) {
            return Err(protocol("HID++ クリック設定のボタン番号"));
        }
        data.get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| protocol("HID++ クリック設定の応答が短すぎます"))
    }

    pub fn read(&self, link: &mut Link, button: u8) -> HubResult<ButtonTuning> {
        let [_, point, trigger, haptic] = self.wire(link, button)?;
        Ok(ButtonTuning {
            levels: [point >> 2, trigger >> 2, haptic >> 2],
            rapid_trigger: trigger & 1 != 0,
        })
    }

    pub fn write(&self, link: &mut Link, button: u8, tuning: ButtonTuning) -> HubResult<()> {
        let [point, trigger, haptic] = tuning.levels;
        if button >= self.count
            || tuning.levels.iter().zip(self.max).any(|(&v, max)| v > max)
            || point == 0
            || (tuning.rapid_trigger && trigger == 0)
        {
            return Err(HubError::Invalid("クリック設定の範囲".into()));
        }
        let wire = [
            button,
            point << 2,
            trigger << 2 | u8::from(tuning.rapid_trigger),
            haptic << 2,
        ];
        link.call(ANALOG, 1, &wire)?;
        require_readback(self.read(link, button)? == tuning, "クリック設定")
    }
}

pub fn battery_feature(link: &Link) -> Option<u16> {
    [UNIFIED_BATTERY, BASIC_BATTERY, VOLTAGE_BATTERY]
        .into_iter()
        .find(|&id| link.has(id))
}

pub fn battery(link: &mut Link, feature: u16) -> HubResult<Battery> {
    let data = link.call(feature, u8::from(feature == UNIFIED_BATTERY), &[])?;
    let (percent, charging) = match feature {
        UNIFIED_BATTERY | BASIC_BATTERY => (
            Some(data[0]).filter(|v| *v <= 100),
            matches!(data[2], 1 | 2 | 4),
        ),
        VOLTAGE_BATTERY => {
            // Older devices give voltage. Interpolate a Li-ion discharge curve.
            const CURVE: [(u16, u8); 6] = [
                (3500, 0),
                (3650, 10),
                (3720, 25),
                (3790, 50),
                (3960, 75),
                (4186, 100),
            ];
            let mv = be(&data, 0).clamp(CURVE[0].0, CURVE[5].0);
            let pair = CURVE.windows(2).find(|p| mv <= p[1].0).unwrap();
            let (low, p) = pair[0];
            let (high, q) = pair[1];
            (
                Some(p + ((mv - low) * u16::from(q - p) / (high - low)) as u8),
                data[2] & 0x80 != 0 && data[2] & 3 != 3,
            )
        }
        _ => return Err(HubError::Unsupported("電池残量")),
    };
    Ok(Battery { percent, charging })
}

fn be(bytes: &[u8], index: usize) -> u16 {
    u16::from_be_bytes([bytes[index], bytes[index + 1]])
}

fn require_readback(matched: bool, setting: &str) -> HubResult<()> {
    if matched {
        Ok(())
    } else {
        Err(protocol(format!("{setting} の読み戻しが一致しません")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x3_dpi_ranges_include_mixed_steps() {
        let data = [
            0x00, 0x64, 0xe0, 0x01, 0x00, 0xc8, 0xe0, 0x02, 0x01, 0xf4, 0, 0,
        ];
        let values = decode_dpi_values(&data).unwrap();
        assert!(values.contains(&101));
        assert!(values.contains(&500));
        assert!(!values.contains(&201));
        assert!(decode_dpi_values(&[0, 100, 0xe0, 0, 0, 200]).is_err());
    }
}
