"""Minimal CDP client (stdlib only) for Steam's SharedJSContext.

Run on the handheld while Steam is running (it listens on 127.0.0.1:8080):

    python3 steam-cdp.py < script.js            # evaluate JavaScript
    python3 steam-cdp.py add-shortcut NAME EXE [START_DIR]
"""
import base64, json, os, socket, struct, sys, urllib.request

def target():
    for t in json.load(urllib.request.urlopen("http://127.0.0.1:8080/json", timeout=3)):
        if t["title"] == "SharedJSContext":
            return t["webSocketDebuggerUrl"]
    raise SystemExit("SharedJSContext not found")

def ws_connect(url):
    path = url.split("127.0.0.1:8080", 1)[1]
    s = socket.create_connection(("127.0.0.1", 8080), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:8080\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
              f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
    resp = b""
    while b"\r\n\r\n" not in resp:
        resp += s.recv(1)
    assert b" 101 " in resp.split(b"\r\n")[0], resp
    return s

def send(s, text):
    data = text.encode(); mask = os.urandom(4); n = len(data)
    hdr = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n) if n < 65536 else bytes([0x80 | 127]) + struct.pack(">Q", n))
    s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

def recv_exact(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c: raise EOFError
        b += c
    return b

def recv(s):
    h = recv_exact(s, 2); n = h[1] & 0x7f
    if n == 126: n = struct.unpack(">H", recv_exact(s, 2))[0]
    elif n == 127: n = struct.unpack(">Q", recv_exact(s, 8))[0]
    return recv_exact(s, n).decode()

def evaluate(js):
    s = ws_connect(target())
    send(s, json.dumps({"id": 1, "method": "Runtime.evaluate", "params": {"expression": js, "awaitPromise": True, "returnByValue": True}}))
    while True:
        m = json.loads(recv(s))
        if m.get("id") == 1:
            return m["result"]

def add_shortcut(name, exe, start_dir):
    q = lambda p: json.dumps('"' + p + '"')  # Steam stores paths quoted
    js = f"""(async () => {{
      const id = await SteamClient.Apps.AddShortcut({json.dumps(name)}, {q(exe)}, {q(start_dir)}, "");
      // set again: AddShortcut sometimes double-quotes the path
      SteamClient.Apps.SetShortcutName(id, {json.dumps(name)});
      SteamClient.Apps.SetShortcutExe(id, {q(exe)});
      SteamClient.Apps.SetShortcutStartDir(id, {q(start_dir)});
      // render at the panel's native size; Steam's default is 1280x720, upscaled and blurry
      SteamClient.Apps.SetAppResolutionOverride(id, "Native");
      return id;
    }})()"""
    return evaluate(js)

if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "add-shortcut":
        start = sys.argv[4] if len(sys.argv) > 4 else os.path.dirname(sys.argv[3])
        print(json.dumps(add_shortcut(sys.argv[2], sys.argv[3], start), ensure_ascii=False))
    else:
        print(json.dumps(evaluate(sys.stdin.read()), ensure_ascii=False))
