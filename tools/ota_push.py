#!/usr/bin/env python3
"""Pushes a firmware image to a `node_b_app`/`repeater` device over node_a's
USB-serial (CH340) bridge, driving node_a's OTA bridge mode.

See ../firmware/README.md "Pushing an OTA update" for the full field
procedure (flashing bootloader + node_b_app first, wiring, etc.) — this
script only covers the actual over-the-air push once that's done.

Usage:
    python3 ota_push.py /dev/cu.wchusbserial1420 --target 2 node_b_app.bin

Requires `pyserial` (``pip install pyserial`` / ``pip3 install pyserial``).

# Protocol (see firmware/src/host_log.rs's module doc comment)
Two framing shapes share node_a's one UART:
  - Plain text lines (`OTA\\n`, `EXIT\\n`) — mode switches.
  - Binary frames `[0xA5][len: u8][len bytes of ota::Packet-encoded data]`
    in BOTH directions: this script sends one per Announce/Chunk/Done, and
    node_a relays back one per Accept/ChunkAck/DoneAck it hears over LoRa.

This script owns the actual OTA state machine (unlike `repeater.rs`'s own
firmware, which only ever RECEIVES OTA — nothing in this repo send*s* OTA
from firmware itself, deliberately: it's much easier to get retry/timeout/
progress-reporting logic right in Python than to fit it in a few KB of
no_std flash budget on node_a).
"""

import argparse
import struct
import sys
import time
import zlib

try:
    import serial
except ImportError:
    sys.exit("pyserial not installed — run: pip3 install pyserial")

# Must match firmware/src/ota.rs exactly.
OTA_MAGIC = 0x5A
OTA_VERSION = 1
CHUNK_DATA_LEN = 32

KIND_ANNOUNCE = 0
KIND_ACCEPT = 1
KIND_CHUNK = 2
KIND_CHUNK_ACK = 3
KIND_DONE = 4
KIND_DONE_ACK = 5

BROADCAST = 0xFF


def encode_announce(updater: int, image_len: int, image_crc: int) -> bytes:
    return struct.pack("<BBBBII", OTA_MAGIC, OTA_VERSION, KIND_ANNOUNCE, updater, image_len, image_crc)


def encode_chunk(target: int, index: int, total: int, data: bytes) -> bytes:
    return struct.pack("<BBBBHH", OTA_MAGIC, OTA_VERSION, KIND_CHUNK, target, index, total) + data


def encode_done(target: int, final_crc: int) -> bytes:
    return struct.pack("<BBBBI", OTA_MAGIC, OTA_VERSION, KIND_DONE, target, final_crc)


def decode(frame: bytes):
    if len(frame) < 4 or frame[0] != OTA_MAGIC or frame[1] != OTA_VERSION:
        return None
    kind = frame[2]
    if kind == KIND_ACCEPT and len(frame) >= 4:
        return ("accept", frame[3])
    if kind == KIND_CHUNK_ACK and len(frame) >= 7:
        index = struct.unpack_from("<H", frame, 4)[0]
        ok = frame[6] != 0
        return ("chunk_ack", index, ok)
    if kind == KIND_DONE_ACK and len(frame) >= 5:
        return ("done_ack", frame[4] != 0)
    return None


