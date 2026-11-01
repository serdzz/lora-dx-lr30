#!/usr/bin/env python3
"""Bridges node_b's GPIO relays/sensors to MQTT, driving node_a's IO
bridge mode over its USB-serial (CH340) port — same framing/bridge
pattern as ../ota_push.py, but for ../firmware/src/io.rs's relay/sensor
protocol instead of firmware chunks, and running continuously (not a
one-shot push) since relay commands and sensor queries can arrive at
any time from Home Assistant / mosquitto.

MQTT topics (controller -> node_a -> LoRa -> node_b):
    lora/node_b/relay/<0|1>/set     payload "ON"/"OFF"/"1"/"0"  -> SetRelay
    lora/node_b/sensor/poll         any payload                -> SensorQuery

MQTT topics (node_b -> LoRa -> node_a -> controller):
    lora/node_b/relay/<0|1>/state   "ON"/"OFF"          (retained)
    lora/node_b/sensor/<0|1>/state  "ON"/"OFF"          (retained)
    lora/node_b/io/log              free-form bridge/node_a log lines

Usage:
    python3 io_mqtt_bridge.py /dev/cu.wchusbserial1420 \\
        --mqtt-host 127.0.0.1 --device-id 0

Requires `pyserial` and `paho-mqtt` (``pip install pyserial paho-mqtt``).

# Protocol (see ../firmware/src/host_log.rs's module doc comment)
Two framing shapes share node_a's one UART, told apart by their first
byte:
  - Plain text lines (`IO\\n`, `EXIT\\n`) — mode switches, same convention
    as `OTA\\n`/`EXIT\\n` in ota_push.py.
  - Binary frames `[0xA6][len: u8][len bytes of io::Packet-encoded data]`
    in BOTH directions: this script sends one per SetRelay/SensorQuery,
    node_a relays back one per RelayAck/SensorReport it hears over LoRa.
"""

import argparse
import struct
import sys
import threading
import time

try:
    import serial
except ImportError:
    sys.exit("pyserial not installed — run: pip3 install pyserial")

try:
    import paho.mqtt.client as mqtt
except ImportError:
    sys.exit("paho-mqtt not installed — run: pip3 install paho-mqtt")

# Must match firmware/src/io.rs exactly.
IO_MAGIC = 0x3C
IO_VERSION = 1

KIND_SET_RELAY = 0
KIND_RELAY_ACK = 1
KIND_SENSOR_QUERY = 2
KIND_SENSOR_REPORT = 3

RELAY_COUNT = 2
SENSOR_COUNT = 2


def encode_set_relay(target: int, relay_index: int, state: bool) -> bytes:
    return struct.pack("<BBBBBB", IO_MAGIC, IO_VERSION, KIND_SET_RELAY, target, relay_index, int(state))


def encode_sensor_query(target: int) -> bytes:
    return struct.pack("<BBBB", IO_MAGIC, IO_VERSION, KIND_SENSOR_QUERY, target)


def decode(frame: bytes):
    if len(frame) < 4 or frame[0] != IO_MAGIC or frame[1] != IO_VERSION:
        return None
    kind = frame[2]
    if kind == KIND_RELAY_ACK and len(frame) >= 7:
        return ("relay_ack", frame[4], bool(frame[5]), bool(frame[6]))  # relay_index, state, ok
    if kind == KIND_SENSOR_REPORT and len(frame) >= 5:
        return ("sensor_report", frame[4])  # digital_mask
    return None


