"""Runs the REAL fogar-gateway locally for fogar-agent's E2E tier, with the gateway's own fake
OpenClaw and no STT/TTS. Run from the gateway checkout (its `tests.fakes` must import):

    uv run --project "$FOGAR_GATEWAY_DIR" --locked python launch.py

Line protocol. stdout: `ready <port>` once the gateway answers, then one line per command.
stdin commands:
  code     -> `code <6 digits>` (opens a pairing code over the admin API)
  revoke   -> `revoked <n>` (deletes every client: live sessions close with 4200)
  restart  -> `restarted` (SIGTERM, i.e. close 1012, then the same port again)
EOF on stdin stops everything.
"""

import asyncio
import os
import secrets
import signal
import socket
import subprocess
import sys
import tempfile

import httpx

from tests.fakes.openclaw import FakeOpenClawServer

ADMIN = secrets.token_hex(16)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def start(env: dict, port: int) -> subprocess.Popen:
    gw = subprocess.Popen(
        [sys.executable, "-m", "uvicorn", "fogar_gateway.main:app", "--host", "127.0.0.1",
         "--port", str(port), "--ws-max-size", "1048576", "--timeout-graceful-shutdown", "5"],
        env=env, stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=sys.stderr,
    )
    async with httpx.AsyncClient() as c:
        for _ in range(1200):  # cold start of the gateway is ~30 s
            if gw.poll() is not None:
                raise SystemExit(f"gateway exited with {gw.returncode}")
            try:
                if (await c.get(f"http://127.0.0.1:{port}/.well-known/fogar-gateway")).status_code == 200:
                    return gw
            except httpx.TransportError:
                pass
            await asyncio.sleep(0.1)
    gw.terminate()
    raise SystemExit("gateway did not come up within 120 s")


def stop(gw: subprocess.Popen) -> None:
    gw.send_signal(signal.SIGTERM)
    try:
        gw.wait(timeout=15)
    except subprocess.TimeoutExpired:
        gw.kill()
        gw.wait()


async def main() -> None:
    token = secrets.token_hex(16)
    with tempfile.TemporaryDirectory(prefix="fogar-gw-e2e-") as home:
        async with FakeOpenClawServer(token=token) as oc:
            env = dict(
                os.environ,
                FOGAR_GATEWAY_HOME=home,
                GATEWAY_NAME="e2e",
                OPENCLAW_HOST="127.0.0.1",
                OPENCLAW_PORT=str(oc.port),
                OPENCLAW_TOKEN=token,
                STT_PROVIDER="off",
                TTS_PROVIDER="off",
                ADMIN_TOKEN=ADMIN,
                SESSION_REVALIDATE_INTERVAL_S="0.5",
            )
            subprocess.run([sys.executable, "-m", "fogar_gateway.cli.__main__", "init"], env=env,
                           stdin=subprocess.DEVNULL, stdout=sys.stderr, check=True)
            port = free_port()
            gw = await start(env, port)
            base = f"http://127.0.0.1:{port}"
            hdr = {"X-Admin-Token": ADMIN}
            print(f"ready {port}", flush=True)
            loop = asyncio.get_running_loop()
            try:
                async with httpx.AsyncClient(base_url=base, headers=hdr) as c:
                    while line := await loop.run_in_executor(None, sys.stdin.readline):
                        cmd = line.strip()
                        if cmd == "code":
                            r = await c.post("/admin/pairing", json={"client_type": "app"})
                            r.raise_for_status()
                            print(f"code {r.json()['code']}", flush=True)
                        elif cmd == "revoke":
                            ids = [x["id"] for x in (await c.get("/admin/clients")).json()]
                            for i in ids:
                                (await c.delete(f"/admin/clients/{i}")).raise_for_status()
                            print(f"revoked {len(ids)}", flush=True)
                        elif cmd == "restart":
                            stop(gw)
                            gw = await start(env, port)
                            print("restarted", flush=True)
                        else:
                            print(f"error unknown command {cmd!r}", flush=True)
            finally:
                stop(gw)


asyncio.run(main())
