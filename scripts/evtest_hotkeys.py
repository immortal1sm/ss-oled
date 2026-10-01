#!/usr/bin/env python3
"""Watch every keyboard-ish evdev node for the ss-oled hotkey combo.

Purpose: prove whether the kernel delivers Ctrl+Alt+1/2/0 at all, before
blaming the kglobalaccel registration path. Pure stdlib + evdev, no sudo
(the /dev/input/event* nodes are crw-rw-rw- on this box).

Usage:  python3 scripts/evtest_hotkeys.py [seconds]
Then press Ctrl+Alt+1, Ctrl+Alt+2 and Ctrl+Alt+0 once each.
"""
import os
import selectors
import sys
import time

try:
    import evdev
    from evdev import ecodes
except ImportError:
    print("ERROR: python-evdev not installed (pip install evdev)", file=sys.stderr)
    sys.exit(1)

DURATION = int(sys.argv[1]) if len(sys.argv) > 1 else 20

# Codes we care about for Ctrl+Alt+1/2/0 (top row, not numpad).
WATCH_CODES = {ecodes.KEY_1, ecodes.KEY_2, ecodes.KEY_0}
# Modifier state keys (left/right variants).
MOD_CODES = {
    ecodes.KEY_LEFTCTRL, ecodes.KEY_RIGHTCTRL,
    ecodes.KEY_LEFTALT, ecodes.KEY_RIGHTALT,
    ecodes.KEY_LEFTSHIFT, ecodes.KEY_RIGHTSHIFT,
}
# Keys that would indicate something is intercepting the combo.
INTRUDERS = {ecodes.KEY_LEFTMETA, ecodes.KEY_RIGHTMETA}

MOD_NAMES = {
    ecodes.KEY_LEFTCTRL: "Ctrl", ecodes.KEY_RIGHTCTRL: "Ctrl",
    ecodes.KEY_LEFTALT: "Alt", ecodes.KEY_RIGHTALT: "Alt",
    ecodes.KEY_LEFTSHIFT: "Shift", ecodes.KEY_RIGHTSHIFT: "Shift",
}


def keyboard_nodes():
    """Every evdev node that reports EV_KEY and is not a mouse/pointer."""
    nodes = []
    for name in sorted(os.listdir("/dev/input")):
        if not name.startswith("event"):
            continue
        path = os.path.join("/dev/input", name)
        try:
            dev = evdev.InputDevice(path)
        except (OSError, evdev.util.Eperm):
            continue
        caps = dev.capabilities(absinfo=False)
        if ecodes.EV_KEY not in caps:
            continue
        # Skip nodes with only relative-axis / button style keys (mice).
        keys = set(caps.get(ecodes.EV_KEY, []))
        if not (keys & WATCH_CODES) and not (keys & MOD_CODES):
            continue
        nodes.append((path, dev))
    return nodes


def main():
    nodes = keyboard_nodes()
    if not nodes:
        print("No readable keyboard evdev nodes found.", file=sys.stderr)
        return 1

    print(f"Monitoring {len(nodes)} keyboard node(s) for {DURATION}s:")
    for path, dev in nodes:
        print(f"  {path:22s} {dev.name}")
    print("\nPress Ctrl+Alt+1, Ctrl+Alt+2, Ctrl+Alt+0 now...\n")

    sel = selectors.DefaultSelector()
    for path, dev in nodes:
        sel.register(dev.fd, selectors.EVENT_READ, data=(path, dev))

    start = time.time()
    hits = []          # (elapsed, devname, keyname, mods)
    mod_state = {}     # devname -> set of active mod names
    intruders = []

    while time.time() - start < DURATION:
        for key, _ in sel.select(timeout=0.2):
            path, dev = key.data
            name = dev.name
            try:
                events = dev.read()
            except OSError:
                continue
            for ev in events:
                if ev.type != ecodes.EV_KEY:
                    continue
                if ev.value == 2:      # auto-repeat, ignore
                    continue
                active = mod_state.setdefault(name, set())
                if ev.code in MOD_CODES:
                    label = MOD_NAMES.get(ev.code)
                    if ev.value == 1:
                        active.add(label)
                    else:
                        active.discard(label)
                elif ev.code in INTRUDERS and ev.value == 1:
                    intruders.append((time.time() - start, name, "Super"))
                elif ev.code in WATCH_CODES and ev.value == 1:
                    kc = ecodes.KEY.get(ev.code, str(ev.code))
                    mods = "+".join(sorted(active))
                    elapsed = time.time() - start
                    hits.append((elapsed, name, kc, mods))
                    print(f"  [{elapsed:5.2f}s] {name}: {mods}+{kc}"
                          if mods else
                          f"  [{elapsed:5.2f}s] {name}: {kc} (NO MODIFIERS)")

    print(f"\n{'='*60}")
    print(f"Capture window: {DURATION}s")
    print(f"Total 1/2/0 keypresses seen: {len(hits)}")
    if hits:
        print("\nBreakdown:")
        for elapsed, name, kc, mods in hits:
            print(f"  {mods}+{kc} on '{name}'")
        with_ctrl_alt = [h for h in hits if "Ctrl" in h[3] and "Alt" in h[3]]
        print(f"\n  with Ctrl+Alt held: {len(with_ctrl_alt)}")
        if with_ctrl_alt:
            print("  => KERNEL PATH IS CLEAN. Ctrl+Alt+1/2/0 reach the")
            print("     kernel, so the fault is upstream (kglobalaccel/KWin).")
        else:
            print("  => Kernel saw the digits but NOT with Ctrl+Alt held.")
            print("     Something is intercepting the combo before the kernel.")
    else:
        print("\n  => NO 1/2/0 keypresses reached the kernel at all.")
        print("     Either you did not press them, or the keyboard/firmware")
        print("     is swallowing the combo. Try pressing plain '1' to sanity-")
        print("check the capture is working at all.")

    if intruders:
        print(f"\nSuper/Superkey presses seen ({len(intruders)}) - a desktop")
        print("environment may be treating this combo as a workspace switch.")
    print("=" * 60)
    return 0


if __name__ == "__main__":
    sys.exit(main())
