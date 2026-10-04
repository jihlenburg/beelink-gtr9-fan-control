#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

//! Controls paired fans with temperature validation and firmware restoration.

use std::env;
use std::error::Error;
use std::ffi::c_int;
use std::fmt::{
    self,
    Display, //
};
use std::fs;
use std::io;
use std::path::{
    Path,
    PathBuf, //
};
use std::process::ExitCode;
use std::sync::atomic::{
    AtomicBool,
    Ordering, //
};
use std::thread;
use std::time::{
    Duration,
    Instant, //
};

const DEFAULT_CONFIG: &str = "/etc/gtr9-fan-control.conf";
const HWMON_ROOT: &str = "/sys/class/hwmon";
const PWM_AUTOMATIC: u8 = 2;
const PWM_MANUAL: u8 = 1;
const PWM_FULL: u8 = 255;
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct AppError(String);

impl Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for AppError {}

fn err(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(AppError(message.into()))
}

/// Stores a duty value in the hardware PWM range of 0 to 255.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Duty(u8);

impl Duty {
    fn from_percent(value: u8) -> Result<Self, Box<dyn Error>> {
        if value > 100 {
            return Err(err(format!("duty must be 0..100%, got {value}")));
        }
        Ok(Self(((u16::from(value) * 255 + 50) / 100) as u8))
    }

    fn percent(self) -> u8 {
        ((u16::from(self.0) * 100 + 127) / 255) as u8
    }
}

/// Stores a temperature in whole degrees Celsius.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct TempC(i32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CurvePoint {
    temp: TempC,
    duty: Duty,
}

#[derive(Debug)]
struct Config {
    poll: Duration,
    fall_delay: Duration,
    hysteresis_c: i32,
    critical: TempC,
    fan_stall: Duration,
    curve: Vec<CurvePoint>,
    fan_stop_below: Option<TempC>,
    fan_resume_at: TempC,
    idle_delay: Duration,
}

