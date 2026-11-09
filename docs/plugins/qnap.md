# Module `qnap` — specification

Status (2026-11-09): implemented, **not verified on real hardware**. The line mapping below comes
from the vendor's model file and reverse engineering of its HAL and web interface, cross-checked
against the Linux driver source. No LED, button, buzzer or Super I/O register has been exercised on
a real machine: treat every line in the support matrix as untested until the
[first-boot procedure](#first-boot-procedure) has been run and the model's `verified` flag set.

## Module boundary

- `crates/rynas-hw-qnap`: `api` (documents, no feature, also linked by the web interface); `model`
  and `provider` (feature `identify`, rynas-web, read only); `gpio`, `beep`, `control`, `disks`,
  `superio` (the Super I/O worker), `domain` (`qnap.hw`), `http`
  (`/api/v1/modules/qnap/{status,settings,beep,mute,confirm-mapping,power-recovery}`), `module`
  (feature `domain`, the default, rynasd).
- `crates/rynas-ui/src/modules/qnap`: Hardware › Front panel (shown only where the module answers).
- Registration: one line each in `rynasd::modules::detect`, `rynas-web`'s provider detection, the
  web interface's tab and dictionary list, and the `rynasctl qnap` subcommand.
- Generic extension points the module uses: `Module::attach` (the notification engine),
  `Module::storage_update` (the verdicts of rynasd's periodic storage check, so that no second
  reader of SMART exists), `Module::stop`, and `HardwareStatus.board_temps`.

## Identification

QNAP's firmware fills DMI with placeholders, so DMI alone can identify nothing. On an HS-251:
system vendor/product `To be filled by O.E.M.`, board `AMI Corporation` / `Aptio CRB`. These are
common on Bay Trail boards. Identification has three layers, each of which must hold:

1. **The signature** (`model::identify`): DMI strings, CPU model, **exactly** the model's PCI device
   set (one more or one less device is another machine), as many ATA ports on
   the AHCI controller as the model has bays, and the NICs' vendor prefix (`00:08:9b`). Firmware
   strings and CPU alone are only "plausible": the module waits up to 2 minutes for the rest (NICs
   may appear late) and otherwise stays out. A machine that does not even look plausible has the
   driver list removed, in case its disks came from a QNAP.
2. **The chip, from its own driver** (`control::confirm`): the hwmon `name` must be the model's chip
   (`f71869a`), on a fanless model every tachometer must read 0, and when `gpio-f7188x` is loaded its
   banks must have the layout of that chip (6, 8, 8, 8, 8, 5, 8, 8 lines). An unreadable chip list
   confirms nothing. The `Confirmed` value of that check is what the controller and the Super I/O
   worker are built from: there is no other way to reach a GPIO write or a port.
3. **The user** (while `verified` is false): LED writes and every Super I/O port access stay off
   until someone who has run the first-boot procedure types the model name in Hardware › Front
   panel (`POST confirm-mapping`). The confirmation is kept in the state directory bound to this
   machine's MAC addresses, so it does not follow the disks to another machine, and it can be
   withdrawn. The LED switch alone does not do it. The worker checks the confirmation itself, so
   `rynasctl qnap power-recovery` is refused without it. `rynasctl qnap led-test` is not: it is the
   test the confirmation is the outcome of, and it only uses the kernel's GPIO driver.

**What this can and cannot exclude.** A 4-bay sibling (TS-451, TS-453) fails on ATA ports and PCI
devices. A sibling with another device set fails on PCI. A fan turning fails the fanless check. It
**cannot** exclude a two-bay model with the same CPU, chip and PCI devices whose fan is stopped: the
model's name lives only in the board's I²C EEPROM (`VPD_MB`), which the module does not read. That
residual risk is what the third layer is for.

The drivers are loaded in order: `f71882fg` first (it names the chip), the others only after the
chip is confirmed; the driver list is written only then.

Each LED line must already be an output and each button an input (GPIO v2 line info) before it is
used; a line in the wrong direction is left alone and reported in the interface.

**Lines are requested "as is"**, with no direction or drive flag: the kernel then neither rewrites
the direction register nor forces push-pull (requesting an output would write `regbase + 3`, and
that survives the process). The vendor's HAL never touches direction or output-mode registers on
these lines, so only the data register changes, as under QTS.

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
| LED brightness | register `0xA1` bits 0-3 | bank 5 lines 0-3, output | raw levels below | HAL, web interface |

Chips are found by label through `/dev/gpiochip*` (uAPI v2): no GPIO numbers are hardcoded.
`gpio-f7188x` has no interrupt support, so the buttons are polled every 125 ms.

Bays: bay 1 is AHCI port 1 and bay 2 AHCI port 0 of `0000:00:13.0` (the vendor's `DEV_PORT`, taken
as 0-based: to be checked against `/sys/class/ata_port/ataN/port_no` on first boot).

**Brightness, evidence chain.** QTS's Hardware page (`hardware.js`) has a slider 0 to 100 in steps of
50 when `led_type` is 0, which `SE_Get_LED_BV_Ctrl_Capability` returns for `LED_BV_CTRL = GPIO`.
`Set_LED_Night_Mode_Status` (`libuLinux_Util`) maps 0/50/100 to GPIO mode 0/1/2 and calls
`/etc/init.d/set_led.sh`, which runs `hal_app --led_brightness_in_night_mode mode=N`. In the script
mode 2 is the normal daytime state (restored at the end of the night period and the 100 % default)
and mode 0 is what "LED Enable = FALSE" uses, so **100 % (mode 2) is the brightest and 0 % is off**.
`sio_sys_set_led_in_night_mode` then sets register `0xA1`: mode 0 all four bits low, mode 1 bits 0
and 2 low and bits 1 and 3 high, mode 2 all high.

**Power recovery, evidence chain.** The Power Recovery page sends `LAST_STATE`, `POWER_ON` or
`POWER_OFF` (`sysRequest.cgi`), stored as 0, 1, 2; `Set_Power_Recovery_Mode` (`libuLinux_config`) maps
them to HAL modes 0, 2, 3; `sio_sys_set_power_recovery_mode` does, on port 0x2E: `87 87`, select
logical device `0x0A`, read register `0xF4`, write `(value & 0xF9) | (mode << 1)` (table: 0→00, 2→10,
3→11), `AA`. Then `comm_sys_set_power_recovery_mode_to_cmos` updates CMOS byte `0x61` (bits 7:6, low
six kept), encoded as 00/10/11 when the byte's low five bits equal 1 and as 10/01/00 otherwise. The
reader takes the chip's value and falls back on CMOS only for `01`. Whether "power on" is offered on
this model in QTS depends on `bIsStorageV2 && smbSupport.powerSchedule`, which could not be read
without running the CGI.

QTS disables these choices while EuP is on, so rynas reads EuP the vendor's way (`0xE0` bit 7 when
the CMOS byte is initialized, otherwise `0xE1` bits 3:2) and refuses the write then. The worker reads
the logical device back before and after every write, compares the whole register afterwards, and
restores the register if the CMOS copy cannot be written.

