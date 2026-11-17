"""Exercise the installed Desktop app-server over stdio, without credentials/model turns.
Usage: python tests/desktop_compat.py --codex-exe <Desktop runtime> --gateway-exe <built gateway>
All configuration, SQLite indexes and synthetic history live under .codex/desktop-tests.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import threading
import time
import uuid


class AppServer:
    def __init__(self, exe, env, cwd, provider=None):
        self.queue = queue.Queue()
        self.process = subprocess.Popen(
            [str(exe), "app-server"] + (["-c", f'model_provider="{provider}"'] if provider else []), env=env, cwd=cwd, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8",
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        threading.Thread(target=lambda: [self.queue.put(line) for line in self.process.stdout], daemon=True).start()
        threading.Thread(target=lambda: [None for _ in self.process.stderr], daemon=True).start()
        self.sequence = 0
        self.call("initialize", {"clientInfo": {"name": "gateway-regression", "version": "0.4.0"},
                                 "capabilities": {"experimentalApi": True}})
        self.process.stdin.write('{"method":"initialized"}\n')
        self.process.stdin.flush()

    def call(self, method, params):
        self.sequence += 1
        self.process.stdin.write(json.dumps({"id": self.sequence, "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            message = json.loads(self.queue.get(timeout=max(.1, deadline - time.monotonic())))
            if message.get("id") == self.sequence:
                assert "error" not in message, (method, message.get("error"))
                return message["result"]
        raise TimeoutError(method)

    def close(self):
        self.process.terminate()
        self.process.wait(timeout=5)


def run_case(args, identity):
    root = Path(".codex/desktop-tests") / uuid.uuid4().hex
    root = root.resolve()
    home, gateway = root / "codex", root / "gateway"
    home.mkdir(parents=True)
    gateway.mkdir()
    env = os.environ.copy()
    env.update(CODEX_HOME=str(home), CODEX_GATEWAY_HOME=str(gateway))
    cli, exe = args.codex_exe.resolve(), args.gateway_exe.resolve()
    version = subprocess.check_output([str(cli), "--version"], env=env, text=True).strip()
    print("Runtime:", cli, version, flush=True)
    (home / "config.toml").write_text('model="gpt-6.1-sol"\nmodel_provider="openai"\n', encoding="utf-8")
    # Model discovery only returns 5.6: reproduce the cache-present failure, not empty-cache fallback.
    narrow = {"models": [{"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "description": "Fixture",
                          "visibility": "list", "priority": 1, "supported_in_api": True,
                          "base_instructions": "Fixture instructions.", "supports_reasoning_summaries": False,
                          "default_reasoning_summary": "none", "default_reasoning_level": "high",
                          "supported_reasoning_levels": [
                              {"effort": effort, "description": effort}
                              for effort in ["low", "medium", "high", "xhigh", "max", "ultra"]]}]}
    (home / "models_cache.json").write_text(json.dumps(narrow), encoding="utf-8")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    (gateway / "config.toml").write_text(
        f'deepseek_api_key="sk-fixture-never-used"\nport={port}\nauto_proxy=false\n', encoding="utf-8")
    sessions = []
    for provider in ("openai", "codex_gateway"):
        tid = str(uuid.uuid4())
        path = home / "sessions/2026/10/27" / f"rollout-2026-10-27T00-00-00-{tid}.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        records = [
            {"type": "session_meta", "payload": {"id": tid, "timestamp": "2026-10-27T00:00:00Z",
                "cwd": str(root), "originator": "codex_cli_rs", "cli_version": version.split()[-1],
                "source": "cli", "model_provider": provider, "base_instructions": {"text": "Fixture."}}},
            {"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Synthetic history; no live model calls."}]}},
            {"type": "response_item", "payload": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "Synthetic answer."}]}}]
        path.write_text("".join(json.dumps({"timestamp": "2026-10-27T00:00:00Z", **r}) + "\n" for r in records), encoding="utf-8")
        sessions.append((provider, tid, path, path.read_bytes()))
    if identity == "codex_gateway":
        # Simulate an upgrade from 0.3.x, preserving both the original backup and active identity.
        backup = gateway / "backups/config.toml.fixture"
        backup.parent.mkdir()
        backup.write_bytes((home / "config.toml").read_bytes())
        (gateway / "state.json").write_text(json.dumps({"backup_id": backup.name, "managed_config": True}), encoding="utf-8")
        (home / "config.toml").write_text(
            'model="gpt-6.1-sol"\nmodel_provider="codex_gateway"\n'
            '[model_providers.codex_gateway]\nname="codex-gateway"\n'
            f'base_url="http://127.0.0.1:{port}/v1"\nwire_api="responses"\nrequires_openai_auth=true\n', encoding="utf-8")
    child = None
    try:
        for cycle in range(2):
            child = subprocess.Popen([str(exe), "start", "--foreground"], env=env, cwd=root,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                     creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
            deadline = time.monotonic() + 15
            while True:
                assert child.poll() is None, "gateway exited before activation"
                if (gateway / "gateway.pid").exists() and f'openai_base_url="http://127.0.0.1:{port}/v1"' in (home / "config.toml").read_text().replace(' ', ''):
                    break
                assert time.monotonic() < deadline, "gateway activation timed out"
                time.sleep(.05)
            catalog = json.loads((home / "codex-gateway-model-catalog.json").read_text())
            server = AppServer(cli, env, root)
            try:
                models = server.call("model/list", {"limit": 100})["data"]
                slugs = {m["model"] for m in models}
                expected = {"deepseek-flash", "deepseek-v4-pro", "gpt-5.6-sol", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-sol", "gpt-6-astra", "gpt-6-luna"}
                assert expected <= slugs, (expected - slugs)
                sol = next(m for m in models if m["model"] == "gpt-5.6-sol")
                assert [r["reasoningEffort"] for r in sol["supportedReasoningEfforts"]] == [
                    "low", "medium", "high", "xhigh", "max", "ultra"]
                assert {m["slug"] for m in catalog["models"]} == slugs
                if args.models_only:
                    print("PASS: model/list includes Max and Ultra; DeepSeek and official models load together.", flush=True)
                    return
                for provider, tid, _, _ in sessions:
                    if provider != identity:
                        try:
                            server.call("thread/resume", {"threadId": tid, "cwd": str(root)})
                        except AssertionError as error:
                            assert "modelProviderMismatch" in str(error), str(error)
                        else:
                            raise AssertionError("Expected client to reject a different provider identity")
                        continue
                    resumed = server.call("thread/resume", {"threadId": tid, "cwd": str(root)})
                    # The history identity must stay stable across activation and shutdown.
                    assert resumed["modelProvider"] == identity
                    native_route = server.call("config/read", {})["config"].get("openai_base_url")
                    assert native_route == f"http://127.0.0.1:{port}/v1", native_route
                    config = server.call("config/read", {})["config"]
                    assert config["model_providers"]["codex_gateway"]["base_url"] == native_route
                    resumed = server.call("thread/resume", {"threadId": tid, "cwd": str(root), "modelProvider": provider})
                    # Matching provider must resume on both cold and already-loaded paths.
                    assert resumed["modelProvider"] == identity
                print(f"{identity} cycle {cycle + 1}: model/list, both provider route settings and history resume passed", flush=True)
            finally:
                server.close()
            stopped = subprocess.run([str(exe), "stop"], env=env, cwd=root, capture_output=True, text=True, encoding="utf-8", timeout=15)
            assert stopped.returncode == 0, stopped.stdout + stopped.stderr
            child.wait(timeout=5)
            child = None
            server = AppServer(cli, env, root)
            try:
                config = server.call("config/read", {})["config"]
                assert not config.get("openai_base_url")
                assert config.get("model_provider", "openai") == identity
                assert config["model_providers"]["codex_gateway"]["base_url"] == "https://chatgpt.com/backend-api/codex"
                for provider, tid, _, _ in sessions:
                    if provider != identity:
                        try:
                            server.call("thread/resume", {"threadId": tid, "cwd": str(root)})
                        except AssertionError as error:
                            assert "modelProviderMismatch" in str(error), str(error)
                        else:
                            raise AssertionError("Expected client to reject a different provider identity")
                        continue
                    resumed = server.call("thread/resume", {"threadId": tid, "cwd": str(root), "modelProvider": provider})
                    # Matching provider must resume on both cold and already-loaded paths.
                    assert resumed["modelProvider"] == identity
                print(f"{identity} cycle {cycle + 1}: stopped gateway histories still resume", flush=True)
            finally:
                server.close()
        other = "codex_gateway" if identity == "openai" else "openai"
        server = AppServer(cli, env, root, provider=other)
        try:
            tid = next(tid for provider, tid, _, _ in sessions if provider == other)
            resumed = server.call("thread/resume", {"threadId": tid, "cwd": str(root)})
            assert resumed["modelProvider"] == other
            print(f"Legacy {other} history resumes when runtime provider matches; no history migration needed", flush=True)
        finally:
            server.close()
        for _, _, path, original in sessions:
            # App-server may append resume metadata; the original history must remain byte-for-byte intact.
            assert path.read_bytes().startswith(original)
        print("PASS: no credentials, model turns, tools or external API requests; fixture:", root, flush=True)
    finally:
        if child and child.poll() is None:
            child.terminate()
            child.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex-exe", type=Path, required=True)
    parser.add_argument("--gateway-exe", type=Path, required=True)
    parser.add_argument("--models-only", action="store_true")
    args = parser.parse_args()
    for identity in ("openai", "codex_gateway"):
        run_case(args, identity)


if __name__ == "__main__":
    main()