impl Config {
    fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let text = fs::read_to_string(path)
            .map_err(|e| err(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self, Box<dyn Error>> {
        let mut poll_seconds = 2;
        let mut fall_delay_seconds = 30;
        let mut hysteresis_c = 4;
        let mut critical_temp_c = 90;
        let mut fan_stall_seconds = 10;
        let mut curve = None;
        let mut fan_stop_below = None;
        let mut fan_resume_at = 45;
        let mut idle_delay_seconds = 30;

        for (index, original) in text.lines().enumerate() {
            let line = original.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| err(format!("line {}: expected key=value", index + 1)))?;
            let key = key.trim();
            let value = value.trim();
            let number = || {
                value
                    .parse::<u64>()
                    .map_err(|e| err(format!("line {}: invalid {key}: {e}", index + 1)))
            };
            let temperature = || {
                i32::try_from(number()?)
                    .map_err(|_| err(format!("line {}: {key} exceeds i32 range", index + 1)))
            };
            match key {
                "poll_seconds" => poll_seconds = number()?,
                "fall_delay_seconds" => fall_delay_seconds = number()?,
                "hysteresis_c" => hysteresis_c = temperature()?,
                "critical_temp_c" => critical_temp_c = temperature()?,
                "fan_stall_seconds" => fan_stall_seconds = number()?,
                "curve" => curve = Some(parse_curve(value)?),
                "fan_stop_below_c" => fan_stop_below = Some(TempC(temperature()?)),
                "fan_resume_at_c" => fan_resume_at = temperature()?,
                "fan_stop_idle_seconds" => idle_delay_seconds = number()?,
                _ => return Err(err(format!("line {}: unknown setting {key}", index + 1))),
            }
        }

        let config = Self {
            poll: Duration::from_secs(poll_seconds),
            fall_delay: Duration::from_secs(fall_delay_seconds),
            hysteresis_c,
            critical: TempC(critical_temp_c),
            fan_stall: Duration::from_secs(fan_stall_seconds),
            curve: curve.ok_or_else(|| err("missing curve setting"))?,
            fan_stop_below,
            fan_resume_at: TempC(fan_resume_at),
            idle_delay: Duration::from_secs(idle_delay_seconds),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.poll < Duration::from_secs(1) || self.poll > Duration::from_secs(30) {
            return Err(err("poll_seconds must be 1..30"));
        }
        if !(1..=15).contains(&self.hysteresis_c) {
            return Err(err("hysteresis_c must be 1..15"));
        }
        if !(70..=105).contains(&self.critical.0) {
            return Err(err("critical_temp_c must be 70..105"));
        }
        if self.curve.len() < 2 {
            return Err(err("curve needs at least two points"));
        }
        if self.curve[0].temp.0 > 20 {
            return Err(err("curve must begin at or below 20C"));
        }
        if self.curve.last().unwrap().temp < self.critical {
            return Err(err("curve must cover critical_temp_c"));
        }
        if self.curve.last().unwrap().duty != Duty(PWM_FULL) {
            return Err(err("last curve duty must be 100%"));
        }
        for pair in self.curve.windows(2) {
            if pair[1].temp <= pair[0].temp {
                return Err(err("curve temperatures must increase"));
            }
            if pair[1].duty < pair[0].duty {
                return Err(err("curve duties must not decrease"));
            }
        }
        if let Some(stop) = self.fan_stop_below {
            if !(15..=45).contains(&stop.0)
                || self.fan_resume_at <= stop
                || self.fan_resume_at.0 > 60
            {
                return Err(err(
                    "fan stop must be 15..45C and resume must be above stop and at most 60C",
                ));
            }
            if self.idle_delay < Duration::from_secs(10)
                || self.idle_delay > Duration::from_secs(300)
            {
                return Err(err("fan_stop_idle_seconds must be 10..300"));
            }
            if self.curve[0].duty.0 == 0 {
                return Err(err("fan-stop mode requires a positive running duty"));
            }
        }
        Ok(())
    }

    /// Selects running duty, forcing full speed at the critical temperature.
    fn running_duty(&self, temp: TempC) -> Duty {
        if temp >= self.critical {
            Duty(PWM_FULL)
        } else {
            self.desired(temp)
        }
    }

    fn desired(&self, temp: TempC) -> Duty {
        self.curve
            .iter()
            .rev()
            .find(|p| temp >= p.temp)
            .unwrap_or(&self.curve[0])
            .duty
    }
}

fn parse_curve(value: &str) -> Result<Vec<CurvePoint>, Box<dyn Error>> {
    value
        .split(',')
        .map(|item| {
            let (temp, duty) = item
                .trim()
                .split_once(':')
                .ok_or_else(|| err(format!("invalid curve point {item:?}")))?;
            Ok(CurvePoint {
                temp: TempC(temp.trim().parse()?),
                duty: Duty::from_percent(duty.trim().parse()?)?,
            })
        })
        .collect()
}

#[derive(Debug)]
struct Hardware {
    ite: PathBuf,
    temp_inputs: Vec<PathBuf>,
}

impl Hardware {
    fn discover(root: &Path) -> Result<Self, Box<dyn Error>> {
        let mut ite = None;
        let mut temp_inputs = Vec::new();
        let mut sensor_names = std::collections::BTreeSet::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            let name = read_trimmed(path.join("name")).unwrap_or_default();
            if name == "it8613" {
                ite = Some(path.clone());
            }
            if name == "k10temp" || name == "amdgpu" {
                sensor_names.insert(name.clone());
                for sensor in fs::read_dir(&path)? {
                    let sensor_path = sensor?.path();
                    let file = sensor_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if file.starts_with("temp") && file.ends_with("_input") {
                        temp_inputs.push(sensor_path);
                    }
                }
            }
        }
        let hardware = Self {
            ite: ite.ok_or_else(|| err("it8613 hwmon device not found; is it87 loaded?"))?,
            temp_inputs,
        };
        if sensor_names.len() != 2 {
            return Err(err(
                "both k10temp and amdgpu temperature sensors are required",
            ));
        }
        for file in [
            "pwm2",
            "pwm3",
            "pwm2_enable",
            "pwm3_enable",
            "pwm2_auto_start",
            "pwm3_auto_start",
            "fan3_input",
        ] {
            if !hardware.ite.join(file).exists() {
                return Err(err(format!("missing ITE attribute {file}")));
            }
        }
        Ok(hardware)
    }

    fn temperature(&self) -> Result<TempC, Box<dyn Error>> {
        let mut values = Vec::new();
        for path in &self.temp_inputs {
            let raw = read_trimmed(path)?.parse::<i32>()?;
            if !(-20_000..=130_000).contains(&raw) {
                return Err(err(format!(
                    "invalid temperature in {}: {raw}",
                    path.display()
                )));
            }
            values.push((raw + 999).div_euclid(1000));
        }
        values
            .into_iter()
            .max()
            .map(TempC)
            .ok_or_else(|| err("all temperature readings are invalid"))
    }

    fn cooling_temperature(&self) -> Result<TempC, Box<dyn Error>> {
        let mut hottest = self.temperature()?;
        // Also protect motherboard and SSD temperatures while airflow is stopped.
        let mut dirs = vec![self.ite.clone()];
        for entry in fs::read_dir(HWMON_ROOT)? {
            let dir = entry?.path();
            if read_trimmed(dir.join("name"))? == "nvme" {
                dirs.push(dir);
            }
        }
        for dir in dirs {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with("temp") && name.ends_with("_input") {
                    let raw: i32 = read_trimmed(&path)?.parse()?;
                    if !(-20_000..=130_000).contains(&raw) {
                        return Err(err("invalid auxiliary temperature"));
                    }
                    hottest = hottest.max(TempC((raw + 999).div_euclid(1000)));
                }
            }
        }
        Ok(hottest)
    }

