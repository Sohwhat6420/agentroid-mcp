//! Fakes: a manual clock, a scripted tree source and a whole fake phone. They let every test (and
//! the selftest scaffold) run with no adb, no device and no real time passing.

use crate::frame::Shot;
use crate::settle::TreeSource;
use crate::transport::{
    Backend, DeviceInfo, HealthHint, Input, Screenshot, Transport, TransportError, POCO_SERIAL,
};
use agentroid_adb_link::guard::S24_SERIAL;
use agentroid_adb_link::runner::Clock;
use std::cell::Cell;

/// A clock that moves only when somebody sleeps or advances it.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: Cell<u64>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn advance(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
    fn sleep_ms(&self, ms: u64) {
        self.advance(ms);
    }
}

/// A one-label screen; different labels give different signatures.
pub fn screen(label: &str) -> String {
    format!("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\"><node class=\"android.widget.TextView\" text=\"{label}\" resource-id=\"\" clickable=\"false\" bounds=\"[0,0][100,100]\"/></hierarchy>")
}

/// Hands out a fixed list of dumps (or errors), one per call; each call costs `cost_ms` of fake time.
pub struct ScriptedSource<'a> {
    clock: &'a FakeClock,
    frames: Vec<Result<String, String>>,
    pos: usize,
    cycle: bool,
    cost_ms: u64,
}

impl<'a> ScriptedSource<'a> {
    /// Plays the frames once, then repeats the last one.
    pub fn new(clock: &'a FakeClock, frames: Vec<Result<String, String>>, cost_ms: u64) -> Self {
        ScriptedSource {
            clock,
            frames,
            pos: 0,
            cycle: false,
            cost_ms,
        }
    }
    /// Plays the frames over and over (a looping fixture).
    pub fn cycling(
        clock: &'a FakeClock,
        frames: Vec<Result<String, String>>,
        cost_ms: u64,
    ) -> Self {
        ScriptedSource {
            clock,
            frames,
            pos: 0,
            cycle: true,
            cost_ms,
        }
    }
}

impl TreeSource for ScriptedSource<'_> {
    fn dump(&mut self) -> Result<String, String> {
        self.clock.advance(self.cost_ms);
        let i = if self.cycle {
            self.pos % self.frames.len()
        } else {
            self.pos.min(self.frames.len() - 1)
        };
        self.pos += 1;
        self.frames[i].clone()
    }
}

pub const FAKE_POCO_ADB: &str = "192.0.2.50:37123";
pub const FAKE_S24_ADB: &str = "192.0.2.60:5555";
/// The fake POCO's second adb name, as when it is on USB and Wi-Fi at once ([`FakeDevice::poco_usb`]).
pub const FAKE_POCO_USB: &str = "usb-fake-poco-1";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A fake POCO (and a fake S24 that must never be touched). Records every call so tests can prove
/// a refused call reached nothing.
pub struct FakeDevice {
    pub screen: (i32, i32),
    pub asleep: bool,
    pub helper_installed: bool,
    /// Make `input` fail like a Xiaomi phone without the Security-settings switch.
    pub refuse_input: bool,
    pub typed: String,
    pub focused: bool,
    pub token_lost: bool,
    pub token_pushes: u32,
    pub s24_connected: bool,
    /// List the POCO a second time, under [`FAKE_POCO_USB`] (one phone, two adb names).
    pub poco_usb: bool,
    /// Serve this tree from the uiautomator backend instead of the built-in Settings screen.
    pub xml_override: Option<String>,
    /// Every transport method called, by name.
    pub calls: Vec<&'static str>,
    /// Every acting call that reached the phone (input and shell lines).
    pub acted: Vec<String>,
    /// What the fake's replies "carried" about battery, power and so on (see [`Transport::health_hint`]).
    pub hint: HealthHint,
    /// How many times `health_hint` was asked (it sends nothing: `calls` does not grow).
    pub hint_reads: u32,
}

impl Default for FakeDevice {
    fn default() -> Self {
        FakeDevice {
            screen: (1220, 2712),
            asleep: false,
            helper_installed: true,
            refuse_input: false,
            typed: String::new(),
            focused: false,
            token_lost: false,
            token_pushes: 0,
            s24_connected: true,
            poco_usb: false,
            xml_override: None,
            calls: vec![],
            acted: vec![],
            hint: HealthHint::default(),
            hint_reads: 0,
        }
    }
}

impl FakeDevice {
    pub fn new() -> Self {
        Self::default()
    }

    /// Anything other than `devices` reached the transport?
    pub fn touched(&self) -> bool {
        self.calls.iter().any(|c| *c != "devices")
    }

    fn settings_xml(&self, with_status_bar: bool) -> String {
        let bar = if with_status_bar {
            "<node class=\"android.widget.TextView\" text=\"12:00\" resource-id=\"com.android.systemui:id/clock\" clickable=\"false\" bounds=\"[0,0][200,80]\"/>"
        } else {
            ""
        };
        format!(
            "UI hierchary dumped to: /dev/tty<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">\
<node class=\"android.widget.FrameLayout\" text=\"\" content-desc=\"\" resource-id=\"\" clickable=\"false\" bounds=\"[0,0][1220,2712]\">{bar}\
<node class=\"android.widget.TextView\" text=\"Settings\" content-desc=\"\" resource-id=\"android:id/title\" clickable=\"false\" bounds=\"[48,200][400,280]\"/>\
<node class=\"android.widget.EditText\" text=\"{}\" content-desc=\"Search\" resource-id=\"search\" clickable=\"true\" focused=\"{}\" bounds=\"[48,600][1172,700]\"/>\
<node class=\"android.widget.LinearLayout\" text=\"\" content-desc=\"Wi-Fi\" resource-id=\"com.android.settings:id/wifi\" clickable=\"true\" bounds=\"[0,800][1220,960]\"/>\
</node></hierarchy>",
            xml_escape(&self.typed),
            self.focused
        )
    }
}

