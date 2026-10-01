#!/usr/bin/env python3
"""Self-verifying hotkey capture test.

Injects a synthetic Ctrl+Alt+1 through /dev/uinput, captures every keyboard
evdev node, and reports whether the injected combo was observed. This proves
the capture path works before any conclusion is drawn about real keypresses.

Usage:  /usr/bin/python3 scripts/evtest_selftest.py
Then, while it runs, press Ctrl+Alt+1 / 2 / 0 on a real keyboard.
"""
import os
import selectors
import sys
import time

import evdev
from evdev import ecodes, UInput

DURATION = 30
WATCH = {ecodes.KEY_1, ecodes.KEY_2, ecodes.KEY_0}
MODS = {
    ecodes.KEY_LEFTCTRL: "Ctrl", ecodes.KEY_RIGHTCTRL: "Ctrl",
    ecodes.KEY_LEFTALT: "Alt", ecodes.KEY_RIGHTALT: "Alt",
    ecodes.KEY_LEFTSHIFT: "Shift", ecodes.KEY_RIGHTSHIFT: "Shift",
}
MOD_CODES = set(MODS)


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
        if ecodes.EV_KEY not in caps:
            continue
        keys = set(caps.get(ecodes.EV_KEY, []))
        if keys & (WATCH | MOD_CODES):
            out.append((path, dev))
    return out


def main():
    nodes = keyboard_nodes()
    print(f"Monitoring {len(nodes)} keyboard node(s) for {DURATION}s:")
    for p, d in nodes:
        print(f"  {p:22s} {d.name}")

    sel = selectors.DefaultSelector()
    for p, d in nodes:
        sel.register(d.fd, selectors.EVENT_READ, data=(p, d))

    # Virtual keyboard used ONLY to self-test the capture path. It MUST be
    # created BEFORE enumerating nodes, otherwise its /dev/input/eventN node
    # does not exist yet and the injected event is never monitored.
    ui = None
    try:
        ui = UInput(name="ss-oled-selftest",
                    events={ecodes.EV_KEY: list(WATCH | MOD_CODES)})
        print("\nInjected a synthetic Ctrl+Alt+1 at t+3s to verify capture.")
    except Exception as e:
        print(f"  (uinput unavailable: {e})")

    # Register the virtual device so the injected event is actually observed.
    if ui is not None:
        nodes = keyboard_nodes()
        for p, d in nodes:
            if "selftest" in (d.name or ""):
                print(f"  virtual device: {p} {d.name}")
                sel.register(d.fd, selectors.EVENT_READ, data=(p, d))

    start = time.time()
    injected_at = None
    seen = []          # (t, devname, keyspec, modstr, source)
    mod_state = {}

    def synth(combo):
        for c in combo:
            ui.write(ecodes.EV_KEY, c, 1)
        ui.syn()
        for c in reversed(combo):
            ui.write(ecodes.EV_KEY, c, 0)
        ui.syn()

    while time.time() - start < DURATION:
        if ui and injected_at is None and time.time() - start > 3:
            injected_at = time.time() - start
            synth([ecodes.KEY_LEFTCTRL, ecodes.KEY_LEFTALT, ecodes.KEY_1])
            print(f"  [{injected_at:5.2f}s] >>> injected Ctrl+Alt+1 <<<")

        for key, _ in sel.select(timeout=0.05):
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
                elif ev.code in WATCH and ev.value == 1:
                    kc = ecodes.KEY.get(ev.code, str(ev.code))
                    modstr = "+".join(sorted(active))
                    src = "INJECTED" if "selftest" in name else "real"
                    t = time.time() - start
                    seen.append((t, name, kc, modstr, src))
                    print(f"  [{t:5.2f}s] {src:8s} {modstr}+{kc}  on {name}")

    if ui:
        ui.close()

    print(f"\n{'='*64}")
    real = [s for s in seen if s[4] == "real"]
    inj = [s for s in seen if s[4] == "INJECTED"]
    print(f"INJECTED events captured: {len(inj)}")
    print(f"REAL keypresses captured: {len(real)}")

    if inj:
        print("\n=> CAPTURE PATH VERIFIED. Injected Ctrl+Alt+1 was seen.")
        print("   If REAL keypresses were 0, the kernel is NOT receiving your")
        print("   physical Ctrl+Alt+1 - the keyboard or an upstream grabber")
        print("   is swallowing it, and no daemon-side change can fix that.")
    else:
        print("\n=> CAPTURE PATH BROKEN. Even the synthetic event was not seen,")
        print("   so this test cannot judge real keypresses at all.")
    print("=" * 64)
    return 0


if __name__ == "__main__":
    sys.exit(main())
