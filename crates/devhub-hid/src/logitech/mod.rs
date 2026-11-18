//! Logitech PRO Wireless and PRO X3 SUPERSTRIKE via the HID++ vendor collection.

mod features;
mod link;

use crate::Watcher;
use devhub_core::backend::*;
use devhub_core::model::*;
use features::{Analog, ButtonTuning, Dpi, Onboard, ReportRate, Sensitivity};
use link::Link;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const PROFILE: &str = "logitech_profile";
const RATE: &str = "polling";
const LOD: &str = "lod";
const SAVE: &str = "logitech_save_startup";
const HOST_ONLY: &str = "「使用するプロファイル」で「PC で設定」を選んでください。";
const RT_TOGGLES: [(&str, &str); 2] = [
    (
        "logitech_left_rt_on",
        "左クリックの Rapid Trigger を有効にする",
    ),
    (
        "logitech_right_rt_on",
        "右クリックの Rapid Trigger を有効にする",
    ),
];

const ANALOG_SETTINGS: [[(&str, &str); 3]; 2] = [
    [
        ("logitech_left_point", "左クリックの作動点"),
        ("logitech_left_rt", "左クリックの Rapid Trigger"),
        ("logitech_left_haptic", "左クリックの振動"),
    ],
    [
        ("logitech_right_point", "右クリックの作動点"),
        ("logitech_right_rt", "右クリックの Rapid Trigger"),
        ("logitech_right_haptic", "右クリックの振動"),
    ],
];

pub(super) fn open(
    device: hidapi::HidDevice,
    mut info: DeviceInfo,
    events: Option<Watcher>,
) -> HubResult<Logitech> {
    let product = if matches!(info.pid, 0xc54f | 0xc0a9) {
        0xc0a9
    } else {
        0xc088
    };
    let wireless = info.transport == Transport::Wireless24G;
    let mut link = Link::bind(device, wireless, product)?;
    link.discover(features::FEATURES)?;
    if let Ok(name) = link.read_name() {
        info.name = name;
    }
    info.firmware = link.firmware_at(false);
    if wireless && info.pid == 0xc54f {
        info.dongle_firmware = link.firmware_at(true);
    }

    let hardware = Hardware {
        dpi: Dpi::discover(&mut link)?,
        rate: ReportRate::discover(&mut link, wireless)?,
        onboard: Onboard::discover(&mut link)?,
        analog: Analog::discover(&mut link)?,
        battery: features::battery_feature(&link),
    };
    info.battery = hardware
        .battery
        .and_then(|feature| features::battery(&mut link, feature).ok());

    let startup_path = startup_path(&link);
    let saved = startup_path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice::<HostSettings>(&data).ok())
        .filter(|prefs| prefs.version == 2);
    let caps = hardware.caps(startup_path.is_some());
    Ok(Logitech {
        info,
        caps,
        hardware,
        startup_path,
        runtime: Mutex::new(Runtime {
            link,
            saved,
            needs_restore: true,
        }),
        events,
    })
}

pub(super) struct Logitech {
    info: DeviceInfo,
    caps: Capabilities,
    hardware: Hardware,
    startup_path: Option<PathBuf>,
    runtime: Mutex<Runtime>,
    events: Option<Watcher>,
}

struct Runtime {
    link: Link,
    saved: Option<HostSettings>,
    needs_restore: bool,
}