    fn gpu_idle(&self) -> Result<bool, Box<dyn Error>> {
        for input in &self.temp_inputs {
            let dir = input.parent().ok_or_else(|| err("invalid sensor path"))?;
            if read_trimmed(dir.join("name"))? == "amdgpu" {
                let busy: u32 = read_trimmed(dir.join("device/gpu_busy_percent"))?.parse()?;
                return Ok(busy <= 5);
            }
        }
        Err(err("GPU activity reading unavailable"))
    }

    fn fan_rpm(&self) -> Result<u32, Box<dyn Error>> {
        Ok(read_trimmed(self.ite.join("fan3_input"))?.parse()?)
    }

    fn set_manual(&self) -> Result<(), Box<dyn Error>> {
        write_value(self.ite.join("pwm2_enable"), PWM_MANUAL)?;
        if let Err(e) = write_value(self.ite.join("pwm3_enable"), PWM_MANUAL) {
            let _ = self.restore_automatic();
            return Err(e);
        }
        Ok(())
    }

    fn set_duty(&self, duty: Duty) -> Result<(), Box<dyn Error>> {
        if let Err(e) = self.write_pair(duty.0) {
            let _ = self.write_pair(PWM_FULL);
            let _ = self.restore_automatic();
            return Err(e);
        }
        Ok(())
    }

    fn write_pair(&self, value: u8) -> Result<(), Box<dyn Error>> {
        write_value(self.ite.join("pwm2"), value)?;
        write_value(self.ite.join("pwm3"), value)
    }

    fn restore_automatic(&self) -> Result<(), Box<dyn Error>> {
        // IT8613 shares manual duty and automatic start PWM registers.
        // Restore this machine's recorded original start values before auto mode.
        let start2 = write_value(self.ite.join("pwm2_auto_start"), 60);
        let start3 = write_value(self.ite.join("pwm3_auto_start"), 60);
        let first = write_value(self.ite.join("pwm2_enable"), PWM_AUTOMATIC);
        let second = write_value(self.ite.join("pwm3_enable"), PWM_AUTOMATIC);
        start2.and(start3).and(first).and(second)
    }
}

fn parse_cpu_counters(text: &str) -> Result<Vec<(u64, u64)>, Box<dyn Error>> {
    let mut result = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let name = fields.next().unwrap_or("");
        if name.starts_with("cpu") && name != "cpu" {
            let values: Vec<u64> = fields.take(8).map(str::parse).collect::<Result<_, _>>()?;
            if values.len() != 8 {
                return Err(err("incomplete CPU activity counters"));
            }
            result.push((values.iter().sum(), values[3] + values[4]));
        }
    }
    if result.is_empty() {
        return Err(err("no CPU activity counters"));
    }
    Ok(result)
}

