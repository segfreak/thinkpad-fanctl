use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FAN_PATH: &str = "/proc/acpi/ibm/fan";
const FANCONTROL_PATH: &str = "/sys/module/thinkpad_acpi/parameters/fan_control";
const CONFIG_PATH: &str = "/etc/thinkpad-fanctl/curve";

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const FORCE_REFRESH: Duration = Duration::from_secs(5);

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_signal(_: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

unsafe extern "C" {
    unsafe fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

struct CurvePoint {
    up: i32,
    down: i32,
    level: String,
}

struct Curve {
    points: Vec<CurvePoint>,
}

impl Default for Curve {
    fn default() -> Self {
        let presets = [
            (0, 0, "0"),
            (58, 50, "1"),
            (68, 60, "2"),
            (75, 68, "3"),
            (82, 75, "5"),
            (88, 82, "7"),
            (93, 86, "full-speed"),
        ];

        Self {
            points: presets
                .into_iter()
                .map(|(up, down, level)| CurvePoint {
                    up,
                    down,
                    level: level.to_string(),
                })
                .collect(),
        }
    }
}

impl Curve {
    fn from_config<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        let mut points = Vec::new();

        for (line_num, raw_line) in content.lines().enumerate() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 3 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: expected 3 columns (<up> <down> <level>)", line_num + 1),
                ));
            }

            let up: i32 = parts[0].parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: invalid up temperature '{}'", line_num + 1, parts[0]),
                )
            })?;

            let down: i32 = parts[1].parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: invalid down temperature '{}'", line_num + 1, parts[1]),
                )
            })?;

            let level = parts[2];
            let valid_level = matches!(
                level,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "auto" | "disengaged" | "full-speed"
            );
            if !valid_level {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: invalid fan level '{}'", line_num + 1, level),
                ));
            }

            points.push(CurvePoint {
                up,
                down,
                level: level.to_string(),
            });
        }

        if points.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "config file contains no curve points",
            ));
        }

        points.sort_by_key(|p| p.up);
        Ok(Self { points })
    }

    fn save_to_config<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut content = String::from(
            "# Format: <up_temp_C> <down_temp_C> <level>\n\
             # Valid levels: 0..7, auto, disengaged, full-speed\n\n",
        );

        for p in &self.points {
            content.push_str(&format!("{:<3} {:<3} {}\n", p.up, p.down, p.level));
        }

        fs::write(path, content)
    }

    fn select_level(&self, temp: i32, current_idx: usize) -> usize {
        log::debug!("temp {temp}°C");

        let mut target = current_idx.min(self.points.len() - 1);
        for (i, point) in self.points.iter().enumerate() {
            if temp >= point.up {
                target = i;
            }
        }
        if target > current_idx {
            return target;
        }

        while target > 0 && temp < self.points[target].down {
            target -= 1;
        }
        target
    }

    #[inline]
    fn level_at(&self, idx: usize) -> &str {
        &self.points[idx].level
    }
}

struct SafetyGuard;

impl Drop for SafetyGuard {
    fn drop(&mut self) {
        let _ = set_fan_level("auto");
    }
}

fn find_cpu_sensor() -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(name) = fs::read_to_string(path.join("name")) {
                if matches!(name.trim(), "coretemp" | "k10temp") {
                    let temp_file = path.join("temp1_input");
                    if temp_file.exists() {
                        return Some(temp_file);
                    }
                }
            }
        }
    }
    let fallback = PathBuf::from("/sys/class/thermal/thermal_zone0/temp");
    fallback.exists().then_some(fallback)
}

#[inline]
fn read_temp_celsius(sensor: &PathBuf) -> Result<i32, io::Error> {
    let raw = fs::read_to_string(sensor)?;
    let milli_c: i32 = raw.trim().parse().unwrap_or(85_000);
    Ok(milli_c / 1000)
}

#[inline]
fn set_fan_level(level: &str) -> Result<(), io::Error> {
    log::debug!("level {level}");

    let mut buf = [0u8; 32];
    let prefix = b"level ";
    let len = prefix.len() + level.len() + 1;

    buf[..prefix.len()].copy_from_slice(prefix);
    buf[prefix.len()..prefix.len() + level.len()].copy_from_slice(level.as_bytes());
    buf[len - 1] = b'\n';

    let mut file = OpenOptions::new().write(true).open(FAN_PATH)?;
    file.write_all(&buf[..len])
}

fn main() {
    let _ = pretty_env_logger::try_init();

    let fanctrl = fs::read_to_string(FANCONTROL_PATH).expect("fancontrol read error");
    assert!(fanctrl.starts_with('Y'));

    let curve = match Curve::from_config(CONFIG_PATH) {
        Ok(c) => {
            log::info!("loaded {} curve points from {}", c.points.len(), CONFIG_PATH);
            c
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let default_curve = Curve::default();
            match default_curve.save_to_config(CONFIG_PATH) {
                Ok(_) => log::info!("generated default config at {}", CONFIG_PATH),
                Err(save_err) => log::warn!("could not create {}: {}", CONFIG_PATH, save_err),
            }
            default_curve
        }
        Err(e) => {
            eprintln!("config error in {}: {}", CONFIG_PATH, e);
            std::process::exit(1);
        }
    };

    unsafe {
        signal(2, handle_signal);
        signal(15, handle_signal);
    }

    let sensor = find_cpu_sensor().expect("no cpu temp sensor found");
    let _guard = SafetyGuard;

    let mut current_idx = 0;
    let mut last_write = Instant::now() - FORCE_REFRESH;

    while RUNNING.load(Ordering::Relaxed) {
        let temp = match read_temp_celsius(&sensor) {
            Ok(t) => t,
            Err(_) => {
                let _ = set_fan_level("full-speed");
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };

        let next_idx = curve.select_level(temp, current_idx);
        let level_str = curve.level_at(next_idx);

        if next_idx != current_idx || last_write.elapsed() >= FORCE_REFRESH {
            if let Err(e) = set_fan_level(level_str) {
                eprintln!("error: {}: {}", FAN_PATH, e);
            } else {
                current_idx = next_idx;
                last_write = Instant::now();
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}
