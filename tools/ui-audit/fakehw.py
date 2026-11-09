"""Fake DS920+ tree for rynasd --hw-root (never the host's RTC/LEDs): writes the tree once, then
refreshes synohwd's status.json every 10 s so it stays fresh. Usage: fakehw.py <root> <fixture>.
With the fixture "generic" it writes a plain PC instead (no Synology platform, no synohwd): the
G8d check that no Synology screen, tab or hint shows up on generic hardware."""
import json, os, sys, time

root, fixture = sys.argv[1], sys.argv[2]
# QNAP=1: a fake QNAP HS-251 (the module's whole signature, its chip's driver, two bays)
qnap = bool(os.environ.get("QNAP"))
generic = fixture == "generic" or qnap


def w(rel, text):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as f:
        f.write(text)


if not generic:
    os.makedirs(os.path.join(root, "sys/devices/platform/synology-nas"), exist_ok=True)
w("sys/class/dmi/id/sys_vendor", "Example Corp.\n" if generic else "Synology Inc.\n")
w("sys/class/dmi/id/product_name", "Generic PC\n" if generic else "DS920+\n")
w("proc/uptime", "120.00 60.00\n")
w("proc/mdstat", "Personalities : [raid1]\nunused devices: <none>\n")
w("proc/sys/kernel/random/boot_id", open("/proc/sys/kernel/random/boot_id").read())
os.makedirs(os.path.join(root, "sys/devices/pnp0/00:02"), exist_ok=True)
w("sys/class/rtc/rtc0/wakealarm", "")
link = os.path.join(root, "sys/class/rtc/rtc0/device")
if not os.path.islink(link):
    os.symlink("../../../devices/pnp0/00:02", link)
for bay in range(1, 1 if generic else 5):
    for color in ("green", "orange"):
        d = f"sys/class/leds/syno:{color}:disk{bay}"
        if color == "green":
            w(d + "/trigger", f"none [ata{bay}] timer\n")
            w(d + "/brightness", "255\n")
        else:
            w(d + "/trigger", "[none] timer\n")
            w(d + "/brightness", "0\n")
if qnap:
    dmi = "sys/class/dmi/id/"
    w(dmi + "bios_vendor", "American Megatrends Inc.\n")
    w(dmi + "sys_vendor", "To be filled by O.E.M.\n")
    w(dmi + "product_name", "To be filled by O.E.M.\n")
    w(dmi + "board_vendor", "AMI Corporation\n")
    w(dmi + "board_name", "Aptio CRB\n")
    w("proc/cpuinfo", "model name\t: Intel(R) Celeron(R)  CPU  J1800  @ 2.41GHz\n")
    for addr, ident in (("00:13.0", "0f23"), ("00:14.0", "0f35"), ("00:1c.2", "0f4c"),
                        ("00:1c.3", "0f4e"), ("03:00.0", "1533"), ("04:00.0", "1533")):
        w(f"sys/bus/pci/devices/0000:{addr}/vendor", "0x8086\n")
        w(f"sys/bus/pci/devices/0000:{addr}/device", f"0x{ident}\n")
    for i, addr in enumerate(("03:00.0", "04:00.0")):
        os.makedirs(os.path.join(root, f"sys/bus/pci/devices/0000:{addr}/net/eth{i}"), exist_ok=True)
        w(f"sys/class/net/eth{i}/address", f"00:08:9b:00:00:0{i}\n")
    w("sys/class/hwmon/hwmon1/name", "f71869a\n")
    w("sys/class/hwmon/hwmon1/temp1_input", "41000\n")
    w("sys/class/hwmon/hwmon1/temp3_input", "33000\n")
    for disk, ata, port in (("sda", 1, 1), ("sdb", 2, 2)):
        dev = f"sys/devices/pci0000:00/0000:00:13.0/ata{ata}/host0/target0:0:0/0:0:0:0"
        os.makedirs(os.path.join(root, dev), exist_ok=True)
        link = os.path.join(root, f"sys/block/{disk}/device")
        os.makedirs(os.path.dirname(link), exist_ok=True)
        if not os.path.islink(link):
            os.symlink(os.path.join(root, dev), link)
        w(f"sys/class/ata_port/ata{ata}/port_no", f"{port}\n")
    w("run/qnap-fake-disks", "sda healthy\nsdb failing\n")
if generic:
    while True:  # nothing to refresh; stays alive like the DS920+ tree (stop.sh kills the PID)
        time.sleep(3600)
base = json.load(open(fixture))
# the DS920+'s two M.2 slots (synohwd's slot map: slot 1 = port 00:14.1, slot 2 = 00:14.0),
# in case the fixture predates it; controller numbers swap between boots
base.setdefault("m2", {
    "1": {"present": True, "controller": "nvme1", "device": "nvme1n1",
          "pci": "0000:05:00.0", "port": "0000:00:14.1"},
    "2": {"present": True, "controller": "nvme0", "device": "nvme0n1",
          "pci": "0000:04:00.0", "port": "0000:00:14.0"},
})
# FAKE_LEDS=1: a synohwd with the front LED level chip (synodeb >= 4.8, scale 0..3 like a
# DS923+), so Hardware > Synology shows the front LED schedule; without it the fields are absent
# (an older synohwd) and the card stays hidden
if os.environ.get("FAKE_LEDS"):
    base.update({"led_schedule_supported": True, "led_level_min": 0, "led_level_max": 3,
                 "led_level_default": 3, "led_level_now": 1, "led_mode_now": "reduced"})
rpm0 = [f.get("rpm") for f in base.get("fans", [])]
tick = 0
while True:
    now = time.time()
    base["epoch"] = int(now)
    # the fans drift by a few rpm, as on the unit: every write is a real change that rynas-web
    # pushes as `hardware.status` (idle_dom.py checks that pages survive it)
    tick += 1
    for f, r in zip(base.get("fans", []), rpm0):
        if r:
            f["rpm"] = r + (tick % 3) * 5
    base["time"] = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(now))
    tmp = os.path.join(root, "run/synohwd/status.json.tmp")
    os.makedirs(os.path.dirname(tmp), exist_ok=True)
    with open(tmp, "w") as f:
        json.dump(base, f)
    os.replace(tmp, os.path.join(root, "run/synohwd/status.json"))
    time.sleep(10)
