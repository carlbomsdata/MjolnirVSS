#!/usr/bin/env python3
"""Takes a screenshot of a VMware virtual machine over VNC.

Why this exists
---------------
The test harness drives virtual machines that have no VMware Tools installed:
Windows Setup while it is running, Windows PE while the recovery application is
on screen, and a freshly restored Windows that has never been logged into.
Without Tools there is no `vmrun captureScreen`, and capturing the VMware window
from the host needs an interactive desktop session, which an automated run does
not have.

VMware Workstation can serve the machine's framebuffer over VNC, which needs
neither. This is the smallest client that can ask for one frame: it speaks
enough of RFB 3.8 to connect, ask for the whole screen as raw pixels, and save
it.

It reads. It sends no key presses and no pointer events, so it cannot change
what the machine is doing.

Usage
-----
    python vnc_screenshot.py --port 5990 --out screen.png
    python vnc_screenshot.py --port 5990 --out screen.png --text

`--text` also prints a coarse description of what is on screen, which is enough
to tell a blank screen from a dialog without anybody looking at the picture.

Copyright (C) the MjolnirVSS contributors.
Licensed under the GNU General Public License, version 3 or later.
"""

from __future__ import annotations

import argparse
import socket
import struct
import sys
import time

# --- RFB message numbers, from the protocol specification -------------------
MSG_SET_PIXEL_FORMAT = 0
MSG_SET_ENCODINGS = 2
MSG_FRAMEBUFFER_UPDATE_REQUEST = 3

SERVER_FRAMEBUFFER_UPDATE = 0
SERVER_SET_COLOUR_MAP = 1
SERVER_BELL = 2
SERVER_CUT_TEXT = 3

ENCODING_RAW = 0

SECURITY_NONE = 1


class VncError(RuntimeError):
    """Something the server did that this client cannot carry on from."""


def recv_exactly(sock: socket.socket, count: int) -> bytes:
    """Reads exactly `count` bytes, or fails saying how far it got."""
    chunks = []
    got = 0
    while got < count:
        block = sock.recv(min(65536, count - got))
        if not block:
            raise VncError(f"the connection closed after {got} of {count} bytes")
        chunks.append(block)
        got += len(block)
    return b"".join(chunks)


def connect(host: str, port: int, timeout: float) -> tuple[socket.socket, int, int]:
    """Opens a VNC connection and returns the socket and the screen size."""
    sock = socket.create_connection((host, port), timeout=timeout)
    sock.settimeout(timeout)

    # --- version handshake -------------------------------------------------
    version = recv_exactly(sock, 12)
    if not version.startswith(b"RFB "):
        raise VncError(f"that is not a VNC server; it said {version!r}")
    sock.sendall(b"RFB 003.008\n")

    # --- security handshake ------------------------------------------------
    count = recv_exactly(sock, 1)[0]
    if count == 0:
        # A zero count is followed by a reason string.
        reason_length = struct.unpack(">I", recv_exactly(sock, 4))[0]
        reason = recv_exactly(sock, reason_length).decode("utf-8", "replace")
        raise VncError(f"the server refused the connection: {reason}")

    types = recv_exactly(sock, count)
    if SECURITY_NONE not in types:
        raise VncError(
            "the server wants a password. Remove RemoteDisplay.vnc.key from the "
            "virtual machine, or add password support to this tool."
        )
    sock.sendall(bytes([SECURITY_NONE]))

    result = struct.unpack(">I", recv_exactly(sock, 4))[0]
    if result != 0:
        raise VncError("the server rejected the connection")

    # --- initialisation ----------------------------------------------------
    # A shared flag of 1 means "do not disconnect anybody else", which matters
    # because somebody may have the machine's window open.
    sock.sendall(b"\x01")
    header = recv_exactly(sock, 24)
    width, height = struct.unpack(">HH", header[:4])
    name_length = struct.unpack(">I", header[20:24])[0]
    recv_exactly(sock, name_length)

    # Ask for a pixel format this tool can read without conversion tables:
    # 32 bits a pixel, true colour, red in the top byte of the low three.
    pixel_format = struct.pack(
        ">BBBBHHHBBBxxx",
        32,  # bits per pixel
        24,  # depth
        0,  # big endian flag
        1,  # true colour flag
        255,  # red max
        255,  # green max
        255,  # blue max
        16,  # red shift
        8,  # green shift
        0,  # blue shift
    )
    sock.sendall(struct.pack(">Bxxx", MSG_SET_PIXEL_FORMAT) + pixel_format)

    # Raw only. Every server supports it, and a screenshot is taken rarely
    # enough that the bandwidth does not matter.
    sock.sendall(struct.pack(">BxHi", MSG_SET_ENCODINGS, 1, ENCODING_RAW))

    return sock, width, height


