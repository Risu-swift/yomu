#!/usr/bin/env python3
"""Emit a test image over the kitty graphics protocol.

Dependency-free: the PNG is generated with zlib and struct, so this runs on a
bare python3 with nothing installed. Use it to find out whether a terminal
really passes inline image escapes through, independently of any application.

    python3 kitty-test.py            # kitty protocol
    python3 kitty-test.py --raw      # dump the escape bytes instead, for
                                     # checking what a pty layer strips

If a colour gradient with diagonal stripes appears, the protocol works.
If you see stray text like `_Gm=1;iVBOR...` the escapes are being mangled.
"""

import base64
import struct
import sys
import zlib

WIDTH, HEIGHT = 320, 180
CHUNK = 4096


def make_png(width: int, height: int) -> bytes:
    """A gradient with diagonal stripes: obvious when it renders, and obvious
    when it renders at the wrong aspect ratio."""
    rows = bytearray()
    for y in range(height):
        rows.append(0)  # PNG filter type: none
        for x in range(width):
            stripe = 40 if (x + y) % 32 < 4 else 0
            r = min(255, x * 255 // width + stripe)
            g = min(255, y * 255 // height + stripe)
            b = min(255, 200 - (x * 120 // width) + stripe)
            rows.extend((r, g, b))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
        + chunk(b"IEND", b"")
    )


def kitty_escapes(png: bytes) -> bytes:
    """Wrap PNG data in kitty APC sequences, split into protocol-sized chunks.

    a=T  transmit and display immediately
    f=100  the payload is a PNG
    m=1  more chunks follow; m=0 marks the last one
    """
    payload = base64.standard_b64encode(png)
    parts = [payload[i : i + CHUNK] for i in range(0, len(payload), CHUNK)]

    out = bytearray()
    for i, part in enumerate(parts):
        last = i == len(parts) - 1
        control = b"a=T,f=100,m=" + (b"0" if last else b"1") if i == 0 else b"m=" + (b"0" if last else b"1")
        out += b"\x1b_G" + control + b";" + part + b"\x1b\\"
    return bytes(out)


def main() -> int:
    png = make_png(WIDTH, HEIGHT)
    escapes = kitty_escapes(png)

    if "--save" in sys.argv:
        path = sys.argv[sys.argv.index("--save") + 1]
        with open(path, "wb") as fh:
            fh.write(png)
        sys.stdout.write(f"wrote {len(png)} bytes to {path}\n")
        return 0

    if "--raw" in sys.argv:
        chunks = escapes.count(b"\x1b_G")
        sys.stdout.write(
            f"png: {len(png)} bytes, escape stream: {len(escapes)} bytes, "
            f"{chunks} chunk(s)\n"
        )
        sys.stdout.write(repr(escapes[:120]) + " ...\n")
        return 0

    sys.stdout.write(f"kitty graphics test — {WIDTH}x{HEIGHT} PNG, {len(escapes)} bytes of escapes\n")
    sys.stdout.flush()
    sys.stdout.buffer.write(escapes)
    sys.stdout.buffer.flush()
    sys.stdout.write("\n\nIf you see a colour gradient above, inline images work here.\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
