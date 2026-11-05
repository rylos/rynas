"""Fake DS920+ tree for rynasd --hw-root (never the host's RTC/LEDs): writes the tree once, then
refreshes synohwd's status.json every 10 s so it stays fresh. Usage: fakehw.py <root> <fixture>."""
import json, os, sys, time

root, fixture = sys.argv[1], sys.argv[2]


def w(rel, text):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as f:
        f.write(text)


os.makedirs(os.path.join(root, "sys/devices/platform/synology-nas"), exist_ok=True)
w("sys/class/dmi/id/sys_vendor", "Synology Inc.\n")
w("sys/class/dmi/id/product_name", "DS920+\n")
w("proc/uptime", "120.00 60.00\n")
w("proc/mdstat", "Personalities : [raid1]\nunused devices: <none>\n")
w("proc/sys/kernel/random/boot_id", open("/proc/sys/kernel/random/boot_id").read())
os.makedirs(os.path.join(root, "sys/devices/pnp0/00:02"), exist_ok=True)
w("sys/class/rtc/rtc0/wakealarm", "")
link = os.path.join(root, "sys/class/rtc/rtc0/device")
if not os.path.islink(link):
    os.symlink("../../../devices/pnp0/00:02", link)
for bay in range(1, 5):
    for color in ("green", "orange"):
        d = f"sys/class/leds/syno:{color}:disk{bay}"
        if color == "green":
            w(d + "/trigger", f"none [ata{bay}] timer\n")
            w(d + "/brightness", "255\n")
        else:
            w(d + "/trigger", "[none] timer\n")
            w(d + "/brightness", "0\n")
base = json.load(open(fixture))
# the DS920+'s two M.2 slots (synohwd's slot map: slot 1 = port 00:14.1, slot 2 = 00:14.0),
# in case the fixture predates it; controller numbers swap between boots
base.setdefault("m2", {
    "1": {"present": True, "controller": "nvme1", "device": "nvme1n1",
          "pci": "0000:05:00.0", "port": "0000:00:14.1"},
    "2": {"present": True, "controller": "nvme0", "device": "nvme0n1",
          "pci": "0000:04:00.0", "port": "0000:00:14.0"},
})
while True:
    now = time.time()
    base["epoch"] = int(now)
    base["time"] = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(now))
    tmp = os.path.join(root, "run/synohwd/status.json.tmp")
    os.makedirs(os.path.dirname(tmp), exist_ok=True)
    with open(tmp, "w") as f:
        json.dump(base, f)
    os.replace(tmp, os.path.join(root, "run/synohwd/status.json"))
    time.sleep(10)