fn busiest_core_percent(
    previous: &[(u64, u64)],
    current: &[(u64, u64)],
) -> Result<f64, Box<dyn Error>> {
    if previous.len() != current.len() {
        return Err(err("CPU topology changed"));
    }
    let mut busiest: f64 = 0.0;
    for ((old_total, old_idle), (total, idle)) in previous.iter().zip(current) {
        let elapsed = total
            .checked_sub(*old_total)
            .filter(|n| *n > 0)
            .ok_or_else(|| err("invalid CPU counter interval"))?;
        let idle_time = idle
            .checked_sub(*old_idle)
            .filter(|n| *n <= elapsed)
            .ok_or_else(|| err("invalid CPU idle interval"))?;
        busiest = busiest.max(100.0 * (elapsed - idle_time) as f64 / elapsed as f64);
    }
    Ok(busiest)
}

fn should_stop_fans(
    stopped: bool,
    idle: bool,
    cooling: TempC,
    stop: TempC,
    resume: TempC,
    idle_elapsed: Duration,
    delay: Duration,
) -> bool {
    idle && if stopped {
        cooling < resume
    } else {
        cooling <= stop && idle_elapsed >= delay
    }
}

/// Restores firmware control when the control loop returns.
struct AutomaticGuard<'a>(&'a Hardware);
impl Drop for AutomaticGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.restore_automatic();
    }
}

fn run(config: &Config, hardware: &Hardware) -> Result<(), Box<dyn Error>> {
    let initial_temp = hardware.temperature()?;
    hardware.set_manual()?;
    let _guard = AutomaticGuard(hardware);
    // Start with a modest pulse; use full speed only for hot starts or recovery.
    let initial_duty = config.running_duty(initial_temp);
    hardware.set_duty(initial_duty.max(Duty::from_percent(35)?))?;
    thread::sleep(Duration::from_secs(1));
    let latest_temp = hardware.temperature()?;
    let mut duty = config.running_duty(latest_temp);
    hardware.set_duty(duty)?;
    let mut lower_since: Option<Instant> = None;
    let mut stalled_since: Option<Instant> = None;
    let mut idle_since: Option<Instant> = None;
    let mut cpu_previous = parse_cpu_counters(&fs::read_to_string("/proc/stat")?)?;
    println!(
        "started: temperature={}C duty={}%; pwm2=pwm3",
        initial_temp.0,
        duty.percent()
    );

    while !STOP.load(Ordering::Relaxed) {
        thread::sleep(config.poll);
        let temp = match hardware.temperature() {
            Ok(value) => value,
            Err(e) => {
                hardware.set_duty(Duty(PWM_FULL))?;
                return Err(err(format!("temperature failure; commanded 100%: {e}")));
            }
        };
        let desired = config.running_duty(temp);
        let mut want_off = false;
        if let Some(stop) = config.fan_stop_below {
            let current = parse_cpu_counters(&fs::read_to_string("/proc/stat")?)?;
            let cpu_idle = busiest_core_percent(&cpu_previous, &current)? <= 10.0;
            cpu_previous = current;
            let gpu_idle = hardware.gpu_idle()?;
            let cooling = hardware.cooling_temperature()?;
            if cpu_idle && gpu_idle && cooling <= stop {
                idle_since.get_or_insert_with(Instant::now);
            } else {
                idle_since = None;
            }
            want_off = should_stop_fans(
                duty.0 == 0,
                cpu_idle && gpu_idle,
                cooling,
                stop,
                config.fan_resume_at,
                idle_since.map(|t| t.elapsed()).unwrap_or_default(),
                config.idle_delay,
            );
        }
        if want_off {
            if duty.0 != 0 {
                duty = Duty(0);
                hardware.set_duty(duty)?;
                println!("idle and cool; fans stopped");
            }
            lower_since = None;
            stalled_since = None;
            println!(
                "temperature={}C duty=0% fan3={}rpm",
                temp.0,
                hardware.fan_rpm()?
            );
            continue;
        }
        if duty.0 == 0 {
            // A modest start pulse avoids a full-speed burst when a short task arrives.
            hardware.set_duty(desired.max(Duty::from_percent(35)?))?;
            thread::sleep(Duration::from_secs(1));
            let latest = hardware.temperature()?;
            duty = config.running_duty(latest);
            hardware.set_duty(duty)?;
            idle_since = None;
            lower_since = None;
            println!("activity or warming; fans restarted at {}%", duty.percent());
        }

        if desired >= duty {
            if desired != duty {
                duty = desired;
                hardware.set_duty(duty)?;
            }
            lower_since = None;
        } else {
            let down_duty = config.desired(TempC(temp.0 + config.hysteresis_c));
            if down_duty < duty {
                let since = lower_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= config.fall_delay {
                    duty = down_duty;
                    hardware.set_duty(duty)?;
                    lower_since = None;
                }
            } else {
                lower_since = None;
            }
        }

        let rpm = hardware.fan_rpm()?;
        if duty.0 > 0 && rpm == 0 {
            let since = stalled_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= config.fan_stall {
                hardware.set_duty(Duty(PWM_FULL))?;
                thread::sleep(Duration::from_secs(3));
                if hardware.fan_rpm()? == 0 {
                    return Err(err("fan3 remained stopped after 100% recovery pulse"));
                }
                duty = Duty(PWM_FULL);
                stalled_since = None;
            }
        } else {
            stalled_since = None;
        }

        println!(
            "temperature={}C duty={}% fan3={}rpm",
            temp.0,
            duty.percent(),
            rpm
        );
    }
    println!("stopping; restoring firmware automatic control");
    Ok(())
}

