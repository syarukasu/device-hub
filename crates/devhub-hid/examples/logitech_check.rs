//! Reads settings through the normal backend, including any configured startup restore.
//! --round-trip changes one setting at a time and restores its old value.
//! --save-startup adopts the currently active settings for the next Device HUB launch.
use devhub_core::backend::DeviceDriver;
use devhub_core::model::*;
use std::io;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options: Vec<_> = std::env::args().skip(1).collect();
    let hub = devhub_hid::connect()?;
    let info = hub
        .scan()?
        .into_iter()
        .find(|d| d.driver == DriverKind::Logitech)
        .ok_or_else(|| io::Error::other("No supported Logitech mouse was found"))?;
    let driver = hub.driver(&info.id)?;
    let info = driver.info();
    let baseline = driver.read()?;
    println!(
        "{} ({:04x}:{:04x}, {:?})",
        info.name, info.vid, info.pid, info.transport
    );
    println!("Battery: {:?}", driver.battery()?);
    println!("{}", serde_json::to_string_pretty(&baseline)?);

    if options.iter().any(|s| s == "--round-trip") {
        round_trip(driver.as_ref(), &baseline)?;
    }
    if options.iter().any(|s| s == "--save-startup") {
        driver.run_action("logitech_save_startup", None)?;
        println!("Startup settings saved");
    }
    Ok(())
}

fn round_trip(
    driver: &dyn DeviceDriver,
    baseline: &DeviceState,
) -> Result<(), Box<dyn std::error::Error>> {
    let caps = driver.caps();
    let mode = "logitech_profile";
    if baseline
        .settings
        .get(mode)
        .is_some_and(|v| v.as_choice() != 0)
    {
        return Err(
            io::Error::other("Select PC control in Device HUB before testing writes").into(),
        );
    }

    let dpi_test = baseline.mouse.as_ref().map(|m| {
        let stage = m.stages[0];
        let next = if stage.x == 800 { 1600 } else { 800 };
        (
            stage,
            DpiStage {
                x: next,
                y: stage.y.map(|_| next),
                ..stage
            },
        )
    });
    if let Some((before, changed)) = dpi_test {
        let result = driver.apply(&Change::DpiStages {
            stages: vec![changed],
            active: 0,
        });
        driver.apply(&Change::DpiStages {
            stages: vec![before],
            active: 0,
        })?;
        result?;
        println!("DPI changed, read back, and restored");
    }

    for spec in &caps.settings {
        if spec.id == mode {
            continue;
        }
        let Some(value) = baseline.settings.get(&spec.id) else {
            continue;
        };
        let alternative = match (&spec.kind, value) {
            (SettingKind::Choice { options }, SettingValue::Choice(old)) => options
                .iter()
                .find(|opt| opt.value != *old)
                .map(|opt| SettingValue::Choice(opt.value)),
            (SettingKind::Slider { min, max, step, .. }, SettingValue::Num(old)) => {
                let new = if old + step <= *max {
                    old + step
                } else {
                    old - step
                };
                (new >= *min && new != *old).then_some(SettingValue::Num(new))
            }
            (SettingKind::Toggle, SettingValue::Bool(old)) => Some(SettingValue::Bool(!old)),
            _ => None,
        };
        if let Some(alternative) = alternative {
            let change = |value| Change::Setting {
                id: spec.id.clone(),
                value,
            };
            let result = driver.apply(&change(alternative));
            driver.apply(&change(value.clone()))?;
            result?;
            println!("{} changed, read back, and restored", spec.id);
        }
    }

    if driver.read()? != *baseline {
        return Err(io::Error::other("The final settings do not match the original state").into());
    }
    println!("Final state matches the baseline");
    Ok(())
}
