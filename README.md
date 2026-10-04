# Beelink GTR9 Pro fan control

A small fail-safe Rust daemon for quiet fan control on the Beelink GTR9 Pro with an ITE IT8613E sensor chip and the out-of-tree `it87` Linux driver.

The daemon follows a temperature-based step curve, delays speed reductions, and can stop fans while the machine is cool and idle. It controls both PWM channels together and restores firmware control when the control loop exits.

## Safety model

- `pwm2` and `pwm3` are always commanded to the same value.
- CPU and GPU temperature inputs are validated; the hottest reading wins.
- Critical CPU/GPU temperatures select 100% duty when sampled; the polling interval determines detection latency.
- A CPU/GPU temperature read failure in the main polling loop commands 100% before exiting. Startup, restart-pulse, and auxiliary sensor failures return an error and trigger firmware restoration after manual control has begun.
- A zero fan3 RPM reading while duty is positive triggers a three-second 100% recovery pulse after the stall timeout. If the fan remains stopped, the daemon exits and restores firmware control. Intentional fan-stop is exempt.
- Normal exit and signals restore firmware automatic mode and this machine's original start PWM of 60 on both channels.
- Normal startup and waking from fan-stop use a one-second pulse at at least 35%, then follow the curve.
- systemd independently runs `restore` after every service exit.
- `restore` discovers only the ITE controller and attempts all restoration writes, even if CPU/GPU sensors, the tachometer, or manual-duty attributes are unavailable.
- Configuration rejects zero or decreasing curve duties and non-increasing temperatures. Stopping fans requires the explicit fan-stop settings. The final point must cover the critical temperature and use 100% duty; critical temperatures independently override the curve.

This is experimental software for undocumented consumer hardware, not a certified functional-safety system. The unavailable tach signal for the `pwm2` fan cannot be monitored.

## Requirements