fn read_trimmed(path: impl AsRef<Path>) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}
fn write_value(path: impl AsRef<Path>, value: u8) -> Result<(), Box<dyn Error>> {
    fs::write(path.as_ref(), value.to_string())
        .map_err(|e| err(format!("cannot write {}: {e}", path.as_ref().display())))
}

/// Reads a validated hwmon temperature in millidegrees Celsius.
fn read_temperature(path: &Path) -> Result<i32, Box<dyn Error>> {
    let raw = read_trimmed(path)?.parse::<i32>()?;
    if !(-20_000..=130_000).contains(&raw) {
        return Err(err(format!(
            "invalid temperature in {}: {raw}",
            path.display()
        )));
    }
    Ok(raw)
}

/// Holds one read-only sample shared by the text and JSON formatters.
struct StatusSnapshot {
    pwm2: u8,
    pwm2_enable: u8,
    pwm3: u8,
    pwm3_enable: u8,
    fan2_input: u32,
    fan3_rpm: u32,
    control_temperature: TempC,
    cpu_temperature_mc: i32,
    motherboard_temperatures_mc: Vec<(u32, i32)>,
}

impl StatusSnapshot {
    fn read(hardware: &Hardware) -> Result<Self, Box<dyn Error>> {
        let mut cpu_temperature = None;
        for input in &hardware.temp_inputs {
            let dir = input.parent().ok_or_else(|| err("invalid sensor path"))?;
            if read_trimmed(dir.join("name"))? == "k10temp" {
                let value = read_temperature(input)?;
                cpu_temperature =
                    Some(cpu_temperature.map_or(value, |previous: i32| previous.max(value)));
            }
        }
        let mut motherboard_temperatures_mc = Vec::new();
        for entry in fs::read_dir(&hardware.ite)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if let Some(sensor) = name
                .strip_prefix("temp")
                .and_then(|s| s.strip_suffix("_input"))
            {
                // Only numeric hwmon channel identifiers can become JSON keys.
                if !sensor.is_empty() && sensor.bytes().all(|b| b.is_ascii_digit()) {
                    motherboard_temperatures_mc.push((sensor.parse()?, read_temperature(&path)?));
                }
            }
        }
        motherboard_temperatures_mc.sort_by_key(|(channel, _)| *channel);
        Ok(Self {
            pwm2: read_trimmed(hardware.ite.join("pwm2"))?.parse()?,
            pwm2_enable: read_trimmed(hardware.ite.join("pwm2_enable"))?.parse()?,
            pwm3: read_trimmed(hardware.ite.join("pwm3"))?.parse()?,
            pwm3_enable: read_trimmed(hardware.ite.join("pwm3_enable"))?.parse()?,
            fan2_input: read_trimmed(hardware.ite.join("fan2_input"))?.parse()?,
            fan3_rpm: hardware.fan_rpm()?,
            control_temperature: hardware.temperature()?,
            cpu_temperature_mc: cpu_temperature
                .ok_or_else(|| err("CPU temperature unavailable"))?,
            motherboard_temperatures_mc,
        })
    }

    fn text(&self) -> String {
        let mut report = format!(
            "pwm2={}\npwm2_enable={}\npwm3={}\npwm3_enable={}\nfan2_input={}\nfan3_input={}\ncontrol_temperature_c={}\n\nFan 3: {} RPM\nFan 2: RPM unavailable (no usable tachometer)\nCPU: {:.1} °C\n",
            self.pwm2,
            self.pwm2_enable,
            self.pwm3,
            self.pwm3_enable,
            self.fan2_input,
            self.fan3_rpm,
            self.control_temperature.0,
            self.fan3_rpm,
            f64::from(self.cpu_temperature_mc) / 1000.0,
        );
        if self.motherboard_temperatures_mc.is_empty() {
            report.push_str("Motherboard: temperature unavailable\n");
        }
        for (channel, value) in &self.motherboard_temperatures_mc {
            report.push_str(&format!(
                "Motherboard (temp{channel}): {:.1} °C\n",
                f64::from(*value) / 1000.0,
            ));
        }
        report
    }

    /// Serializes numeric readings using fixed keys and validated channel numbers.
    fn json(&self) -> String {
        let motherboard = self
            .motherboard_temperatures_mc
            .iter()
            .map(|(channel, value)| format!("\"temp{channel}\":{}", f64::from(*value) / 1000.0))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"schema_version\":1,\"fans\":{{\"fan2_rpm\":null,\"fan3_rpm\":{}}},\"temperatures_c\":{{\"cpu\":{},\"control\":{},\"motherboard\":{{{}}}}},\"raw\":{{\"pwm2\":{},\"pwm2_enable\":{},\"pwm3\":{},\"pwm3_enable\":{},\"fan2_input\":{}}}}}\n",
            self.fan3_rpm,
            f64::from(self.cpu_temperature_mc) / 1000.0,
            self.control_temperature.0,
            motherboard,
            self.pwm2,
            self.pwm2_enable,
            self.pwm3,
            self.pwm3_enable,
            self.fan2_input,
        )
    }
}