def request_frame(sock: socket.socket, width: int, height: int, incremental: int = 0) -> None:
    sock.sendall(
        struct.pack(
            ">BBHHHH", MSG_FRAMEBUFFER_UPDATE_REQUEST, incremental, 0, 0, width, height
        )
    )


def read_frame(sock: socket.socket, width: int, height: int, deadline: float) -> bytearray:
    """Collects rectangles until the whole screen has been filled once."""
    frame = bytearray(width * height * 4)
    covered = 0
    wanted = width * height

    while covered < wanted:
        if time.monotonic() > deadline:
            raise VncError("the server did not send a whole screen in time")

        message = recv_exactly(sock, 1)[0]
        if message == SERVER_BELL:
            continue
        if message == SERVER_CUT_TEXT:
            recv_exactly(sock, 3)
            length = struct.unpack(">I", recv_exactly(sock, 4))[0]
            recv_exactly(sock, length)
            continue
        if message == SERVER_SET_COLOUR_MAP:
            recv_exactly(sock, 3)
            _, count = struct.unpack(">HH", recv_exactly(sock, 4))
            recv_exactly(sock, count * 6)
            continue
        if message != SERVER_FRAMEBUFFER_UPDATE:
            raise VncError(f"the server sent message type {message}, which this tool does not read")

        recv_exactly(sock, 1)
        rectangles = struct.unpack(">H", recv_exactly(sock, 2))[0]
        for _ in range(rectangles):
            x, y, w, h, encoding = struct.unpack(">HHHHi", recv_exactly(sock, 12))
            if encoding != ENCODING_RAW:
                raise VncError(f"the server used encoding {encoding}, and this tool only reads raw")
            if w == 0 or h == 0:
                continue
            pixels = recv_exactly(sock, w * h * 4)
            for row in range(h):
                target = ((y + row) * width + x) * 4
                source = row * w * 4
                frame[target : target + w * 4] = pixels[source : source + w * 4]
            covered += w * h

    return frame


def describe(frame: bytearray, width: int, height: int) -> str:
    """A coarse description of the picture, for a log rather than an eye.

    Enough to tell a black screen from a blue one from a busy one, which is
    usually all a test needs in order to say whether something is on screen.
    """
    total = width * height
    step = max(1, total // 20000)

    counts: dict[tuple[int, int, int], int] = {}
    samples = 0
    for index in range(0, total, step):
        at = index * 4
        blue, green, red = frame[at], frame[at + 1], frame[at + 2]
        # Quantised, so near identical shades count as one colour.
        key = (red // 32, green // 32, blue // 32)
        counts[key] = counts.get(key, 0) + 1
        samples += 1

    ranked = sorted(counts.items(), key=lambda kv: kv[1], reverse=True)[:4]
    parts = []
    for (red, green, blue), count in ranked:
        share = count / samples * 100
        parts.append(f"#{red * 32:02x}{green * 32:02x}{blue * 32:02x} {share:.0f}%")

    distinct = len(counts)
    busy = "blank" if distinct <= 3 else ("simple" if distinct <= 40 else "busy")
    return f"{width}x{height}, {busy}, {distinct} colours; " + ", ".join(parts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--timeout", type=float, default=20.0)
    parser.add_argument("--text", action="store_true", help="also describe the picture")
    args = parser.parse_args()

    try:
        sock, width, height = connect(args.host, args.port, args.timeout)
    except (OSError, VncError) as e:
        print(f"could not connect to {args.host}:{args.port}: {e}", file=sys.stderr)
        return 2

    try:
        request_frame(sock, width, height)
        frame = read_frame(sock, width, height, time.monotonic() + args.timeout)
    except (OSError, VncError) as e:
        print(f"could not read the screen: {e}", file=sys.stderr)
        return 3
    finally:
        sock.close()

    from PIL import Image

    image = Image.frombytes("RGBA", (width, height), bytes(frame), "raw", "BGRA")
    image.convert("RGB").save(args.out)
    print(f"saved {args.out} ({width}x{height})")

    if args.text:
        print(describe(frame, width, height))
    return 0


if __name__ == "__main__":
    sys.exit(main())
