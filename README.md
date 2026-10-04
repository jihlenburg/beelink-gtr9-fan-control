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
- Configuration rejects decreasing duties and non-increasing temperatures. The final point must cover the critical temperature and use 100% duty; critical temperatures independently override the curve.

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
  "fans": { "fan2_rpm": null, "fan3_rpm": 698 },
  "temperatures_c": {
    "cpu": 38.5,
    "control": 39,
    "motherboard": { "temp1": 38, "temp2": 46, "temp3": 46 }
  },
  "raw": {
    "pwm2": 31,
    "pwm2_enable": 1,
    "pwm3": 31,
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

## Configuration

The bundled configuration uses a 12% running-duty floor calibrated on the original test machine:

```ini
poll_seconds=2
fall_delay_seconds=30
hysteresis_c=4
critical_temp_c=85
fan_stall_seconds=10
curve=0:12,50:25,60:35,70:55,78:75,85:100
fan_stop_below_c=45
fan_resume_at_c=50
fan_stop_idle_seconds=30
```

On this machine, 8%, 10%, and 12% all produced approximately 690 RPM on fan3; 5% stopped it. The 12% floor retains margin without increasing measured speed. Only fan3 provides a usable tachometer signal. The 60 PWM automatic start value is specific to the original settings captured on this machine.

Fan-stop requires 30 seconds with every logical CPU at most 10% busy, GPU utilization at most 5%, and CPU/GPU, motherboard and SSD temperatures at or below 45C. Activity or any of these temperatures reaching 50C restarts the fans. Restart uses a one-second pulse at at least 35% duty. Omit `fan_stop_below_c` to disable fan-stop.

Each curve point is `temperature_C:duty_percent`. The curve selects the last point whose temperature is at or below the current reading; values are not interpolated. Duty increases on the next polling iteration and decreases only after the configured delay and hysteresis margin.

Edit `/etc/gtr9-fan-control.conf`, validate it, and restart to apply changes:

```sh
gtr9-fan-control validate /etc/gtr9-fan-control.conf
sudo systemctl restart gtr9-fan-control
```

Configuration is loaded at startup. Validation checks syntax and configuration constraints; it does not verify hardware compatibility or calibrate fan duty.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| `it8613 hwmon device not found` | Confirm the `it87` driver is loaded and exposes an `it8613` device. |
| Missing CPU/GPU sensors | Both `k10temp` and `amdgpu` must be available. |
| Missing ITE attribute | Verify the hardware and driver expose the expected PWM channels and fan3 tachometer. |
| Temperature or activity read failure | Inspect the logged error and sensor availability; auxiliary sensors and GPU activity are used by fan-stop mode. |
| Fan3 remains stopped after recovery | Restore firmware control and check the fan and calibrated running duty before restarting. |

## Uninstall

From the repository root:

```sh
sudo ./uninstall.sh
```

This stops and disables the service, attempts firmware restoration, and removes the installed binary and service unit. It retains `/etc/gtr9-fan-control.conf`.

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