class NodeABridge:
    """Talks to node_a's IO bridge mode over its USB-serial port — same
    byte-paced-write / framed-read pattern as ota_push.py's NodeABridge,
    just for 0xA6 io frames instead of 0xA5 ota frames."""

    def __init__(self, port: str, baud: int = 115200):
        self.ser = serial.Serial(port, baud, timeout=0.2)
        self._lock = threading.Lock()

    def write_paced(self, data: bytes, gap_s: float = 0.002):
        with self._lock:
            for c in data:
                self.ser.write(bytes([c]))
                self.ser.flush()
                time.sleep(gap_s)

    def enter_io_mode(self, timeout_s: float = 300.0, log_cb=None):
        self.ser.reset_input_buffer()
        self.write_paced(b"\nIO\n")
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
            if line and log_cb:
                log_cb(line)
            if "IO bridge mode" in line and "exited" not in line:
                return
        raise TimeoutError("node_a never entered IO bridge mode")

    def exit_io_mode(self):
        self.write_paced(b"EXIT\n")
        time.sleep(0.2)

    def send_frame(self, payload: bytes):
        if len(payload) > 255:
            raise ValueError("io frame too long for the 1-byte length prefix")
        self.write_paced(bytes([0xA6, len(payload)]) + payload)

    def read_frame(self, timeout_s: float, log_cb=None):
        """Reads one `[0xA6][len][data]` frame off the UART, or None on
        timeout — interleaved text log lines are drained via `log_cb`
        rather than confusing the frame parser (same split ota_push.py's
        read_frame does for 0xA5)."""
        deadline = time.monotonic() + timeout_s
        buf = bytearray()
        while time.monotonic() < deadline:
            b = self.ser.read(1)
            if not b:
                continue
            if not buf and b[0] == 0xA6:
                length_byte = self.ser.read(1)
                if not length_byte:
                    continue
                length = length_byte[0]
                data = self.ser.read(length)
                if len(data) < length:
                    continue
                return data
            elif b[0] == ord("\n"):
                if buf and log_cb:
                    log_cb(bytes(buf).decode(errors="replace").rstrip())
                buf.clear()
            else:
                buf.extend(b)
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("port", help="node_a's serial port, e.g. /dev/cu.wchusbserial1420")
    ap.add_argument("--device-id", type=int, default=0, help="node_b's DEVICE_ID (target field)")
    ap.add_argument("--mqtt-host", default="127.0.0.1")
    ap.add_argument("--mqtt-port", type=int, default=1883)
    ap.add_argument("--poll-interval", type=float, default=10.0,
                     help="seconds between automatic SensorQuery polls (0 = disable)")
    args = ap.parse_args()

    mqttc = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="lora-io-bridge")

    def mqtt_log(msg: str):
        ts = time.strftime("%H:%M:%S")
        print(f"[{ts}] {msg}")
        mqttc.publish("lora/node_b/io/log", f"[{ts}] {msg}", qos=0, retain=False)

    bridge = NodeABridge(args.port)
    print("Waiting for node_a to enter IO bridge mode (finishes current ping/pong round first) ...")
    bridge.enter_io_mode(log_cb=mqtt_log)
    print("node_a is in IO bridge mode.")

    pending_relay_cmds: "list[tuple[int, bool]]" = []
    cmds_lock = threading.Lock()

    def on_connect(client, userdata, flags, reason_code, properties=None):
        client.subscribe("lora/node_b/relay/+/set")
        client.subscribe("lora/node_b/sensor/poll")
        mqtt_log(f"MQTT connected, subscribed (reason_code={reason_code})")

    def on_message(client, userdata, msg):
        topic = msg.topic
        payload = msg.payload.decode(errors="replace").strip().upper()
        if topic.startswith("lora/node_b/relay/") and topic.endswith("/set"):
            try:
                relay_index = int(topic.split("/")[3])
            except (IndexError, ValueError):
                return
            state = payload in ("ON", "1", "TRUE")
            with cmds_lock:
                pending_relay_cmds.append((relay_index, state))
        elif topic == "lora/node_b/sensor/poll":
            with cmds_lock:
                pending_relay_cmds.append((None, None))  # sentinel: trigger a sensor poll

    mqttc.on_connect = on_connect
    mqttc.on_message = on_message
    mqttc.connect(args.mqtt_host, args.mqtt_port, keepalive=30)
    mqttc.loop_start()

    last_poll = 0.0
    try:
        while True:
            with cmds_lock:
                cmds = pending_relay_cmds[:]
                pending_relay_cmds.clear()

            for relay_index, state in cmds:
                if relay_index is None:
                    _do_sensor_query(bridge, args.device_id, mqttc, mqtt_log)
                else:
                    _do_set_relay(bridge, args.device_id, relay_index, state, mqttc, mqtt_log)

            if args.poll_interval > 0 and time.monotonic() - last_poll >= args.poll_interval:
                _do_sensor_query(bridge, args.device_id, mqttc, mqtt_log)
                last_poll = time.monotonic()

            time.sleep(0.2)
    except KeyboardInterrupt:
        pass
    finally:
        bridge.exit_io_mode()
        mqttc.loop_stop()
        mqttc.disconnect()


def _do_set_relay(bridge, target, relay_index, state, mqttc, log_cb):
    log_cb(f"-> SetRelay relay={relay_index} state={state}")
    bridge.send_frame(encode_set_relay(target, relay_index, state))
    reply = bridge.read_frame(timeout_s=5.0, log_cb=log_cb)
    decoded = decode(reply) if reply else None
    if decoded and decoded[0] == "relay_ack":
        _, got_index, got_state, ok = decoded
        if ok:
            mqttc.publish(
                f"lora/node_b/relay/{got_index}/state",
                "ON" if got_state else "OFF",
                qos=0,
                retain=True,
            )
            log_cb(f"<- RelayAck relay={got_index} state={got_state} ok={ok}")
        else:
            log_cb(f"<- RelayAck relay={got_index} REJECTED (out-of-range on node_b)")
    else:
        log_cb(f"SetRelay: no/bad reply (got {decoded!r})")


def _do_sensor_query(bridge, target, mqttc, log_cb):
    bridge.send_frame(encode_sensor_query(target))
    reply = bridge.read_frame(timeout_s=5.0, log_cb=log_cb)
    decoded = decode(reply) if reply else None
    if decoded and decoded[0] == "sensor_report":
        mask = decoded[1]
        for i in range(SENSOR_COUNT):
            state = "ON" if (mask & (1 << i)) else "OFF"
            mqttc.publish(f"lora/node_b/sensor/{i}/state", state, qos=0, retain=True)
        log_cb(f"<- SensorReport mask={mask:02x}")
    else:
        log_cb(f"SensorQuery: no/bad reply (got {decoded!r}) — node_b out of range?")


if __name__ == "__main__":
    main()