class NodeABridge:
    """Talks to node_a's OTA bridge mode over its USB-serial port."""

    def __init__(self, port: str, baud: int = 115200):
        self.ser = serial.Serial(port, baud, timeout=0.2)

    def write_paced(self, data: bytes, gap_s: float = 0.002):
        """node_a reads its UART RX one byte per DMA transfer, so back-to-back
        bytes overrun and get dropped — "OTA\\n" sent in one write() was never
        recognised. Pace bytes so each read is re-armed in time."""
        for c in data:
            self.ser.write(bytes([c]))
            self.ser.flush()
            time.sleep(gap_s)

    def enter_ota_mode(self, timeout_s: float = 300.0):
        """node_a only polls for host commands between sweep rounds (20 pings
        at SF12 can take minutes), so wait for its "OTA bridge mode" log line
        before sending any frame — otherwise the Announce times out host-side
        while node_a is still mid-round. Re-sends "OTA" after every round
        summary in case the previous one was lost on the wire."""
        self.ser.reset_input_buffer()
        self.write_paced(b"\nOTA\n")
        deadline = time.monotonic() + timeout_s
        buf = bytearray()
        while time.monotonic() < deadline:
            b = self.ser.read(1)
            if not b:
                continue
            if b[0] != ord("\n"):
                buf.extend(b)
                continue
            line = bytes(buf).decode(errors="replace").rstrip()
            buf.clear()
            if line:
                print("  [node_a log]", line)
            if "OTA bridge mode" in line and "exited" not in line:
                return
            if "summary" in line:
                self.write_paced(b"OTA\n")
        raise TimeoutError("node_a never entered OTA bridge mode")

    def exit_ota_mode(self):
        self.write_paced(b"EXIT\n")
        time.sleep(0.2)

    def send_frame(self, payload: bytes):
        if len(payload) > 255:
            raise ValueError("OTA frame too long for the 1-byte length prefix")
        self.write_paced(bytes([0xA5, len(payload)]) + payload)

    def read_frame(self, timeout_s: float):
        """Reads one `[0xA5][len][data]` frame off the UART, or None on
        timeout. Interleaved host_log! text lines are drained and printed
        (prefixed) rather than confusing the frame parser — node_a's own
        UART carries both text logs and binary frames on the same wire
        (see host_log.rs), told apart the same way firmware tells them
        apart: 0xA5 as the first byte of a frame vs. anything else being
        plain text up to the next newline.
        """
        deadline = time.monotonic() + timeout_s
        buf = bytearray()
        while time.monotonic() < deadline:
            b = self.ser.read(1)
            if not b:
                continue
            if not buf and b[0] == 0xA5:
                buf.extend(b)
                length_byte = self.ser.read(1)
                if not length_byte:
                    buf.clear()
                    continue
                length = length_byte[0]
                data = self.ser.read(length)  # len counts payload bytes only
                if len(data) < length:
                    buf.clear()
                    continue
                return data
            elif b[0] == ord("\n"):
                if buf:
                    print("  [node_a log]", bytes(buf).decode(errors="replace").rstrip())
                buf.clear()
            else:
                buf.extend(b)
        return None


def push(port: str, target: int, image_path: str, retries: int = 5):
    with open(image_path, "rb") as f:
        image = f.read()
    image_crc = zlib.crc32(image) & 0xFFFFFFFF
    total_chunks = (len(image) + CHUNK_DATA_LEN - 1) // CHUNK_DATA_LEN
    print(f"Image: {len(image)} bytes, crc32={image_crc:08x}, {total_chunks} chunks")

    bridge = NodeABridge(port)
    try:
        print("Waiting for node_a to enter OTA bridge mode (finishes current round first) ...")
        bridge.enter_ota_mode()
        print(f"Announcing to target={target} ...")
        bridge.send_frame(encode_announce(updater=0, image_len=len(image), image_crc=image_crc))
        reply = bridge.read_frame(timeout_s=10.0)
        decoded = decode(reply) if reply else None
        if not decoded or decoded[0] != "accept":
            sys.exit(f"No Accept from target (got {decoded!r}) — check target is powered, "
                      f"DEVICE_ID matches, and in range.")
        print(f"Target {decoded[1]} accepted.")

        for index in range(total_chunks):
            start = index * CHUNK_DATA_LEN
            data = image[start:start + CHUNK_DATA_LEN]
            for attempt in range(retries):
                bridge.send_frame(encode_chunk(target, index, total_chunks, data))
                reply = bridge.read_frame(timeout_s=5.0)
                decoded = decode(reply) if reply else None
                if decoded and decoded[0] == "chunk_ack" and decoded[1] == index and decoded[2]:
                    break
                print(f"  chunk {index}/{total_chunks}: retry {attempt + 1}/{retries} "
                      f"(got {decoded!r})")
            else:
                sys.exit(f"Chunk {index} failed after {retries} retries — aborting.")
            if index % 10 == 0 or index == total_chunks - 1:
                print(f"  chunk {index + 1}/{total_chunks} acked")

        print("Sending Done ...")
        bridge.send_frame(encode_done(target, image_crc))
        reply = bridge.read_frame(timeout_s=10.0)
        decoded = decode(reply) if reply else None
        if decoded and decoded[0] == "done_ack" and decoded[1]:
            print("Target confirmed CRC match — it will swap and reboot now.")
        else:
            print(f"WARNING: no clean DoneAck (got {decoded!r}) — target may not have applied the update.")
    finally:
        bridge.exit_ota_mode()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("port", help="node_a's serial port, e.g. /dev/cu.wchusbserial1420")
    ap.add_argument("image", help="path to the new node_b_app/repeater .bin image")
    ap.add_argument("--target", type=int, required=True, help="target device's DEVICE_ID")
    ap.add_argument("--retries", type=int, default=5)
    args = ap.parse_args()
    push(args.port, args.target, args.image, args.retries)


if __name__ == "__main__":
    main()
