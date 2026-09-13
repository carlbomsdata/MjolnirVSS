#!/usr/bin/env python3
"""Sends key presses and mouse clicks to a VMware virtual machine over VNC.

Why this exists
---------------
Two of the things MjolnirVSS has to be tested for cannot be tested without
driving a machine that has no VMware Tools in it:

* the recovery application has to be **operable from the keyboard alone**, in
  Windows PE, where nothing can be installed to help;
* an unattended install occasionally stops on a question, and a test run that
  needs somebody to click Yes is not an automated test.

VMware Workstation serves the framebuffer over VNC, and the same connection
carries key and pointer events. This is the smallest client that can send them.

Companion to `vnc_screenshot.py`, which reads the screen. Use them together:
send a key, take a picture, look at what happened.

Usage
-----
    python vnc_input.py --port 5990 --keys "Tab Tab Return"
    python vnc_input.py --port 5990 --type "hello world"
    python vnc_input.py --port 5990 --click 512,384

Key names are X11 keysym names without the prefix, so `Return`, `Tab`, `Escape`,
`Down`, `F10`. A single character is sent as itself. Prefix with `ctrl+`,
`alt+` or `shift+` for a chord.

Copyright (C) the MjolnirVSS contributors.
Licensed under the GNU General Public License, version 3 or later.
"""

from __future__ import annotations

import argparse
import struct
import sys
import time

from vnc_screenshot import VncError, connect

MSG_KEY_EVENT = 4
MSG_POINTER_EVENT = 5

# The X11 keysyms for the keys a test actually presses. Anything not here that
# is a single character is sent as its own code point, which is what the
# protocol says for Latin-1 and ASCII.
KEYSYMS = {
    "BackSpace": 0xFF08,
    "Tab": 0xFF09,
    "Return": 0xFF0D,
    "Enter": 0xFF0D,
    "Escape": 0xFF1B,
    "Esc": 0xFF1B,
    "Space": 0x0020,
    "Delete": 0xFFFF,
    "Home": 0xFF50,
    "Left": 0xFF51,
    "Up": 0xFF52,
    "Right": 0xFF53,
    "Down": 0xFF54,
    "Page_Up": 0xFF55,
    "Page_Down": 0xFF56,
    "End": 0xFF57,
    "Insert": 0xFF63,
    "Menu": 0xFF67,
    "Shift": 0xFFE1,
    "Shift_L": 0xFFE1,
    "Control": 0xFFE3,
    "Control_L": 0xFFE3,
    "Alt": 0xFFE9,
    "Alt_L": 0xFFE9,
    "Super": 0xFFEB,
    "Win": 0xFFEB,
}
for _n in range(1, 13):
    KEYSYMS[f"F{_n}"] = 0xFFBD + _n

MODIFIERS = {
    "ctrl": 0xFFE3,
    "control": 0xFFE3,
    "alt": 0xFFE9,
    "shift": 0xFFE1,
    "win": 0xFFEB,
    "super": 0xFFEB,
}


def keysym_for(name: str) -> int:
    """The keysym for a key name or a single character."""
    if name in KEYSYMS:
        return KEYSYMS[name]
    if len(name) == 1:
        return ord(name)
    # Names are matched case insensitively as a convenience, but only after the
    # exact forms, so a literal "f" is the letter and "F1" is the function key.
    for known, value in KEYSYMS.items():
        if known.lower() == name.lower():
            return value
    raise VncError(f"unknown key name {name!r}")


def send_key(sock, keysym: int, down: bool) -> None:
    sock.sendall(struct.pack(">BBHI", MSG_KEY_EVENT, 1 if down else 0, 0, keysym))


def press(sock, spec: str, hold: float = 0.02) -> None:
    """Presses one key, with any modifiers named before a plus sign.

    A single character is always itself, so typing a literal plus sign works
    and is not read as an empty key with a modifier in front of it.
    """
    if len(spec) == 1:
        parts = [spec]
    else:
        parts = spec.split("+")
    key = parts[-1]
    modifiers = [MODIFIERS[p.lower()] for p in parts[:-1] if p.lower() in MODIFIERS]

    # A character that needs shift on a plain keyboard is sent with shift held,
    # because the guest maps the keysym through its own layout.
    needs_shift = len(key) == 1 and (key.isupper() or key in '!"#$%&()*+:<>?@^_{|}~')
    if needs_shift and MODIFIERS["shift"] not in modifiers:
        modifiers.append(MODIFIERS["shift"])

    for m in modifiers:
        send_key(sock, m, True)
    keysym = keysym_for(key)
    send_key(sock, keysym, True)
    time.sleep(hold)
    send_key(sock, keysym, False)
    for m in reversed(modifiers):
        send_key(sock, m, False)


def type_text(sock, text: str, delay: float = 0.03) -> None:
    """Types a string one character at a time."""
    for char in text:
        if char == "\n":
            press(sock, "Return")
        elif char == "\t":
            press(sock, "Tab")
        elif char == " ":
            press(sock, "Space")
        else:
            press(sock, char)
        time.sleep(delay)


def click(sock, x: int, y: int, button: int = 1) -> None:
    """Moves the pointer and clicks."""
    mask = 1 << (button - 1)
    sock.sendall(struct.pack(">BBHH", MSG_POINTER_EVENT, 0, x, y))
    time.sleep(0.05)
    sock.sendall(struct.pack(">BBHH", MSG_POINTER_EVENT, mask, x, y))
    time.sleep(0.05)
    sock.sendall(struct.pack(">BBHH", MSG_POINTER_EVENT, 0, x, y))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--timeout", type=float, default=20.0)
    parser.add_argument("--keys", help="space separated key presses, e.g. \"Tab Tab Return\"")
    parser.add_argument("--type", dest="text", help="text to type")
    parser.add_argument("--click", help="x,y to click")
    parser.add_argument(
        "--settle",
        type=float,
        default=0.5,
        help="seconds to wait after sending, so the guest can react",
    )
    args = parser.parse_args()

    if not (args.keys or args.text or args.click):
        parser.error("nothing to send: use --keys, --type or --click")

    try:
        sock, width, height = connect(args.host, args.port, args.timeout)
    except (OSError, VncError) as e:
        print(f"could not connect to {args.host}:{args.port}: {e}", file=sys.stderr)
        return 2

    try:
        if args.click:
            x, y = (int(p) for p in args.click.split(","))
            if not (0 <= x < width and 0 <= y < height):
                print(f"{x},{y} is outside the {width}x{height} screen", file=sys.stderr)
                return 2
            click(sock, x, y)
            print(f"clicked {x},{y}")

        if args.keys:
            for spec in args.keys.split():
                press(sock, spec)
                time.sleep(0.08)
            print(f"pressed {args.keys}")

        if args.text:
            type_text(sock, args.text)
            print(f"typed {len(args.text)} characters")

        time.sleep(args.settle)
    except (OSError, VncError) as e:
        print(f"could not send: {e}", file=sys.stderr)
        return 3
    finally:
        sock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