impl Transport for FakeDevice {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.calls.push("devices");
        let mut v = vec![DeviceInfo {
            serial: FAKE_POCO_ADB.into(),
            model: "23113RKC6G".into(),
            hw_serial: POCO_SERIAL.into(),
        }];
        if self.poco_usb {
            v.push(DeviceInfo {
                serial: FAKE_POCO_USB.into(),
                model: "23113RKC6G".into(),
                hw_serial: POCO_SERIAL.into(),
            });
        }
        if self.s24_connected {
            v.push(DeviceInfo {
                serial: FAKE_S24_ADB.into(),
                model: "SM-S921B".into(),
                hw_serial: S24_SERIAL.into(),
            });
        }
        Ok(v)
    }

    fn screen_size(&mut self, _serial: &str) -> Result<(i32, i32), TransportError> {
        self.calls.push("screen_size");
        Ok(self.screen)
    }

    fn screen_state(&mut self, _serial: &str) -> Result<String, TransportError> {
        self.calls.push("screen_state");
        Ok(if self.asleep {
            "Dozing".into()
        } else {
            "Awake".into()
        })
    }

    fn screenshot(&mut self, _serial: &str, max_width: i32) -> Result<Screenshot, TransportError> {
        self.calls.push("screenshot");
        let (w, h) = self.screen;
        let iw = max_width.min(w).max(1);
        let ih = (f64::from(h) * f64::from(iw) / f64::from(w)).round() as i32;
        Ok(Screenshot {
            jpeg: vec![0xff; 24_000],
            shot: Shot {
                dev_w: w,
                dev_h: h,
                img_w: iw,
                img_h: ih,
            },
        })
    }

    fn tree_xml(&mut self, _serial: &str, backend: Backend) -> Result<String, TransportError> {
        self.calls.push("tree_xml");
        if self.asleep {
            return Err(TransportError(
                "null root node returned by UiTestAutomationBridge".into(),
            ));
        }
        match backend {
            Backend::Uiautomator => Ok(self
                .xml_override
                .clone()
                .unwrap_or_else(|| self.settings_xml(false))),
            Backend::Helper => {
                if !self.helper_installed {
                    return Err(TransportError("helper is not installed".into()));
                }
                if self.token_lost {
                    self.token_lost = false; // the 401 path: push a fresh token and retry
                    self.token_pushes += 1;
                }
                Ok(self.settings_xml(true))
            }
        }
    }

    fn input(&mut self, _serial: &str, input: &Input) -> Result<(), TransportError> {
        self.calls.push("input");
        self.acted.push(format!("{input:?}"));
        if self.refuse_input {
            return Err(TransportError(
                "java.lang.SecurityException: Injecting input events requires INJECT_EVENTS".into(),
            ));
        }
        match input {
            Input::Tap { x, y, .. } => {
                self.focused = (48..1172).contains(x) && (600..700).contains(y)
            }
            Input::Text(t) => {
                if self.focused {
                    self.typed.push_str(&t.replace("%s", " ").replace('\\', ""));
                }
            }
            Input::UnicodeText(t) => {
                if !self.helper_installed {
                    return Err(TransportError("no helper".into()));
                }
                if self.focused {
                    self.typed.push_str(t);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn packages(&mut self, _serial: &str) -> Result<Vec<String>, TransportError> {
        self.calls.push("packages");
        Ok([
            "com.android.vending",
            "com.android.settings",
            "com.google.android.apps.maps",
            "com.google.android.apps.photos",
            "com.google.android.apps.docs",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect())
    }

    fn shell(&mut self, _serial: &str, line: &str) -> Result<String, TransportError> {
        self.calls.push("shell");
        self.acted.push(line.to_string());
        if line.starts_with("echo before:") {
            return Ok(format!("before:mWakefulness={}\nafter:mWakefulness=Awake isKeyguardShowing=false\ndisabled:false\n", if self.asleep { "Dozing" } else { "Awake" }));
        }
        if let Some(rest) = line.strip_prefix("am start -W ") {
            let comp = rest.split('\'').nth(3).unwrap_or("?");
            if rest.contains("no.such/.Activity") {
                return Ok(
                    "Error: Activity class {no.such/no.such.Activity} does not exist.\n".into(),
                );
            }
            self.focused = false;
            return Ok(format!(
                "Starting: Intent {{ {rest} }}\nStatus: ok\nActivity: {comp}\n"
            ));
        }
        if let Some(key) = line.strip_prefix("getprop ") {
            return Ok(format!("value-of-{key}\n"));
        }
        if line.starts_with("monkey ") {
            return Ok("Events injected: 1\n".into());
        }
        Ok(String::new())
    }

    fn helper_status(&mut self, _serial: &str) -> Result<(), String> {
        self.calls.push("helper_status");
        if self.helper_installed {
            Ok(())
        } else {
            Err("com.artemis.helper is not installed".into())
        }
    }

    fn helper_forget_token(&mut self, _serial: &str) -> Result<(), TransportError> {
        self.calls.push("helper_forget_token");
        self.token_lost = true;
        Ok(())
    }

    fn health_hint(&mut self, _serial: &str) -> HealthHint {
        self.hint_reads += 1; // deliberately not in `calls`: it reaches no phone
        self.hint.clone()
    }
}
