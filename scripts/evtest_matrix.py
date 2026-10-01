#!/usr/bin/env python3
"""Capture EVERY key event so one run maps which combos survive to the kernel.

Purpose: ss-oled hotkeys never fire. A self-verifying capture proved the
monitor works and that Ctrl+Alt+1/2/0 physically pressed produce ZERO kernel
events. This script widens the net - it logs every key with its modifier
state - so a single 45s window shows exactly which combos the keyboard
delivers and which it swallows.

Usage:  /usr/bin/python3 scripts/evtest_matrix.py [seconds]

Press each of these once or twice, in this order:
    1  Ctrl+1  Alt+1  Ctrl+Alt+1          (is it Ctrl+Alt specifically?)
    2  Ctrl+Alt+F9  Alt+Shift+J           (unclaimed combos)
    3  Ctrl+Alt+Numpad1                   (numpad vs top row)
    4  F9  Ctrl+F9                        (function keys)
"""
import os
import selectors
import sys
import time
from collections import Counter

import evdev
from evdev import ecodes

DURATION = int(sys.argv[1]) if len(sys.argv) > 1 else 45

MODS = {
    ecodes.KEY_LEFTCTRL: "Ctrl", ecodes.KEY_RIGHTCTRL: "Ctrl",
    ecodes.KEY_LEFTALT: "Alt", ecodes.KEY_RIGHTALT: "Alt",
    ecodes.KEY_LEFTSHIFT: "Shift", ecodes.KEY_RIGHTSHIFT: "Shift",
    ecodes.KEY_LEFTMETA: "Super", ecodes.KEY_RIGHTMETA: "Super",
}
MOD_CODES = set(MODS)

# Combos we specifically want to know about.
TRACKED = {
    "1", "Ctrl+1", "Alt+1", "Ctrl+Alt+1",
    "Ctrl+Alt+F9", "Alt+Shift+J", "Ctrl+Alt+KP_1",
    "F9", "Ctrl+F9",
}


def keyboard_nodes():
    out = []
    for name in sorted(os.listdir("/dev/input")):
        if not name.startswith("event"):
            continue
        path = os.path.join("/dev/input", name)
        try:
            dev = evdev.InputDevice(path)
        except OSError:
            continue
        caps = dev.capabilities(absinfo=False)
        if ecodes.EV_KEY in caps and set(caps.get(ecodes.EV_KEY, [])) & MOD_CODES:
            out.append((path, dev))
    return out


def main():
    nodes = keyboard_nodes()
    print(f"Monitoring {len(nodes)} keyboard node(s) for {DURATION}s:")
    for p, d in nodes:
        print(f"  {p:22s} {d.name}")
    print("\nPress, in order: 1 | Ctrl+1 | Alt+1 | Ctrl+Alt+1 | Ctrl+Alt+F9 |")
    print("Alt+Shift+J | Ctrl+Alt+Numpad1 | F9 | Ctrl+F9   (twice each)\n")

    sel = selectors.DefaultSelector()
    for p, d in nodes:
        sel.register(d.fd, selectors.EVENT_READ, data=(p, d))

    start = time.time()
    mod_state = {}
    combos = Counter()      # "Ctrl+Alt+1" -> count
    devices = Counter()     # devname -> count

    while time.time() - start < DURATION:
        for key, _ in sel.select(timeout=0.1):
            path, dev = key.data
            name = dev.name
            try:
                events = dev.read()
            except OSError:
                continue
            for ev in events:
                if ev.type != ecodes.EV_KEY or ev.value == 2:
                    continue
                active = mod_state.setdefault(name, set())
                if ev.code in MODS:
                    lbl = MODS[ev.code]
                    if ev.value == 1:
                        active.add(lbl)
                    else:
                        active.discard(lbl)
                elif ev.value == 1:
                    kc = ecodes.KEY.get(ev.code, str(ev.code))
                    kc = kc[4:] if kc.startswith("KEY_") else kc
                    combo = "+".join(sorted(active) + [kc])
                    combos[combo] += 1
                    devices[name] += 1
                    mark = " *" if combo in TRACKED else ""
                    print(f"  [{time.time()-start:5.2f}s] {combo:24s} "
                          f"on {name}{mark}")

    print(f"\n{'='*66}")
    print(f"TOTAL keypresses captured: {sum(combos.values())}")
    if not combos:
        print("\n=> NOTHING AT ALL reached the kernel. The monitor itself may")
        print("   be broken (it was verified working earlier, so re-run).")
        print("=" * 66)
        return 1

    print("\nPer-device totals:")
    for d, c in devices.most_common():
        print(f"  {c:5d}  {d}")

    print("\n--- TRACKED COMBOS ---")
    for t in ["1", "Ctrl+1", "Alt+1", "Ctrl+Alt+1", "Ctrl+Alt+F9",
              "Alt+Shift+J", "Ctrl+Alt+KP_1", "F9", "Ctrl+F9"]:
        print(f"  {'OK  ' if combos.get(t) else 'MISS'} {t:20s} "
              f"x{combos.get(t, 0)}")

    print("\n--- ALL COMBOS SEEN ---")
    for c, n in combos.most_common(40):
        print(f"  {n:5d}  {c}")

    ca = combos.get("Ctrl+Alt+1", 0)
    f9 = combos.get("Ctrl+Alt+F9", 0)
    print(f"\n{'='*66}")
    if ca == 0 and f9 > 0:
        print("=> Ctrl+Alt+DIGIT is swallowed, but Ctrl+Alt+F9 works.")
        print("   Something specifically eats Ctrl+Alt+<digit>.")
    elif ca == 0 and f9 == 0:
        print("=> NO Ctrl+Alt combo of any kind reaches the kernel.")
        print("   Ctrl+Alt itself is being intercepted upstream.")
    elif ca > 0:
        print("=> Ctrl+Alt+1 DID reach the kernel this time.")
        print("   The hotkey problem is NOT the keyboard - look upstream.")
    print("=" * 66)
    return 0


if __name__ == "__main__":
    sys.exit(main())
