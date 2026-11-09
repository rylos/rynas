# Module `qnap` — specification

Status (2026-11-09): implemented, **not verified on real hardware**. The line mapping below comes
from the vendor's model file and reverse engineering of its HAL, cross-checked against the Linux
driver source. No LED, button or buzzer has been exercised on a real machine: treat every line in
the support matrix as untested until the [first-boot procedure](#first-boot-procedure) has been run
and the model's `verified` flag set.

## Module boundary

- `crates/rynas-hw-qnap`: `api` (documents, no feature, also linked by the web interface); `model`
  and `provider` (feature `identify`, rynas-web, read only); `gpio`, `beep`, `control`, `disks`,
  `domain` (`qnap.hw`), `http` (`/api/v1/modules/qnap/{status,settings,beep,mute}`), `module`
  (feature `domain`, rynasd).
- `crates/rynas-ui/src/modules/qnap`: Hardware › Front panel (shown only where the module answers).
- Registration: one line each in `rynasd::modules::detect`, `rynas-web`'s provider detection, the
  web interface's tab and dictionary list. `f71869a` is a known sensor chip in
  `rynas-web/src/metrics/temps.rs` (a chip name, not the module).

## Identification

QNAP's firmware fills DMI with placeholders, so DMI alone can identify nothing. On an HS-251:
system vendor/product `To be filled by O.E.M.`, board `AMI Corporation` / `Aptio CRB`. These are
common on Bay Trail boards. A model therefore matches only on its **whole signature**
(`model::MODELS`):

1. DMI vendor/product/board strings, exact;
2. the CPU model (`/proc/cpuinfo`);
3. the expected PCI devices at their addresses (for the HS-251: the Bay Trail AHCI, xHCI and PCIe
   root ports, an I210 on each of buses 3 and 4);
4. the NICs' vendor prefix (`00:08:9b`).

Then the Super I/O chip is confirmed from its own driver (`control::confirm`): the hwmon `name`
must be the model's chip (`f71869a`), and, when `gpio-f7188x` is loaded, its banks must have the
layout of that chip (F71869A: 6, 8, 8, 8, 8, 5, 8, 8 lines; the plain F71869 has 7 banks, bank 6
with 6 lines). The `Confirmed` value of that check is what the controller is built from: there is
no other way to reach a GPIO write. Anything else: no module, no tab, routes answer 404, nothing
written, and no driver list left behind on a machine whose chip turned out to be another one.

Each LED line must be reported as an output and each button as an input (by `gpioinfo`'s data, the
GPIO v2 ioctl) before it is used; a line in the wrong direction is left alone and reported in the
interface. rynas never changes a line's direction.

## Line mapping (HS-251)

`SIO:I<reg>:B<bit>` is the vendor's notation in `/etc/model.conf`. In `drivers/gpio/gpio-f7188x.c`
(`f71869a_gpio_bank[]`) each bank is a GPIO chip labelled `gpio-f7188x-<n>` with register base
`0xF0 - n * 0x10`: `base + 1` is the output data, `base + 2` the pin status. So the vendor's
register gives the bank directly (checked by a unit test, `vendor_registers_are_the_driver_banks`).

| Function | Vendor (`model.conf`) | gpio-f7188x | Polarity | Source |
|---|---|---|---|---|
| Status LED, green | `SIO:I91:B2` | bank 6 line 2, output | active low | model.conf, driver, HAL |
| Status LED, red | `SIO:I91:B3` | bank 6 line 3, output | active low | same |
| Reset button | `SIO:I92:B1` | bank 6 line 1, input | pressed = low | same |
| Bay 1 error LED | `SIO:I81:B0` | bank 7 line 0, output | active low | same |
| Bay 2 error LED | `SIO:I81:B1` | bank 7 line 1, output | active low | same |
| USB copy button | `SIO:IE2:B2` | bank 1 line 2, input | pressed = low | same |
| USB LED | `SIO:IE1:B7` | bank 1 line 7, output | active low | same |

Chips are found by label through `/dev/gpiochip*` (uAPI v2): no GPIO numbers are hardcoded.
`gpio-f7188x` has no interrupt support, so the buttons are polled every 125 ms.

Bays: bay 1 is AHCI port 1 and bay 2 AHCI port 0 of `0000:00:13.0` (the vendor's `DEV_PORT`, taken
as 0-based: to be checked against `/sys/class/ata_port/ataN/port_no` on first boot).

## Support matrix

"Implemented" means the code exists and passes its tests against fake hardware. Nothing below has
been tested on a real HS-251.

| Capability | Vendor mechanism | Verdict | Linux path in rynas |
|---|---|---|---|
| Status LED: green, red, off | SIO GPIO | implemented | gpiod, bank 6 |
| Status LED: green blink, red blink (125 ms toggle), alternate (500 ms) | software loop in the HAL | implemented | software blink, same timings |
| Status LED follows the system | priority logic in the HAL | implemented, simplified | Auto: green, red blink on a disk warning, red on a failure |
| Bay error LEDs | SIO GPIO, on/off | implemented | gpiod; failed disk steady, warning blinking (rynas's choice), from SMART and md's failed members |
| USB LED: on, off, blink (125 ms) | SIO GPIO + software loop | implemented | gpiod |
| Front LED brightness (`LED_BV_CTRL = GPIO`, "night mode") | register 0xA1 bits 0-3 (bank 5 lines 0-3): level 0 all low, 1 lines 0/2 low, 2+ none low | **not implemented** | The vendor code does not settle which level is brightest and the lines' default state is unknown. Determine it during the first-boot test, then add as data. |
| Reset button | polled, 3 s beep, 3-14 s resets password/network, 15 s+ restores config | events only, by design | gpiod poll. Recorded and/or notified; **never** resets anything |
| USB copy button | polled, short press starts a copy | events only | gpiod poll; no copy action yet |
| Power button | ACPI (a separate vendor path via LDN 6 reg 0xB2 exists) | not this module | ACPI input device, handled by systemd-logind |
| Buzzer | PC speaker: PIT channel 2, divisor 0x05D3 (about 800 Hz), gated by port 0x61, in `sub_83340` of the HAL | implemented | `pcspkr` input device, `EV_SND`/`SND_TONE`; no port access |
| Buzzer patterns | vendor modes: short 500 ms, long 1 s; 0 short, 1 long, 2 three short, 3-7 two long, 8-9 three long, 12-13 long-short-short, 14 three quick short | implemented | `beep::pattern` |
| Disk-failure alarm | the vendor code ties no pattern to disk failure as such (broken drive plugged in uses mode 14) | implemented | mode 14 pattern, repeated every 5 minutes until muted |
| Temperatures | `CPU_TEMP_UNIT=SIO:1` (temp1), `SYSTEM_TEMP_UNIT=SIO:3` (temp3), via hwmon | implemented | `f71882fg` hwmon: temp3 shown as "System"; CPU from `coretemp` |
| Fan | `MAX_FAN_NUM=0` | not applicable | fanless |
| Power recovery after outage | LDN 0xA reg 0xF4 bits 2:1, with a CMOS mirror at 0x61 | **not implemented** | Raw Super I/O port writes only, no Linux driver exposes it. Set it in the BIOS setup. |
| Wake-on-LAN | not settled by the vendor code (no ethtool, HAL returns "not implemented" without an MCU) | generic | NIC WoL with standard tools; the board's S5 wake behaviour is unverified |
| Scheduled power-on | no RTC wake in the HAL | generic | rynas's power schedule on `rtc0` (`rtc_cmos`); whether this firmware honours the alarm from S5 is unverified |
| EuP (low-power standby) | LDN 0xA reg 0xE0/0xE1 + CMOS | **not implemented** | Raw Super I/O port access only; BIOS setting |
| HDMI | CAP bit set, no HAL control path | not applicable | standard i915, nothing to control |
| IR receiver | `fintek-cir` loads on QTS; nothing in the HAL | not applicable | kernel input device (`fintek-cir`, `CONFIG_IR_FINTEK=m`); rynas has no use for a remote |
| Watchdog | `iTCO_wdt` is loaded; no HAL path | not in this module | rynas has no generic watchdog path; left to the system |
| LCD panel, voice alert | no such model data (`VOICE_ALERT_SUPPORT=0`) | not applicable | |
| Per-bay disk power, USB port power, PCIe slot power | no control path for this model | not applicable | |
| UPS | NUT | core | rynas's generic NUT page (0925:1234 is a NUT device) |

## Kernel modules

The module writes `/etc/modules-load.d/rynas-qnap.conf` (`gpio-f7188x`, `f71882fg`, `pcspkr`) at the
start of rynasd, only on an identified machine, and loads them at once with `modprobe` in a
transient unit outside rynasd's sandbox (`ProtectKernelModules`), through `rynas_tasks::Outside`.

Debian 13 kernel config, read from the package `linux-config-6.12` (6.12.107-1, amd64,
`config.amd64_none_amd64`): `CONFIG_GPIO_F7188X=m`, `CONFIG_SENSORS_F71882FG=m`,
`CONFIG_INPUT_PCSPKR=m`, `CONFIG_PCSPKR_PLATFORM=y`, `CONFIG_GPIO_CDEV=y`, `CONFIG_ITCO_WDT=m`,
`CONFIG_IR_FINTEK=m`. If `pcspkr` is blacklisted on an installation, systemd-modules-load skips it;
the interface then reports that no speaker was found.

## Install notes

**Bay Trail stability.** Freezes on this CPU family are widely reported and fixed with
`intel_idle.max_cstate=1` on the kernel command line, at the cost of a little more power use
([Debian wiki](https://wiki.debian.org/InstallingDebianOn/PageFragmentIntelBayTrailCStateIssue),
[kernel bug 109051](https://bugzilla.kernel.org/show_bug.cgi?id=109051)). It is an install-time
decision: add it to `GRUB_CMDLINE_LINUX_DEFAULT` and run `update-grub`. rynas does not change the
boot configuration.

## Prior art

- `qnap8528` and `QNAP-EC`: drivers for the IT8528 embedded controller of other QNAP models, not
  this Super I/O.
- `leds-qnap-mcu` (Linux 6.18): the MCU of ARM QNAP models.
- A published reverse-engineering note for the TS-453 Pro shows the same F71869A controlled by
  writing its registers directly; rynas uses the mainline `gpio-f7188x` driver instead.

## First-boot procedure

Run this on Debian on the real machine before turning on "Let rynas drive the LEDs". Keep that
setting off while testing: with it on, rynasd holds the LED lines and they are busy. Install the
`gpiod` package (libgpiod 2.x) and read each state before changing it.

1. **Identification and drivers**

       cat /sys/class/dmi/id/{bios_vendor,sys_vendor,product_name,board_vendor,board_name}
       modprobe -a f71882fg gpio-f7188x pcspkr
       cat /sys/class/hwmon/hwmon*/name          # expect f71869a
       gpiodetect                                # gpio-f7188x-0 ... -7, lines 6,8,8,8,8,5,8,8

   Note the `gpiochipN` of each bank (`gpio-f7188x-6` etc.); the numbers below are placeholders.

2. **Read the lines, change nothing** (`-a`: as-is, the direction is not touched)

       gpioinfo -c gpiochipB6 | sed -n 1,9p       # lines 2,3 should be output, line 1 input
       gpioinfo -c gpiochipB7 | sed -n 1,3p       # lines 0,1 output
       gpioinfo -c gpiochipB1 | sed -n 1,9p       # line 7 output, line 2 input

   If a line that should be an output reads as an input, or the reverse, **stop**: the mapping or
   the BIOS setup differs from what the module expects, and it will refuse to use that line.

3. **Buttons (read only)**: press and hold while it prints; expect 1 at rest, 0 while pressed.

       while :; do gpioget -a -c gpiochipB6 1; sleep 0.2; done     # reset button
       while :; do gpioget -a -c gpiochipB1 2; sleep 0.2; done     # USB copy button

4. **One LED at a time** (active low: 0 lights it). Only on a line step 2 showed as an output:

       timeout 3 gpioset -c gpiochipB6 2=0   # status green on for 3 s
       timeout 1 gpioset -c gpiochipB6 2=1   # off
       timeout 3 gpioset -c gpiochipB6 3=0   # status red
       timeout 1 gpioset -c gpiochipB6 3=1

   Repeat for the bay error LEDs (bank 7 lines 0 and 1, checking which bay lights) and the USB LED
   (bank 1 line 7). If an LED does not react, the GPIO logical device may be inactive: the vendor
   HAL activates it (LDN 6, register 0x30) and `gpio-f7188x` does not. Do not write that register
   by hand; report the result.

5. **Buzzer**: `ls /sys/class/input/*/device/name` must show `PC Speaker`. Then use "Test beep" on
   Hardware › Front panel.

6. **Bays**: with a disk in bay 1 only, `cat /sys/class/ata_port/ata*/port_no` and
   `readlink -f /sys/block/sd?/device` must show that disk on the port the table gives for bay 1.

When everything matches, set `verified: true` for the model in `model::MODELS` and turn on the
setting.

## Adding a model

Add an entry to `model::MODELS` with values read from the machine: its DMI strings, CPU, PCI
devices, NIC prefix, the chip's hwmon name and GPIO bank layout, the lines from the vendor's
`model.conf`, the bays from its `[System Disk N]` sections. Add the model's register checks to the
unit test. Leave `verified: false` until the first-boot procedure has been run on that model.