struct Hardware {
    dpi: Option<Dpi>,
    rate: Option<ReportRate>,
    onboard: Option<Onboard>,
    analog: Option<Analog>,
    battery: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HostSettings {
    version: u8,
    dpi: Option<Sensitivity>,
    polling: Option<u16>,
    clicks: Vec<ButtonTuning>,
}

impl Hardware {
    fn caps(&self, can_save: bool) -> Capabilities {
        let mut caps = Capabilities::empty();
        caps.battery = self.battery.is_some();
        if let Some(onboard) = &self.onboard {
            caps.settings.push(
                SettingSpec::choice(
                    PROFILE,
                    section::PERFORMANCE,
                    "使用するプロファイル",
                    onboard.choices(),
                )
                .help("DPI やポーリングを変更するには「PC で設定」を選びます。"),
            );
        }
        if let Some(rate) = &self.rate {
            caps.settings.push(self.host_setting(SettingSpec::choice(
                RATE,
                section::PERFORMANCE,
                "ポーリングレート",
                rate.options.clone(),
            )));
        }
        if let Some(dpi) = &self.dpi {
            let last = dpi.x[dpi.x.len() - 1];
            let step = dpi
                .x
                .windows(2)
                .map(|v| v[1] - v[0])
                .reduce(gcd)
                .unwrap_or(1);
            caps.mouse = Some(MouseCaps {
                dpi_min: dpi.x[0].into(),
                dpi_max: last.into(),
                dpi_step: step.into(),
                min_stages: 1,
                max_stages: 1,
                xy_split: dpi.y.is_some(),
                stage_colors: false,
                stage_toggle: false,
                buttons: vec![],
                button_kinds: vec![],
                require_left_click: false,
                macros: None,
                live_buttons: vec![],
                adaptive_dpi: None,
            });
            if let Some([low, middle, high]) = dpi.lod {
                caps.settings.push(self.host_setting(SettingSpec::choice(
                    LOD,
                    section::SENSOR,
                    "リフトオフ距離",
                    vec![
                        ChoiceOpt::new(low.into(), "低い"),
                        ChoiceOpt::new(middle.into(), "標準"),
                        ChoiceOpt::new(high.into(), "高い"),
                    ],
                )));
            }
        }
        if let Some(analog) = &self.analog {
            for (button, settings) in ANALOG_SETTINGS
                .iter()
                .take(usize::from(analog.count))
                .enumerate()
            {
                let (id, label) = RT_TOGGLES[button];
                caps.settings.push(self.host_setting(SettingSpec::toggle(
                    id,
                    section::MAGNETIC,
                    label,
                )));
                for (field, &(id, label)) in settings.iter().enumerate() {
                    let low = if field == 2 { 0.0 } else { 1.0 };
                    let spec = SettingSpec::slider(
                        id,
                        section::MAGNETIC,
                        label,
                        low,
                        f32::from(analog.max[field]),
                        1.,
                        "",
                    );
                    caps.settings.push(self.host_setting(spec));
                }
            }
        }
        if can_save {
            caps.actions.push(ActionSpec::new(SAVE, section::SYSTEM, "現在の設定を起動用に保存")
                .help("PC で設定中の値を Device HUB の起動時に復元します。本体プロファイルの選択中は PC 側の復元を解除します。"));
        }
        caps
    }

    fn host_setting(&self, spec: SettingSpec) -> SettingSpec {
        if self.onboard.is_some() {
            spec.enabled_when(PROFILE, SettingValue::Choice(0), HOST_ONLY)
        } else {
            spec
        }
    }

    fn ensure_host(&self, link: &mut Link) -> HubResult<()> {
        if self.onboard.is_some() && !Onboard::host(link)? {
            return Err(HubError::Invalid(HOST_ONLY.into()));
        }
        Ok(())
    }

    fn read_host(&self, link: &mut Link) -> HubResult<HostSettings> {
        let dpi = self.dpi.as_ref().map(|dpi| dpi.read(link)).transpose()?;
        let polling = self.rate.as_ref().map(|rate| rate.read(link)).transpose()?;
        let mut clicks = Vec::new();
        if let Some(analog) = &self.analog {
            for i in 0..analog.count {
                clicks.push(analog.read(link, i)?);
            }
        }
        Ok(HostSettings {
            version: 2,
            dpi,
            polling,
            clicks,
        })
    }

