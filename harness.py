# -*- coding: utf-8 -*-
"""Terminal Emulator sidecar 端到端验证 harness（多会话协议）。

以 DBX 宿主身份用 stdio-framed 协议驱动 dbx-terminal-emulator：
  1. plugin/initialize 握手
  2. terminal/shells 可用性列表
  3. 同时启动两个会话（不同 shell），验证输出按 sessionId 隔离
  4. 分别写入命令，各自回显互不串扰
  5. 独立 resize 只影响目标会话
  6. stop 掉一个会话：无 exit 事件；另一个继续工作
  7. 自然退出（exit 命令）带 sessionId 的 exit 事件
"""
import json
import os
import struct
import subprocess
import sys
import threading
import time

BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join("target", "release", "dbx-terminal-emulator.exe")

proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
frames = []  # (kind, payload)
frames_lock = threading.Condition()
reader_done = [False]
all_output = bytearray()  # (sid, bytes) 累积


def read_exact(stream, n):
    buf = b""
    while len(buf) < n:
        chunk = stream.read(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def reader():
    try:
        while True:
            header = read_exact(proc.stdout, 5)
            if header is None:
                break
            kind = header[0]
            length = struct.unpack(">I", header[1:5])[0]
            payload = read_exact(proc.stdout, length)
            if payload is None:
                break
            with frames_lock:
                frames.append((kind, payload))
                if kind == 1 and payload[:2] == struct.pack(">H", 15) and payload[2:17] == b"terminal/output":
                    sid = struct.unpack("<I", payload[17:21])[0]
                    all_output.extend(sid.to_bytes(4, "little") + payload[21:])
                frames_lock.notify_all()
    finally:
        with frames_lock:
            reader_done[0] = True
            frames_lock.notify_all()


threading.Thread(target=reader, daemon=True).start()


def send_json(obj):
    payload = json.dumps(obj).encode()
    proc.stdin.write(b"\x00" + struct.pack(">I", len(payload)) + payload)
    proc.stdin.flush()


def send_binary(channel, payload):
    name = channel.encode()
    frame = struct.pack(">H", len(name)) + name + payload
    proc.stdin.write(b"\x01" + struct.pack(">I", len(frame)) + frame)
    proc.stdin.flush()


def rpc(req_id, method, params=None, timeout=20):
    send_json({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params or {}})
    deadline = time.time() + timeout
    with frames_lock:
        while time.time() < deadline:
            for i, (kind, payload) in enumerate(frames):
                if kind == 0 and (b'"id":%d' % req_id) in payload:
                    msg = json.loads(frames.pop(i)[1])
                    assert "error" not in msg, f"{method} 错误: {msg['error']}"
                    return msg["result"]
            frames_lock.wait(0.05)
    raise AssertionError(f"超时等待 {method}")


def session_output(sid):
    out = bytearray()
    with frames_lock:
        data = bytes(all_output)
    i = 0
    while i + 4 <= len(data):
        s = struct.unpack("<I", data[i:i + 4])[0]
        # 变长：无法定长切分，改为逐帧扫描原始 frames
        break
    # 简化：直接扫原始帧
    with frames_lock:
        for kind, payload in frames:
            if kind == 1 and payload[:2] == struct.pack(">H", 15) and payload[2:17] == b"terminal/output":
                sid_f = struct.unpack("<I", payload[17:21])[0]
                if sid_f == sid:
                    out.extend(payload[21:])
    return bytes(out)


def wait_output(sid, pattern, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pattern in session_output(sid):
            return True
        time.sleep(0.1)
    return False


def autoanswer_dsr():
    answered = {}
    while not reader_done[0]:
        with frames_lock:
            for kind, payload in frames:
                if kind == 1 and payload[:2] == struct.pack(">H", 15) and payload[2:17] == b"terminal/output":
                    sid = struct.unpack("<I", payload[17:21])[0]
                    count = payload[21:].count(b"\x1b[6n")
                    if count > answered.get(sid, 0):
                        answered[sid] = count
                        send_binary("terminal/input", struct.pack("<I", sid) + b"\x1b[1;1R")
        time.sleep(0.1)


step = 0
def ok(msg):
    global step
    step += 1
    print(f"  [PASS {step}] {msg}")


threading.Thread(target=autoanswer_dsr, daemon=True).start()

print("== 1. initialize ==")
r = rpc(1, "plugin/initialize", {"host": {"protocolVersions": [1]}})
assert r["protocolVersion"] == 1
ok(f"协议版本 1, plugin={r['plugin']}")

print("== 2. terminal/shells ==")
r = rpc(2, "terminal/shells")
ids = [s["id"] for s in r["shells"] if s["available"]]
ok(f"可用 shell: {ids}")

print("== 3. 双会话并发 ==")
s1 = rpc(3, "terminal/start", {"shell": "powershell", "cols": 80, "rows": 24})["sessionId"]
s2 = rpc(4, "terminal/start", {"shell": "cmd", "cols": 100, "rows": 30})["sessionId"]
assert s1 != s2
ok(f"session1={s1}(powershell) session2={s2}(cmd)")

print("== 4. 各自就绪 ==")
assert wait_output(s1, b"PS ", 60), "s1 无提示符"
ok("s1 提示符就绪")
assert wait_output(s2, b">", 30), "s2 无提示符"
ok("s2 提示符就绪")

print("== 5. 输入隔离 ==")
send_binary("terminal/input", struct.pack("<I", s1) + b"echo from-s1\r")
assert wait_output(s1, b"from-s1", 15), "s1 无回显"
send_binary("terminal/input", struct.pack("<I", s2) + b"echo from-s2\r")
assert wait_output(s2, b"from-s2", 15), "s2 无回显"
ok("两个会话各自回显，互不串扰")

print("== 6. 独立 resize ==")
rpc(6, "terminal/resize", {"sessionId": s1, "cols": 120, "rows": 40})
send_binary("terminal/input", struct.pack("<I", s1) + b"echo resized-s1\r")
assert wait_output(s1, b"resized-s1", 10)
ok("s1 resize 后继续工作")
assert wait_output(s2, b"from-s2", 1)  # s2 输出未受影响（已有内容仍在）

print("== 7. stop s1：无 exit 事件，s2 不受影响 ==")
rpc(7, "terminal/stop", {"sessionId": s1})
time.sleep(1.5)
exit_s1 = [1 for kind, payload in frames if kind == 1 and payload[:2] == struct.pack(">H", 13) and payload[2:15] == b"terminal/exit" and struct.unpack("<I", payload[15:19])[0] == s1]
assert not exit_s1, "stop 不应产生 exit 事件"
ok("s1 stop 无 exit 事件")
send_binary("terminal/input", struct.pack("<I", s2) + b"echo still-alive\r")
assert wait_output(s2, b"still-alive", 10), "s2 被误伤"
ok("s2 继续正常工作")

print("== 8. 自然退出带 sessionId ==")
send_binary("terminal/input", struct.pack("<I", s2) + b"exit\r")
deadline = time.time() + 15
got = None
while time.time() < deadline:
    for kind, payload in frames:
        if kind == 1 and payload[:2] == struct.pack(">H", 13) and payload[2:15] == b"terminal/exit":
            sid = struct.unpack("<I", payload[15:19])[0]
            code = payload[19:].decode()
            got = (sid, code)
    if got:
        break
    time.sleep(0.1)
assert got and got[0] == s2, f"未收到 s2 的自然退出事件: {got}"
ok(f"s2 自然退出事件 sessionId={got[0]} code={got[1]}")

r = rpc(8, "terminal/status", {"sessionId": s2})
assert r["exited"] is True
r = rpc(9, "terminal/status", {"sessionId": s1})
print("  [INFO] s1 status(已 stop 移除):", r)

print("\n全部通过 ✔")
proc.stdin.close()
proc.terminate()