**Serialization.** The chip's configuration mode is one state, and `request_muxed_region`, the
kernel's lock for it, cannot be taken from user space. rynasd holds `/run/rynas/qnap-superio.lock`
while it polls and releases it only with its own GPIO access stopped, for the worker's whole run;
the worker needs that lock, so a direct `rynasctl qnap power-recovery` is refused while rynasd is
running. Unbinding `gpio-f7188x` around the access was considered and rejected: it would drop the
held line requests for no gain, since rynasd is the only regular user. What remains: `f71882fg`
enters only at its init, `fintek-cir` at probe, suspend and when its device is opened, and an
administrator's `gpioinfo` or `gpioget` can do so at any moment. Such an access in the few port
operations between the last logical-device check and the write cannot be excluded; the checks turn
every wider overlap into an error.

**GPIO logical device.** The vendor's HAL writes LDN 6 register `0x30` = 1 before using these lines
and `gpio-f7188x` does not, so writes may succeed with no visible effect. The Front panel shows the
activation state (after confirmation) and offers the vendor's activation through the same worker.
"Commanded" in the interface means rynas wrote the line, not that an LED changed.

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
| Front LED brightness (`LED_BV_CTRL = GPIO`) | register 0xA1 bits 0-3, three levels | implemented | gpiod, bank 5 lines 0-3, 100/50/0 % as in QTS |
| Reset button | polled, 3 s beep, 3-14 s resets password/network, 15 s+ restores config | events only, by design | gpiod poll. Recorded and/or notified; **never** resets anything |
| USB copy button | polled, short press starts a copy | events only | gpiod poll; no copy action yet |
| Power button | ACPI (a separate vendor path via LDN 6 reg 0xB2 exists) | not this module | ACPI input device, handled by systemd-logind |
| Buzzer | PC speaker: PIT channel 2, divisor 0x05D3 (about 800 Hz), gated by port 0x61, in `sub_83340` of the HAL | implemented | `pcspkr` input device, `EV_SND`/`SND_TONE`; no port access |
| Buzzer patterns | vendor modes: short 500 ms, long 1 s; 0 short, 1 long, 2 three short, 3-7 two long, 8-9 three long, 12-13 long-short-short, 14 three quick short | implemented | `beep::pattern` |
| Disk-failure alarm | the vendor code ties no pattern to disk failure as such (broken drive plugged in uses mode 14) | implemented | mode 14 pattern, repeated every 5 minutes until muted; a mute lasts across restarts until that disk's failure clears |
| Temperatures | `CPU_TEMP_UNIT=SIO:1` (temp1), `SYSTEM_TEMP_UNIT=SIO:3` (temp3), via hwmon | implemented | the provider reports temp3 as "System" (`board_temps`); CPU from `coretemp` |
| Fan | `MAX_FAN_NUM=0` | not applicable | fanless |
| Power recovery after outage | LDN 0x0A reg 0xF4 bits 2:1, BIOS copy in CMOS 0x61 | implemented, on request only | `rynasctl qnap power-recovery` in a transient unit (rynasd's seccomp forbids `ioperm`): read-modify-write of those bits, ids checked, read back; CMOS through the kernel's `cmos_nvram`; never at start |
| Wake-on-LAN | not settled by the vendor code (no ethtool, HAL returns "not implemented" without an MCU) | generic | NIC WoL with standard tools; the board's S5 wake behaviour is unverified |
| Scheduled power-on | no RTC wake in the HAL | generic | rynas's power schedule on `rtc0` (`rtc_cmos`); whether this firmware honours the alarm from S5 is unverified |
| EuP (low-power standby) | same logical device, but reg 0xE0 bit 7 or a whole-register write of 0xE1, chosen by a CMOS state unknown until the box runs Debian | **not implemented** | The vendor's second branch overwrites 0xE1 whole (0xCC/0xC0), which is not a read-modify-write of known bits. BIOS setting. |
| HDMI | CAP bit set, no HAL control path | not applicable | standard i915, nothing to control |
| IR receiver | `fintek-cir` loads on QTS; nothing in the HAL | not applicable | kernel input device (`fintek-cir`, `CONFIG_IR_FINTEK=m`); rynas has no use for a remote |
| Watchdog | `iTCO_wdt` is loaded; no HAL path | not in this module | rynas has no generic watchdog path; left to the system |
| LCD panel, voice alert | no such model data (`VOICE_ALERT_SUPPORT=0`) | not applicable | |
| Per-bay disk power, USB port power, PCIe slot power | no control path for this model | not applicable | |
| UPS | NUT | core | rynas's generic NUT page (0925:1234 is a NUT device) |

## Kernel modules

Once the signature matches, the module loads `f71882fg` with `modprobe` in a transient unit outside
rynasd's sandbox (`ProtectKernelModules`, `rynas_tasks::Outside`) and has the chip confirmed by it;
only then come `gpio-f7188x` and `pcspkr`, and `/etc/modules-load.d/rynas-qnap.conf` is written. The
file is removed when the machine does not match, so that disks moved elsewhere stop probing.

`pcspkr` keeps a tone until told otherwise: rynas silences it at start and at shutdown, but a
SIGKILL or a crash in the middle of a beep leaves the tone on until rynasd next starts.

Super I/O accesses from the worker run with rynasd's GPIO polling stopped; see "Serialization".

Debian 13 kernel config, read from the package `linux-config-6.12` (6.12.107-1, amd64,
`config.amd64_none_amd64`): `CONFIG_GPIO_F7188X=m`, `CONFIG_SENSORS_F71882FG=m`,
`CONFIG_INPUT_PCSPKR=m`, `CONFIG_PCSPKR_PLATFORM=y`, `CONFIG_GPIO_CDEV=y`, `CONFIG_ITCO_WDT=m`,
`CONFIG_IR_FINTEK=m`, `CONFIG_RTC_DRV_CMOS=y`, `CONFIG_RTC_NVMEM=y`, `CONFIG_NVMEM_SYSFS=y`. If `pcspkr` is blacklisted on an installation, systemd-modules-load skips it;
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

Run this on Debian on the real machine before confirming the model in Hardware › Front panel.
Stop rynasd first (`systemctl stop rynasd rynas-web`): while it runs it holds the button lines, and
the commands below would get "busy". Install the `gpiod` package (libgpiod 2.x). Every step reads
the current state before anything changes.

**Do not use `gpioset`.** It requests the line as an output, which forces push-pull
(`regbase + 3`) and survives the command. Step 4 uses `rynasctl qnap led-test`, which requests the
line as is, like rynasd does.

1. **Identification and drivers**

       cat /sys/class/dmi/id/{bios_vendor,sys_vendor,product_name,board_vendor,board_name}
       ls /sys/bus/pci/devices | wc -l           # 31
       ls -d /sys/bus/pci/devices/0000:00:13.0/ata*   # two
       modprobe f71882fg
       cat /sys/class/hwmon/hwmon*/name          # expect f71869a
       grep . /sys/class/hwmon/hwmon*/fan?_input # expect 0 (fanless)
       modprobe -a gpio-f7188x pcspkr
       gpiodetect                                # gpio-f7188x-0 ... -7, lines 6,8,8,8,8,5,8,8

   Note the `gpiochipN` of each bank; `gpiochipB6` below means the chip of `gpio-f7188x-6`.

2. **Read the lines, change nothing**

       gpioinfo -c gpiochipB6 | sed -n 1,9p       # lines 2,3 output, line 1 input
       gpioinfo -c gpiochipB7 | sed -n 1,3p       # lines 0,1 output
       gpioinfo -c gpiochipB1 | sed -n 1,9p       # line 7 output, line 2 input
       gpioinfo -c gpiochipB5 | sed -n 1,5p       # lines 0-3 output (brightness)

   If a line that should be an output reads as an input, or the reverse, **stop**: the mapping or
   the BIOS setup differs from what the module expects, and it will refuse to use that line.

3. **Buttons (read only, as is)**: press and hold while it prints; expect 1 at rest, 0 pressed.

       while :; do gpioget -a -c gpiochipB6 1; sleep 0.2; done     # reset button
       while :; do gpioget -a -c gpiochipB1 2; sleep 0.2; done     # USB copy button

4. **One LED at a time**: it lights for 3 s and the level it had is put back.

       /usr/libexec/rynas/rynasctl qnap led-test status-green 3
       /usr/libexec/rynas/rynasctl qnap led-test status-red 3
       /usr/libexec/rynas/rynasctl qnap led-test bay1 3      # check which bay lights
       /usr/libexec/rynas/rynasctl qnap led-test bay2 3
       /usr/libexec/rynas/rynasctl qnap led-test usb 3

   If an LED does not react, the GPIO logical device may be inactive: the vendor HAL activates it
   (LDN 6, register 0x30) and `gpio-f7188x` does not. Do not write that register by hand; report
   the result.

5. **Buzzer**: `grep -H . /sys/class/input/*/device/name` must list `PC Speaker`. It is tested from
   the interface in step 9.

6. **Bays**: with a disk in bay 1 only, `cat /sys/class/ata_port/ata*/port_no` and
   `readlink -f /sys/block/sd?/device` must show that disk on the port the table gives for bay 1.

7. **Confirm** (`systemctl start rynasd rynas-web`): type the model name in Hardware › Front panel.
   No port of the Super I/O is touched before this step.

8. **Read the Super I/O, change nothing.** The card "After a power outage" reads it when it
   opens: the current setting, EuP, and whether the GPIO device is activated. Compare with the
   BIOS setup. With rynasd stopped the same reading is
   `/usr/libexec/rynas/rynasctl qnap power-recovery read`
   (`{"mode":"last_state","sio_bits":0,"cmos":33,"eup":false,"gpio_active":true}`). Note the
   values. If `mode` is null or contradicts the BIOS setup, do not write it. If the LEDs did not
   react in step 4 and `gpio_active` is false, press "Activate the GPIO device" and repeat step 4
   with rynasd stopped; otherwise leave it alone.

9. **Then the writes, one at a time**: turn on "Let rynas drive the LEDs", try each brightness
   level and note which is brightest (it should be 100 %), press "Test beep", and only then change
   the setting "After a power outage". Read it again as in step 8 and in the BIOS setup, then test
   by cutting mains power.

When everything matches, set `verified: true` for the model in `model::MODELS`.

## Adding a model

Add an entry to `model::MODELS` with values read from the machine: its DMI strings, CPU, the
complete PCI device set, NIC prefix, the chip's hwmon name and GPIO bank layout, the lines from the vendor's
`model.conf`, the bays from its `[System Disk N]` sections. Add the model's register checks to the
unit test. Leave `verified: false` until the first-boot procedure has been run on that model.
