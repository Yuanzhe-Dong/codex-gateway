"""Opt-in official upstream smoke test; one billed turn, no tools or real subagents.

Reads OAuth in memory, sends only synthetic messages, and never writes credentials.
Run against a running local gateway: python tests/live_smoke.py --live
"""
import argparse
import json
import os
from pathlib import Path
import urllib.error
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", required=True)
    parser.add_argument("--gateway-url", default="http://127.0.0.1:17899/v1")
    parser.add_argument("--model", default="gpt-6-luna")
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.gateway_url)
    if url.scheme != "http" or url.hostname not in ("127.0.0.1", "localhost"):
        parser.error("gateway-url must be an HTTP loopback endpoint")
    home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    tokens = json.loads((home / "auth.json").read_text(encoding="utf-8"))["tokens"]
    marker = "GATEWAY_AGENT_OK"
    request_body = {
        "model": args.model,
        "instructions": "This is a gateway compatibility probe. Return only the requested marker.",
        "stream": True, "store": False, "reasoning": {"effort": "low"},
        "tools": [],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Read the worker message and return its marker."}]},
            {"type": "agent_message", "author": "/root/fixture_worker", "recipient": "/root",
             "content": [{"type": "input_text", "text": f"The marker is {marker}."}]},
        ],
    }
    request = urllib.request.Request(args.gateway_url.rstrip("/") + "/responses",
        data=json.dumps(request_body).encode(), headers={
            "Authorization": "Bearer " + tokens["access_token"],
            "chatgpt-account-id": tokens["account_id"],
            "Content-Type": "application/json", "Accept": "text/event-stream",
            "originator": "codex_cli_rs",
        })
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        response = opener.open(request, timeout=90)
    except urllib.error.HTTPError as error:
        try:
            detail = json.loads(error.read()).get("error", {})
        except (ValueError, UnicodeDecodeError):
            detail = {}
        raise SystemExit(f"FAIL HTTP {error.code}; code={detail.get('code')}; param={detail.get('param')}") from None
    output, completed = "", False
    with response:
        for line in response:
            if not line.startswith(b"data:"):
                continue
            data = line[5:].strip()
            if data == b"[DONE]":
                continue
            event = json.loads(data)
            if event.get("type") == "response.output_text.delta":
                output += event.get("delta", "")
            if event.get("type") == "response.completed":
                completed = True
            if event.get("type") in ("error", "response.failed"):
                raise SystemExit("FAIL: upstream SSE error event")
    if not completed or output.strip() != marker:
        raise SystemExit(f"FAIL: completed={completed}, marker_match={output.strip() == marker}")
    print(f"PASS: {args.model}, HTTP 200, agent_message content accepted, marker matched, SSE completed")


if __name__ == "__main__":
    main()