- Linux with systemd and readable CPU/GPU hwmon sensors (`k10temp` and `amdgpu`)
- Rust 1.85 or newer to build
- [`frankcrawford/it87`](https://github.com/frankcrawford/it87) loaded with IT8613E support
- Beelink GTR9 Pro hardware matching the tested PWM mapping

## Build and install

```sh
cargo test
cargo build --release
cargo run -- validate gtr9-fan-control.conf
sudo ./install.sh
gtr9-fan-control validate
gtr9-fan-control status
```

Run these commands from the repository root. The installer places the binary in `/usr/local/sbin/`, installs the systemd unit, and creates `/etc/gtr9-fan-control.conf` only if it does not already exist. It preserves existing configuration and does not start the service.

After reviewing the configuration and calibrating a safe minimum duty on your machine:

```sh
sudo systemctl enable --now gtr9-fan-control
journalctl -u gtr9-fan-control -f
```

## Operation and recovery

`status` and `validate` are read-only and run without `sudo` when the sensor files and configuration are readable. `run` and `restore` write hardware settings and require root. If `/usr/local/sbin` is absent from your shell's `PATH`, use `/usr/local/sbin/gtr9-fan-control`.

Show fan RPM, CPU temperature, and each motherboard temperature channel without root:

```sh
gtr9-fan-control status
watch -n 2 gtr9-fan-control status
```

The report includes the hottest CPU sensor reading and all ITE temperature channels, identified as `temp1`, `temp2`, etc., because their physical locations are undocumented. Fan3 provides usable RPM; fan2 is marked unavailable. Raw PWM and tachometer fields remain available in the output.

For scripts and monitoring, request JSON:

```sh
gtr9-fan-control status --json
gtr9-fan-control status --json | python3 -m json.tool
```

The output is one JSON object with `schema_version: 1`, `fans` (`fan2_rpm`, `fan3_rpm`), `temperatures_c` (`cpu`, `control`, and a `motherboard` channel map), and `raw` PWM values/modes plus `fan2_input`. RPM values are integers; temperatures are numeric degrees Celsius. Unavailable fan2 RPM is `null`; an absent motherboard sensor set is `{}`. `control` is the rounded-up hottest CPU/GPU reading used by the daemon. Errors go to stderr with a nonzero exit status and no partial JSON on stdout.

Example JSON output (readings vary):

```json
{
  "schema_version": 1,
  "fans": { "fan2_rpm": null, "fan3_rpm": 870 },
  "temperatures_c": {
    "cpu": 38.5,
    "control": 39,
    "motherboard": { "temp1": 38, "temp2": 46, "temp3": 46 }
  },
  "raw": {
    "pwm2": 38,
    "pwm2_enable": 1,
    "pwm3": 38,
    "pwm3_enable": 1,
    "fan2_input": 0
  }
}
```

Inspect service status and recent logs:

```sh
systemctl status gtr9-fan-control
journalctl -u gtr9-fan-control -b -n 100
gtr9-fan-control status
```

To stop the daemon and restore firmware control, stop the service first so it cannot continue writing PWM values:

```sh
sudo systemctl stop gtr9-fan-control
sudo gtr9-fan-control restore
```

The service does not automatically restart after failures. Inspect the logs and resolve the cause before starting it again.

The journal records duty changes, fan-stop/restart transitions, and stall recovery immediately. While duty remains steady, it reports temperature and RPM once per minute. Sensor polling, temperature protection, and stall detection still run at `poll_seconds`; each iteration shares one CPU/GPU temperature sample between curve selection and the fan-stop checks.

## Configuration

The bundled configuration uses a 15% running-duty floor:

```ini
poll_seconds=2
fall_delay_seconds=30
hysteresis_c=4
critical_temp_c=85
fan_stall_seconds=10
curve=0:15,50:25,60:35,70:55,78:75,85:100
fan_stop_below_c=45
fan_resume_at_c=50
fan_stop_idle_seconds=30
```

On this machine, 8%, 10%, and 12% all produced approximately 690 RPM on fan3; 5% stopped it. The running floor is now set to 15% for additional margin, measuring approximately 870 RPM on fan3 at 39C CPU temperature in a short check on 2026-10-04. Only fan3 provides a usable tachometer signal. The 60 PWM automatic start value is specific to the original settings captured on this machine.

Fan-stop requires 30 seconds with every logical CPU at most 10% busy, GPU utilization at most 5%, and CPU/GPU, motherboard and SSD temperatures at or below 45C. Activity or any of these temperatures reaching 50C restarts the fans. Restart uses a one-second pulse at at least 35% duty. Omit `fan_stop_below_c` to disable fan-stop.

Each curve point is `temperature_C:duty_percent`, with duty in `1..100`. Use the fan-stop settings for intentional zero duty. The curve selects the last point whose temperature is at or below the current reading; values are not interpolated. Duty increases on the next polling iteration and decreases only after the configured delay and hysteresis margin.

Edit `/etc/gtr9-fan-control.conf`, validate it, and restart to apply changes:

```sh
gtr9-fan-control validate /etc/gtr9-fan-control.conf
sudo systemctl restart gtr9-fan-control
```

CPU hotplug is supported: changes in logical CPU identities or reset activity counters establish a new baseline and reset the fan-stop idle timer. Fans restart if they were stopped, and stopping requires a fresh idle cooldown. The controller does not offline CPUs itself.

Configuration is loaded at startup. Validation checks syntax and configuration constraints; it does not verify hardware compatibility or calibrate fan duty.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| `it8613 hwmon device not found` | Confirm the `it87` driver is loaded and exposes an `it8613` device. |
| Missing CPU/GPU sensors | Both `k10temp` and `amdgpu` must be available. |
| `k10temp` or `amdgpu` has no temperature inputs | The driver is present but exposes no temperature channels; resolve this before starting control. `restore` remains available. |
| Missing ITE attribute | Verify the hardware and driver expose the expected PWM channels and fan3 tachometer. |
| Temperature or activity read failure | Inspect the logged error and sensor availability; auxiliary sensors and GPU activity are used by fan-stop mode. |
| Fan3 remains stopped after recovery | Restore firmware control and check the fan and calibrated running duty before restarting. |

## Uninstall

From the repository root:

```sh
sudo ./uninstall.sh
```

This stops and disables the service, attempts firmware restoration, and removes the installed binary and service unit. It retains `/etc/gtr9-fan-control.conf`.

## Power-saving experiments

Short idle tests on the original machine on 2026-10-04 measured approximately 4.4–4.6 W package power. Progressively offlining cores down to two physical cores saved only about 0.2–0.3 W, with inconsistent results across repeats. Automatic core offlining is not implemented.

Changing ASPM policies and permitting runtime suspend for unused Ethernet/SD devices produced no clear package-power improvement. The tested PCIe links retained ASPM disabled, and those devices remained active. Experimental settings were restored afterward.

Further display-off tests on 2026-10-04 used the updated daemon and 15% running floor. Each phase settled for 20 seconds, then collected twelve ten-second `turbostat` samples. The sequence was:

| Setting | Mean package power | Fan state |
| --- | --- | --- |
| Original `balanced` profile and USB settings | 4.59 W | Running |
| `power-saver` profile (EPP `power`) | 4.38 W | Running, then stopped |
| Original settings restored | 4.40 W | Running |
| Autosuspend permitted for two USB receivers and the POROSVOC device | 4.56 W | Stopped |
| Original settings restored again | 4.42 W | Stopped |

The power-saver result was close to the later baseline readings, so these tests did not establish a repeatable saving. USB autosuspend suspended one receiver, while the other receiver, POROSVOC device, and both affected USB controllers remained active. Both experiments restored the original power profile and USB settings; no persistent USB rules were installed. Fan3 measured around 870 RPM while running, and normal fan-stop/restart transitions occurred during the experiment. These transitions and the resulting temperature changes limit comparisons between phases. CPU utilization averaged approximately 0.4% across all logical CPUs, and advertised C3 residency was approximately 99%. The measurement session itself contributed background activity.

These are short observations, not controlled benchmarks. Package energy counters do not measure total wall consumption; use an external meter to assess whole-system or peripheral savings. AMDGPU's `power1_average` on this APU includes CPU power and must not be added to the package reading as a separate GPU measurement.

## Contributing

Follow the applicable [Linux kernel Rust coding guidelines](https://docs.kernel.org/rust/coding-guidelines.html): default `rustfmt` formatting, vertical grouped imports, sentence-style Markdown comments, and `///` documentation for item contracts. The trailing `//` in grouped imports preserves their vertical layout with stable `rustfmt`.

Document every unsafe block with a preceding `// SAFETY:` explanation. Prefer fallible validation and checked conversions for configuration inputs. Prefer narrowly scoped `#[expect(...)]` over lint suppression when an exception is justified. This userspace daemon retains `std` and Cargo; kernel APIs and kernel-specific C type aliases do not apply.

Before submitting, run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
cargo run -- validate gtr9-fan-control.conf
```

Install your toolchain's `rustfmt` component if `cargo fmt` is unavailable. Keep hardware validation separate from these checks and describe any hardware testing in the pull request.

## License

[MIT](LICENSE).
