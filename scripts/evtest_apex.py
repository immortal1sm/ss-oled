#!/usr/bin/env python3
"""Capture events from Apex Pro Consumer Control for 8 seconds.
   Press each media key (play/pause, next, prev) at least once.
"""
import evdev, sys, time
dev = evdev.InputDevice("/dev/input/event259")
print(f"device: {dev.name}")
print(f"vendor: {dev.info.vendor:04x}  product: {dev.info.product:04x}")
print()
print("Listening for events for 8 seconds — press your keys now...")
print()

start = time.time()
events_seen = []
try:
    while time.time() - start < 8:
        try:
            for event in dev.read_loop():
                if event.type == evdev.ecodes.EV_KEY:
                    code = event.code
                    val = event.value  # 1=down, 0=up, 2=repeat
                    name = evdev.ecodes.KEY.get(code, f"code_{code}")
                    elapsed = time.time() - start
                    events_seen.append((elapsed, code, val, name))
                    print(f"  [{elapsed:.2f}s] type=KEY code={code} ({hex(code)}) value={val} -> {name}")
                if time.time() - start >= 8:
                    break
        except BlockingIOError:
            time.sleep(0.05)
except KeyboardInterrupt:
    pass

print()
print(f"Total events captured: {len(events_seen)}")
print()
print("Distinct key codes pressed:")
codes = sorted(set((c, n) for _, c, _, n in events_seen))
for c, n in codes:
    print(f"  {c} ({hex(c)}): {n}")