fn status(hardware: &Hardware, json: bool) -> Result<(), Box<dyn Error>> {
    let snapshot = StatusSnapshot::read(hardware)?;
    print!(
        "{}",
        if json {
            snapshot.json()
        } else {
            snapshot.text()
        }
    );
    Ok(())
}

/// Accepts only the documented status output option.
fn status_json_option(args: &[String]) -> Result<bool, Box<dyn Error>> {
    match args {
        [] => Ok(false),
        [option] if option == "--json" => Ok(true),
        _ => Err(err("Usage: gtr9-fan-control status [--json]")),
    }
}

unsafe extern "C" {
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
}
extern "C" fn stop_signal(_: c_int) {
    STOP.store(true, Ordering::Relaxed);
}
fn install_signal_handlers() {
    // SAFETY: On the supported Linux target, these are SIGINT and SIGTERM.
    // `stop_signal` has the C signal-handler ABI, remains valid for the process
    // lifetime, and only performs a lock-free atomic store in signal context.
    unsafe {
        signal(2, stop_signal);
        signal(15, stop_signal);
    }
}

fn real_main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("help");
    let config_path = Path::new(args.get(2).map(String::as_str).unwrap_or(DEFAULT_CONFIG));
    match command {
        "run" => {
            let config = Config::load(config_path)?;
            let hw = Hardware::discover(Path::new(HWMON_ROOT))?;
            install_signal_handlers();
            run(&config, &hw)
        }
        "restore" => {
            Hardware::discover(Path::new(HWMON_ROOT))?.restore_automatic()?;
            println!("pwm2 and pwm3 restored to automatic mode");
            Ok(())
        }
        "status" => {
            let json = status_json_option(&args[2..])?;
            status(&Hardware::discover(Path::new(HWMON_ROOT))?, json)
        }
        "validate" => {
            Config::load(config_path)?;
            println!("{} is valid", config_path.display());
            Ok(())
        }
        _ => {
            println!(
                "Usage: gtr9-fan-control <run|restore|validate> [config]\n       gtr9-fan-control status [--json]"
            );
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gtr9-fan-control: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "poll_seconds=2\nfall_delay_seconds=30\nhysteresis_c=4\ncritical_temp_c=90\nfan_stall_seconds=10\ncurve=0:25,60:30,70:40,80:55,90:100\n";

    #[test]
    fn parses_and_selects_step_curve() {
        let c = Config::parse(GOOD).unwrap();
        assert_eq!(c.desired(TempC(59)).percent(), 25);
        assert_eq!(c.desired(TempC(60)).percent(), 30);
        assert_eq!(c.desired(TempC(89)).percent(), 55);
        assert_eq!(c.desired(TempC(90)).percent(), 100);
    }

    #[test]
    fn critical_temperature_overrides_curve_during_running_duty_selection() {
        let config = Config::parse(&GOOD.replace("90:100", "100:100")).unwrap();
        assert_eq!(config.running_duty(TempC(89)), config.desired(TempC(89)));
        assert_eq!(config.desired(TempC(90)).percent(), 55);
        assert_eq!(config.running_duty(TempC(90)), Duty(PWM_FULL));
        assert_eq!(config.running_duty(TempC(110)), Duty(PWM_FULL));
    }

    #[test]
    fn rejects_temperature_settings_that_would_wrap_into_valid_values() {
        for (setting, wrapped_value) in [
            ("hysteresis_c", 4_u64),
            ("critical_temp_c", 90),
            ("fan_stop_below_c", 40),
            ("fan_resume_at_c", 45),
        ] {
            let text = format!("{GOOD}{setting}={}\n", (1_u64 << 32) + wrapped_value);
            assert!(
                Config::parse(&text).is_err(),
                "accepted overflowing {setting}"
            );
        }
    }

    #[test]
    fn rejects_decreasing_duty() {
        assert!(Config::parse(&GOOD.replace("70:40", "70:20")).is_err());
    }

    #[test]
    fn rejects_curve_without_full_speed_at_critical() {
        assert!(Config::parse(&GOOD.replace("90:100", "90:90")).is_err());
    }

    #[test]
    fn temperature_failure_is_not_hidden_by_another_sensor() {
        let dir = env::temp_dir().join(format!("gtr9-temp-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cpu = dir.join("cpu");
        let gpu = dir.join("gpu");
        fs::write(&cpu, "30001").unwrap();
        fs::write(&gpu, "30000").unwrap();
        let hw = Hardware {
            ite: dir.clone(),
            temp_inputs: vec![cpu, gpu.clone()],
        };
        assert_eq!(hw.temperature().unwrap(), TempC(31));
        fs::write(&gpu, "garbage").unwrap();
        assert!(hw.temperature().is_err());
        fs::write(&gpu, "131000").unwrap();
        assert!(hw.temperature().is_err());
        fs::remove_file(&gpu).unwrap();
        assert!(hw.temperature().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_reports_cpu_separately_from_gpu_and_does_not_write_hardware() {
        let dir = env::temp_dir().join(format!("gtr9-status-test-{}", std::process::id()));
        let cpu = dir.join("cpu");
        let gpu = dir.join("gpu");
        let ite = dir.join("ite");
        for path in [&cpu, &gpu, &ite] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(cpu.join("name"), "k10temp").unwrap();
        fs::write(cpu.join("temp1_input"), "38500").unwrap();
        fs::write(gpu.join("name"), "amdgpu").unwrap();
        fs::write(gpu.join("temp1_input"), "55000").unwrap();
        let attributes = [
            ("pwm2", "31"),
            ("pwm3", "31"),
            ("pwm2_enable", "1"),
            ("pwm3_enable", "1"),
            ("fan2_input", "0"),
            ("fan3_input", "698"),
            ("temp1_input", "38000"),
            ("temp2_input", "46000"),
        ];
        for (name, value) in attributes {
            fs::write(ite.join(name), value).unwrap();
        }
        let hardware = Hardware {
            ite: ite.clone(),
            temp_inputs: vec![gpu.join("temp1_input"), cpu.join("temp1_input")],
        };
        let snapshot = StatusSnapshot::read(&hardware).unwrap();
        let report = snapshot.text();
        let json = snapshot.json();
        assert!(json.contains("\"fan2_rpm\":null"));
        assert!(json.contains("\"fan3_rpm\":698"));
        assert!(json.contains("\"cpu\":38.5"));
        assert!(json.contains("\"control\":55"));
        assert!(json.contains("\"motherboard\":{\"temp1\":38,\"temp2\":46}"));
        assert!(report.contains("Fan 3: 698 RPM"));
        assert!(report.contains("Fan 2: RPM unavailable"));
        assert!(report.contains("CPU: 38.5 °C"));
        assert!(report.contains("control_temperature_c=55"));
        assert!(report.contains("Motherboard (temp2): 46.0 °C"));
        for (name, value) in attributes {
            assert_eq!(read_trimmed(ite.join(name)).unwrap(), value);
        }
        fs::write(ite.join("temp2_input"), "131000").unwrap();
        assert!(StatusSnapshot::read(&hardware).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_options_reject_unknown_or_duplicate_flags() {
        assert!(!status_json_option(&[]).unwrap());
        assert!(status_json_option(&["--json".into()]).unwrap());
        assert!(status_json_option(&["--jsno".into()]).is_err());
        assert!(status_json_option(&["--json".into(), "--json".into()]).is_err());
    }

    #[test]
    fn restores_original_start_duty_before_automatic_mode() {
        let dir = env::temp_dir().join(format!("gtr9-restore-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let hw = Hardware {
            ite: dir.clone(),
            temp_inputs: vec![],
        };
        hw.restore_automatic().unwrap();
        for c in [2, 3] {
            assert_eq!(
                read_trimmed(dir.join(format!("pwm{c}_auto_start"))).unwrap(),
                "60"
            );
            assert_eq!(
                read_trimmed(dir.join(format!("pwm{c}_enable"))).unwrap(),
                "2"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_busy_core_wakes_fans_even_on_a_32_thread_cpu() {
        let previous = vec![(100, 100); 32];
        let mut current = vec![(200, 200); 32];
        current[7] = (200, 100);
        assert_eq!(busiest_core_percent(&previous, &current).unwrap(), 100.0);
        assert!(busiest_core_percent(&previous, &previous).is_err());
    }

    #[test]
    fn ignores_duplicate_guest_cpu_time() {
        let text = "cpu 0 0 0 0 0 0 0 0 0 0\ncpu0 10 1 2 80 5 1 1 0 9 1\n";
        assert_eq!(parse_cpu_counters(text).unwrap(), vec![(100, 85)]);
    }

    #[test]
    fn fan_stop_needs_idle_cooldown_and_restarts_on_activity_or_heat() {
        let check = |stopped, cpu_idle, gpu_idle, temp, seconds| {
            should_stop_fans(
                stopped,
                cpu_idle && gpu_idle,
                TempC(temp),
                TempC(40),
                TempC(45),
                Duration::from_secs(seconds),
                Duration::from_secs(30),
            )
        };
        assert!(!check(false, true, true, 35, 29));
        assert!(check(false, true, true, 40, 30));
        assert!(!check(false, true, true, 41, 30));
        assert!(check(true, true, true, 44, 0));
        assert!(!check(true, true, true, 45, 0));
        assert!(!check(true, false, true, 35, 30));
        assert!(!check(true, true, false, 35, 30));
    }

    #[test]
    fn rejects_fan_stop_without_temperature_hysteresis() {
        let text = format!("{GOOD}fan_stop_below_c=40\nfan_resume_at_c=40\n");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn duty_conversion_is_bounded() {
        assert_eq!(Duty::from_percent(0).unwrap().0, 0);
        assert_eq!(Duty::from_percent(100).unwrap().0, 255);
        assert!(Duty::from_percent(101).is_err());
    }
}
