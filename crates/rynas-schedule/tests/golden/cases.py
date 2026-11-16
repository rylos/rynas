#!/usr/bin/env python3
"""Writes cases.txt: the calendar expressions, time zones and base times whose next runs
gen.sh asks `systemd-analyze calendar` for (the base times around each DST change of 2026).

Run: python3 cases.py > cases.txt
"""
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

ZONES = [
    "Europe/Rome", "America/New_York", "Australia/Lord_Howe", "Europe/Dublin", "America/Havana",
    "America/Santiago", "Pacific/Chatham", "Africa/Casablanca", "UTC",
]

DST_EXPRS = [
    "*-*-* 02:30:00", "*-*-* 02:00:00", "*-*-* 01:30:00", "*-*-* 01:45:00", "*-*-* 02:15:00",
    "*-*-* 02:45:00", "*-*-* 03:00:00", "*-*-* 00:00:00", "*-*-* 00:30:00", "*-*-* 23:30:00",
    "hourly", "*-*-* *:15:00", "*-*-* *:00/20:00", "*-*-* *:00/10:00", "*-*-* 00/2:00:00",
    "*-*-* 01..04:00,30:00", "*-*-* 02,03:30:00", "*-*-* 02:15,45:00", "Sun *-*-* 02:30:00",
    "Mon..Fri *-*-* 02:30:00", "Sat,Sun *-*-* 00:00:00", "daily", "weekly", "monthly",
    "*-*~01 02:30:00", "Sun *-*-01..07 02:30:00", "Sun *-*~07/1 02:30:00", "*-*-* 01:59:59",
]

# Plain calendar arithmetic (no DST): ranges, repetition, end of month, leap days, weekdays.
PLAIN_EXPRS = [
    "*-*-* 03:00:00", "Mon,Wed,Fri *-*-* 18:45:00", "*-*-01,15 04:00:00", "*-02-29 12:00:00",
    "*-*-31 00:00:00", "*-*~01 23:00:00", "*-*~03..01 06:00:00", "*-02~01 00:00:00",
    "Fri *-*-13 13:13:00", "*-01,04,07,10-01 00:00:00", "quarterly", "semiannually", "yearly",
    "2027..2030-06-01 00:00:00", "*-*-* 08..18/3:00:00", "*-*-* 08..18:00/15:00",
    "*-*-* *:*:00", "*-*-* *:*:00/15", "*-*-* 10:00:30", "*-1/2-1 00:00:00", "*-*-1/10 05:00:00",
    "Mon *-*-* 00/6:30:00", "*-*-* 00/5:00:00", "*-*-* *:00/25:00", "Tue..Thu *-*-08..14 07:00:00",
    "Sat *-*~07/1 22:00:00", "*-12-25 00:00:00", "*-*-* 23:59:59", "minutely", "weekly",
    "*-*-* 02:30:00 UTC", "*-*-* 12:00:00 Asia/Tokyo",
]

N = 5


def transitions(tz, year):
    z = ZoneInfo(tz)
    out = []
    t = datetime(year, 1, 1, tzinfo=timezone.utc)
    end = datetime(year + 1, 1, 1, tzinfo=timezone.utc)
    prev = t.astimezone(z).utcoffset()
    step = timedelta(minutes=15)
    while t < end:
        t2 = t + step
        off = t2.astimezone(z).utcoffset()
        if off != prev:
            out.append(t2)
            prev = off
        t = t2
    return out


def fmt(t):
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def main():
    print("# tz<TAB>base (UTC)<TAB>iterations<TAB>expression")
    for tz in ZONES:
        bases = []
        for tr in transitions(tz, 2026):
            for d in (-26 * 60, -90, -20, 20, 70):
                bases.append(tr + timedelta(minutes=d))
        if not bases:
            bases = [datetime(2026, 3, 28, 11, tzinfo=timezone.utc)]
        for b in bases:
            for e in DST_EXPRS:
                print(f"{tz}\t{fmt(b)}\t{N}\t{e}")
    for tz in ["Europe/Rome", "UTC"]:
        for b in ["2026-01-30T12:00:00Z", "2027-02-27T23:59:59Z", "2028-02-28T10:00:00Z", "2026-12-31T23:30:00Z"]:
            for e in PLAIN_EXPRS:
                print(f"{tz}\t{b}\t{N}\t{e}")


main()