    fn restore_if_needed(&self, runtime: &mut Runtime) -> HubResult<bool> {
        let Some(saved) = &runtime.saved else {
            return Ok(false);
        };
        let link = &mut runtime.link;
        let onboard = if let Some(onboard) = self.onboard.as_ref() {
            (!Onboard::host(link)?).then_some(onboard)
        } else {
            None
        };
        if !runtime.needs_restore && onboard.is_none() {
            return Ok(false);
        }

        link.confirm_unit()?;
        if let Some(onboard) = onboard {
            onboard.select(link, 0)?;
        }
        if let (Some(dpi), Some(value)) = (&self.dpi, saved.dpi) {
            if dpi.accepts(value) && dpi.read(link)? != value {
                dpi.write(link, value)?;
            }
        }
        if let (Some(rate), Some(hz)) = (&self.rate, saved.polling) {
            if rate.options.iter().any(|opt| opt.value == i64::from(hz)) && rate.read(link)? != hz {
                rate.write(link, hz)?;
            }
        }
        if let Some(analog) = &self.analog {
            for (i, &values) in saved
                .clicks
                .iter()
                .take(usize::from(analog.count))
                .enumerate()
            {
                if analog.read(link, i as u8)? != values {
                    analog.write(link, i as u8, values)?;
                }
            }
        }
        runtime.needs_restore = false;
        Ok(true)
    }
}

impl Logitech {
    fn runtime(&self) -> std::sync::MutexGuard<'_, Runtime> {
        self.runtime.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn notify_change(&self) {
        if let Some(events) = &self.events {
            events(HubEvent::StateChanged(self.info.id.clone()));
        }
    }
}

impl DeviceDriver for Logitech {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut runtime = self.runtime();
        self.hardware.restore_if_needed(&mut runtime)?;
        let link = &mut runtime.link;
        let mut settings = BTreeMap::new();
        if let Some(onboard) = &self.hardware.onboard {
            settings.insert(
                PROFILE.into(),
                SettingValue::Choice(onboard.current(link)?.into()),
            );
        }
        let hardware = self.hardware.read_host(link)?;
        if let Some(hz) = hardware.polling {
            settings.insert(RATE.into(), SettingValue::Choice(hz.into()));
        }
        if let Some(lod) = hardware.dpi.and_then(|dpi| dpi.lod) {
            settings.insert(LOD.into(), SettingValue::Choice(lod.into()));
        }
        for (index, click) in hardware.clicks.iter().enumerate() {
            settings.insert(
                RT_TOGGLES[index].0.into(),
                SettingValue::Bool(click.rapid_trigger),
            );
            for (&(id, _), &value) in ANALOG_SETTINGS[index].iter().zip(&click.levels) {
                settings.insert(id.into(), SettingValue::Num(value.into()));
            }
        }
        Ok(DeviceState {
            profile: ProfileState::default(),
            settings,
            mouse: hardware.dpi.map(|dpi| MouseState {
                stages: vec![DpiStage {
                    x: dpi.x.into(),
                    y: dpi.y.map(u32::from),
                    enabled: true,
                    color: Rgb {
                        r: 255,
                        g: 255,
                        b: 255,
                    },
                }],
                active_stage: 0,
                buttons: vec![],
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        let mut runtime = self.runtime();
        let Runtime { link, saved, .. } = &mut *runtime;
        link.confirm_unit()?;
        if let Change::Setting {
            id,
            value: SettingValue::Choice(sector),
        } = change
        {
            if id == PROFILE {
                let sector = u16::try_from(*sector)
                    .map_err(|_| HubError::Invalid("プロファイル番号".into()))?;
                let onboard = self
                    .hardware
                    .onboard
                    .as_ref()
                    .ok_or(HubError::Unsupported("本体プロファイル"))?;
                onboard.select(link, sector)?;
                if sector != 0 {
                    *saved = None;
                }
                drop(runtime);
                self.notify_change();
                return Ok(());
            }
        }

        self.hardware.ensure_host(link)?;
        let mut reread = false;
        match change {
            Change::DpiStages { stages, active: 0 } if stages.len() == 1 && stages[0].enabled => {
                let dpi = self
                    .hardware
                    .dpi
                    .as_ref()
                    .ok_or(HubError::Unsupported("DPI"))?;
                let before = dpi.read(link)?;
                let new = dpi.snap(stages[0].x, stages[0].y, before.lod)?;
                dpi.write(link, new)?;
                if let Some(saved) = saved {
                    saved.dpi = Some(new);
                }
                reread = u32::from(new.x) != stages[0].x || new.y.map(u32::from) != stages[0].y;
            }
            Change::Setting {
                id,
                value: SettingValue::Choice(hz),
            } if id == RATE => {
                let rate = self
                    .hardware
                    .rate
                    .as_ref()
                    .ok_or(HubError::Unsupported("ポーリングレート"))?;
                let hz =
                    u16::try_from(*hz).map_err(|_| HubError::Invalid("ポーリングレート".into()))?;
                rate.write(link, hz)?;
                if let Some(saved) = saved {
                    saved.polling = Some(hz);
                }
            }
            Change::Setting {
                id,
                value: SettingValue::Choice(lod),
            } if id == LOD => {
                let dpi = self
                    .hardware
                    .dpi
                    .as_ref()
                    .filter(|dpi| dpi.lod.is_some())
                    .ok_or(HubError::Unsupported("リフトオフ距離"))?;
                let mut value = dpi.read(link)?;
                value.lod = Some(
                    u8::try_from(*lod).map_err(|_| HubError::Invalid("リフトオフ距離".into()))?,
                );
                dpi.write(link, value)?;
                if let Some(saved) = saved {
                    saved.dpi = Some(value);
                }
            }
            Change::Setting {
                id,
                value: SettingValue::Bool(enabled),
            } => {
                let analog = self
                    .hardware
                    .analog
                    .as_ref()
                    .ok_or(HubError::Unsupported("Rapid Trigger"))?;
                let button = RT_TOGGLES
                    .iter()
                    .take(usize::from(analog.count))
                    .position(|(key, _)| key == id)
                    .ok_or(HubError::Unsupported("Rapid Trigger"))?;
                update_button(analog, link, saved, button, |settings| {
                    settings.rapid_trigger = *enabled;
                })?;
            }
            Change::Setting {
                id,
                value: SettingValue::Num(value),
            } => {
                let analog = self
                    .hardware
                    .analog
                    .as_ref()
                    .ok_or(HubError::Unsupported("クリック設定"))?;
                let (button, field) = ANALOG_SETTINGS
                    .iter()
                    .take(usize::from(analog.count))
                    .enumerate()
                    .find_map(|(button, settings)| {
                        settings
                            .iter()
                            .position(|(key, _)| key == id)
                            .map(|field| (button, field))
                    })
                    .ok_or(HubError::Unsupported("クリック設定"))?;
                let low = if field == 2 { 0. } else { 1. };
                if !(low..=f32::from(analog.max[field])).contains(value) || value.fract() != 0.0 {
                    return Err(HubError::Invalid("クリック設定の範囲".into()));
                }
                update_button(analog, link, saved, button, |settings| {
                    settings.levels[field] = *value as u8;
                })?;
            }
            _ => return Err(HubError::Unsupported("Logicool の設定")),
        }
        drop(runtime);
        if reread {
            self.notify_change();
        }
        Ok(())
    }

    fn battery(&self) -> HubResult<Battery> {
        let mut runtime = self.runtime();
        let restored = self.hardware.restore_if_needed(&mut runtime)?;
        let feature = self
            .hardware
            .battery
            .ok_or(HubError::Unsupported("電池残量"))?;
        let battery = features::battery(&mut runtime.link, feature)?;
        drop(runtime);
        if restored {
            self.notify_change();
        }
        Ok(battery)
    }

    fn run_action(&self, id: &str, _file: Option<&Path>) -> HubResult<ActionOutcome> {
        if id != SAVE {
            return Err(HubError::Unsupported("Logicool の操作"));
        }
        let path = self
            .startup_path
            .as_deref()
            .ok_or(HubError::Unsupported("起動時の復元"))?;
        let mut runtime = self.runtime();
        runtime.link.confirm_unit()?;
        if self.hardware.onboard.is_some() && !Onboard::host(&mut runtime.link)? {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(file_error(err)),
            }
            runtime.saved = None;
        } else {
            let prefs = self.hardware.read_host(&mut runtime.link)?;
            let bytes = serde_json::to_vec_pretty(&prefs).map_err(file_error)?;
            std::fs::create_dir_all(path.parent().expect("startup directory"))
                .map_err(file_error)?;
            let temp = path.with_extension(format!("{:04x}.tmp", self.info.pid));
            std::fs::write(&temp, bytes).map_err(file_error)?;
            std::fs::rename(&temp, path).map_err(file_error)?;
            runtime.saved = Some(prefs);
        }
        runtime.needs_restore = false;
        Ok(ActionOutcome::default())
    }
}

fn update_button(
    analog: &Analog,
    link: &mut Link,
    saved: &mut Option<HostSettings>,
    button: usize,
    edit: impl FnOnce(&mut ButtonTuning),
) -> HubResult<()> {
    let mut tuning = analog.read(link, button as u8)?;
    edit(&mut tuning);
    analog.write(link, button as u8, tuning)?;
    if let Some(slot) = saved
        .as_mut()
        .and_then(|prefs| prefs.clicks.get_mut(button))
    {
        *slot = tuning;
    }
    Ok(())
}

fn startup_path(link: &Link) -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").filter(|p| !p.is_empty())?;
    let unit = u32::from_be_bytes(link.unit);
    Some(
        PathBuf::from(base)
            .join("DeviceHub")
            .join("Logitech")
            .join(format!("{:04x}-{unit:08x}.json", link.model)),
    )
}

fn file_error(error: impl std::fmt::Display) -> HubError {
    HubError::Io(format!("起動設定の保存: {error}"))
}

fn gcd(mut a: u16, mut b: u16) -> u16 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}
