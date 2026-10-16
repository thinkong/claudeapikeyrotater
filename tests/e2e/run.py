"""End-to-end test: mock upstream + the real proxy binary + the official anthropic SDK.

    python tests/e2e/run.py target/debug/claude-key-router[.exe]

Requires `pip install anthropic httpx`. Keys and clients are created through the
admin API (the same one the web UI uses), not the config file.
"""
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time

import anthropic
import httpx

HERE = os.path.dirname(os.path.abspath(__file__))
MOCK = "http://127.0.0.1:9901"
PROXY = "http://127.0.0.1:9902"
ADMIN = {"x-api-key": "admin-secret"}
SECRETS = {"a": "sk-a", "a2": "sk-x", "b": "sk-b", "c": "sk-c"}
CLIENT = None  # created via the admin API

CONFIG = """
listen = "127.0.0.1:9902"
upstream = "http://127.0.0.1:9901"
database = "{db}"
admin_token = "admin-secret"

[cooldowns]
credit_exhausted_secs = 600
"""

failures = []


def check(cond, what):
    print(("  ok   " if cond else "  FAIL ") + what)
    if not cond:
        failures.append(what)


def mock_config(**kw):
    cfg = {"credit_400": [], "credit_402": [], "ratelimit": [], "auth": []}
    cfg.update(kw)
    httpx.post(MOCK + "/mock/config", json=cfg)


def admin(method, path, body=None):
    return httpx.request(method, PROXY + "/proxy/admin" + path, json=body, headers=ADMIN, timeout=30)


def keys():
    return {k["name"]: k for k in admin("GET", "/keys").json()}


def refresh_all():
    for k in keys():
        admin("POST", f"/keys/{k}/refresh")


def msg_body(aff_text="hi", **extra):
    b = {"model": "claude-opus-5-5", "max_tokens": 100, "messages": [{"role": "user", "content": aff_text}]}
    b.update(extra)
    return b


def post_msg(body, headers=None, token=None):
    h = {"x-api-key": token or CLIENT, "anthropic-version": "2023-06-01"}
    h.update(headers or {})
    return httpx.post(PROXY + "/v1/messages", json=body, headers=h, timeout=30)


def me(token=None):
    return httpx.get(PROXY + "/proxy/me", headers={"x-api-key": token or CLIENT}).json()


def wait_up(url):
    for _ in range(100):
        try:
            httpx.get(url, timeout=1)
            return
        except Exception:
            time.sleep(0.1)
    raise SystemExit("server did not start: " + url)


def main():
    binary = os.path.abspath(sys.argv[1])
    tmp = tempfile.mkdtemp()
    cfg_path = os.path.join(tmp, "config.toml")
    with open(cfg_path, "w") as f:
        f.write(CONFIG.format(db=os.path.join(tmp, "test.db").replace("\\", "/")))
    mock = subprocess.Popen([sys.executable, os.path.join(HERE, "mock_upstream.py"), "9901"])
    proxy = None
    try:
        proxy = subprocess.Popen([binary, cfg_path], env={**os.environ, "RUST_LOG": "error"})
        wait_up(MOCK + "/mock/log")
        wait_up(PROXY + "/health")
        run_tests()
    finally:
        if proxy:
            proxy.terminate()
        mock.terminate()
    print()
    if failures:
        print(f"{len(failures)} FAILED")
        sys.exit(1)
    print("all passed")


def run_tests():
    global CLIENT

    print("admin API: clients and keys")
    r = httpx.get(PROXY + "/ui")
    check(r.status_code == 200 and "<title>Key Router</title>" in r.text, "web UI served at /ui")
    check(httpx.get(PROXY + "/proxy/status").status_code == 401, "admin API requires the admin token")
    r = admin("POST", "/clients", {"name": "alice"})
    CLIENT = r.json()["token"]
    check(r.status_code == 201 and CLIENT.startswith("sk-ckr-"), "client created, token returned once")
    check(admin("POST", "/clients", {"name": "alice"}).status_code == 400, "duplicate client name rejected")
    check(admin("POST", "/clients", {"name": "bad name!"}).status_code == 400, "invalid client name rejected")
    check(CLIENT not in json.dumps(admin("GET", "/clients").json()), "client list never exposes tokens")
    r = post_msg(msg_body())
    check(r.status_code == 503, f"no upstream keys -> 503 ({r.status_code})")
    mock_config()
    r = admin("POST", "/keys", {"name": "a", "key": SECRETS["a"], "balance_usd": 20})
    check(r.status_code == 201 and r.json()["check"]["ok"], "key added and checked with a 1-token request")
    for n in ["a2", "b", "c"]:
        admin("POST", "/keys", {"name": n, "key": SECRETS[n], "check": False})
    check(admin("POST", "/keys", {"name": "dup", "key": SECRETS["b"]}).status_code == 400, "same secret twice rejected")
    k = keys()
    check(len(k) == 4 and k["a"]["key_hint"] == "…sk-a" and not ({"key", "secret"} & set(k["a"])), "key list shows hints only")
    time.sleep(0.2)
    k = keys()
    check(k["a"]["estimated_remaining_usd"] is not None and k["a"]["estimated_remaining_usd"] < 20, "check request is metered against the balance")

    print("client auth")
    r = httpx.post(PROXY + "/v1/messages", json=msg_body(), headers={"x-api-key": "wrong"})
    check(r.status_code == 401 and r.json()["error"]["type"] == "authentication_error", "unknown client token -> 401")

    print("non-streaming cost + byte-for-byte forwarding")
    r = post_msg(msg_body())
    check(r.status_code == 200, "200 OK")
    # 100*4 + 200*5 (5m write) + 1000*0.2 (read) + 50*20 = 2600 / 1e6
    check(r.headers.get("x-proxy-cost-usd") == "0.002600", f"x-proxy-cost-usd header ({r.headers.get('x-proxy-cost-usd')})")
    check("proxy_cost" not in r.text and r.headers.get("content-length") == str(len(r.content)), "response body untouched (no injected fields)")
    rid = r.headers["x-proxy-request-id"]
    # Odd spacing and key order: upstream must receive exactly these bytes.
    raw = b'{ "messages":[{"content":"hi","role":"user"}],  "model":"claude-opus-5-5","max_tokens":100 }'
    r = httpx.post(PROXY + "/v1/messages", content=raw, headers={"x-api-key": CLIENT, "content-type": "application/json"})
    check(r.headers.get("x-mock-body-sha256") == hashlib.sha256(raw).hexdigest(), "request body forwarded byte-for-byte (no injection)")
    time.sleep(0.2)
    row = httpx.get(PROXY + f"/proxy/requests/{rid}", headers={"x-api-key": CLIENT}).json()
    check(row.get("cost_usd") == 0.0026 and row["usage"]["cache_read_input_tokens"] == 1000, "request lookup by client")

    print("SDK")
    sdk = anthropic.Anthropic(base_url=PROXY, api_key=CLIENT, max_retries=0)
    m = sdk.messages.create(**msg_body())
    check(m.content[0].text == "hello" and m.usage.input_tokens == 100, "SDK non-streaming")
    with sdk.messages.stream(**msg_body("stream please")) as s:
        final = s.get_final_message()
    text = "".join(b.text for b in final.content if b.type == "text")
    check(text == "Hello world" and final.usage.output_tokens == 50, f"SDK streaming intact ({text!r})")
    raw = httpx.post(PROXY + "/v1/messages", json=msg_body("stream raw", stream=True), headers={"x-api-key": CLIENT})
    check("proxy_cost" not in raw.text and raw.text.count("event: message_delta") == 1, "stream events untouched")
    time.sleep(0.3)
    row = httpx.get(PROXY + f"/proxy/requests/{raw.headers['x-proxy-request-id']}", headers={"x-api-key": CLIENT}).json()
    check(row["cost_usd"] == 0.0026 and row["usage"]["output_tokens"] == 50, "stream cost still metered (request lookup)")

    print("credit exhaustion (400 and 402 shapes) -> per-key cooldown + failover")
    mock_config(credit_400=["sk-a", "sk-x"], credit_402=["sk-c"])
    ok = True
    for i in range(12):
        r = post_msg(msg_body(f"conversation {i}"))
        ok &= r.status_code == 200 and r.headers.get("x-proxy-upstream-key") == "b"
    check(ok, "every request served by the only funded key")
    k = keys()
    check(all(k[n]["state"] == "out_of_credits" for n in ["a", "a2", "c"]), "a, a2 (400 shape) and c (402 shape) out of credits")
    check(k["a"]["cooldown_secs"] > 500, "cooldown uses credit_exhausted_secs")
    r = admin("POST", "/keys/a/refresh").json()
    check(r["check"]["ok"] is False and "credit" in r["check"]["message"].lower() and keys()["a"]["state"] == "out_of_credits", "refresh re-checks and keeps an empty key benched")

    print("all keys out of credits")
    mock_config(credit_400=["sk-a", "sk-x", "sk-b"], credit_402=["sk-c"])
    post_msg(msg_body("anything"))
    r = post_msg(msg_body("anything again"))
    check(r.status_code == 402 and r.json()["error"]["type"] == "billing_error" and "retry-after" in r.headers, "proxy answers 402 billing_error with retry-after")

    print("top-up and refresh")
    mock_config()
    admin("PATCH", "/keys/a", {"balance_usd": 25})
    k = keys()["a"]
    check(k["state"] == "available" and k["estimated_remaining_usd"] == 25, "recording a balance lifts the credit cooldown")
    r = admin("POST", "/keys/b/refresh").json()
    check(r["check"]["ok"] and keys()["b"]["state"] == "available", "refresh brings a funded key back")
    refresh_all()

    print("rate limit -> key cooldown from retry-after, failover")
    mock_config(ratelimit=["sk-a"])
    ok = all(post_msg(msg_body(f"rl {i}")).status_code == 200 for i in range(12))
    k = keys()["a"]
    check(ok, "all requests succeeded")
    check(k["state"] == "cooling_down" and 0 < k["cooldown_secs"] <= 7, f"key a cooling for retry-after ({k['cooldown_secs']}s)")
    mock_config()
    refresh_all()

    print("sticky routing moves off a benched key, and back after refresh")
    aff = {"x-proxy-affinity": "conversation-42"}
    home = post_msg(msg_body("sticky"), aff).headers["x-proxy-upstream-key"]
    same = all(post_msg(msg_body("sticky"), aff).headers["x-proxy-upstream-key"] == home for _ in range(3))
    check(same, f"same affinity sticks to one key ({home})")
    mock_config(credit_400=[SECRETS[home]])
    r = post_msg(msg_body("sticky"), aff)
    moved = r.headers["x-proxy-upstream-key"]
    check(r.status_code == 200 and moved != home and r.headers["x-proxy-attempts"] == "2", f"{home} out of credits mid-conversation -> retried on {moved}, client still gets 200")
    rs = [post_msg(msg_body("sticky"), aff) for _ in range(3)]
    check(all(x.headers["x-proxy-upstream-key"] == moved and x.headers["x-proxy-attempts"] == "1" for x in rs), f"later turns go straight to {moved} while {home} is benched")
    mock_config()
    admin("POST", f"/keys/{home}/refresh")
    check(post_msg(msg_body("sticky"), aff).headers["x-proxy-upstream-key"] == home, f"after refresh the conversation returns to {home}")

    print("disable / replace / remove keys")
    for n in ["a", "a2", "c"]:
        admin("PATCH", f"/keys/{n}", {"enabled": False})
    ok = all(post_msg(msg_body(f"d {i}")).headers.get("x-proxy-upstream-key") == "b" for i in range(6))
    check(ok and keys()["a"]["state"] == "disabled", "disabled keys get no traffic")
    for n in ["a", "a2", "c"]:
        admin("PATCH", f"/keys/{n}", {"enabled": True})
    mock_config(auth=["sk-c"])
    admin("POST", "/keys/c/refresh")
    c = keys()["c"]
    check(c["state"] == "cooling_down" and "authentication" in (c["reason"] or ""), "invalid key detected by refresh")
    r = admin("PATCH", "/keys/c", {"key": "sk-c2"}).json()
    check(r["check"]["ok"] and keys()["c"]["state"] == "available" and keys()["c"]["key_hint"] == "…k-c2", "replacing the secret clears the cooldown and re-checks")
    SECRETS["c"] = "sk-c2"
    mock_config()

    print("resource pinning (files)")
    r = httpx.post(PROXY + "/v1/files", files={"file": ("x.txt", b"hello")}, headers={"x-api-key": CLIENT})
    fid, owner = r.json()["id"], r.headers["x-proxy-upstream-key"]
    ok = True
    for i in range(8):
        body = msg_body()
        body["messages"] = [{"role": "user", "content": [{"type": "document", "source": {"type": "file", "file_id": fid}}, {"type": "text", "text": f"q{i}"}]}]
        r = post_msg(body)
        ok &= r.status_code == 200 and r.headers.get("x-proxy-upstream-key") == owner
    check(ok, f"messages referencing {fid} always routed to {owner}")
    r = httpx.get(PROXY + f"/v1/files/{fid}/content", headers={"x-api-key": CLIENT})
    check(r.status_code == 200 and r.headers.get("x-proxy-upstream-key") == owner, "file download routed by path pin")
    mock_config(credit_400=[SECRETS[owner]])
    post_msg(msg_body("bench me"), {"x-proxy-key": owner})
    r = httpx.get(PROXY + f"/v1/files/{fid}/content", headers={"x-api-key": CLIENT})
    check(r.status_code == 200 and keys()[owner]["state"] == "out_of_credits", "pinned download works but does not lift the credit cooldown")
    mock_config()
    refresh_all()

    print("batches")
    r = httpx.post(PROXY + "/v1/messages/batches", json={"requests": [{"custom_id": "a", "params": msg_body()}, {"custom_id": "b", "params": msg_body("x")}]}, headers={"x-api-key": CLIENT})
    bid = r.json()["id"]
    before = me()["spend_usd"]
    for _ in range(2):
        r = httpx.get(PROXY + f"/v1/messages/batches/{bid}/results", headers={"x-api-key": CLIENT})
        lines = [json.loads(l) for l in r.text.splitlines() if l]
    check(r.status_code == 200 and "proxy_cost" not in r.text and len(lines) == 2, "batch results untouched")
    time.sleep(0.3)
    check(abs(me()["spend_usd"] - before - 0.0013) < 1e-9, "batch item metered at 50%, once despite two downloads")
    b = sdk.messages.batches.retrieve(bid)
    check(b.results_url.startswith(PROXY), f"results_url rewritten to proxy ({b.results_url})")
    results = list(sdk.messages.batches.results(bid))
    check(len(results) == 2 and results[0].result.type == "succeeded", "SDK reads batch results through the proxy")

    print("client disconnect mid-stream still records usage")
    with httpx.stream("POST", PROXY + "/v1/messages", json=msg_body("slow", stream=True), headers={"x-api-key": CLIENT, "x-mock-slow": "1"}) as r:
        rid = r.headers["x-proxy-request-id"]
        next(r.iter_raw())
    time.sleep(1.0)
    row = httpx.get(PROXY + f"/proxy/requests/{rid}", headers=ADMIN).json()
    check("ended early" in (row.get("error") or "") and row["usage"]["input_tokens"] == 100 and row["cost_usd"] > 0, "partial stream recorded and billed")

    print("client management")
    r = admin("POST", "/clients/alice/rotate").json()
    old, CLIENT = CLIENT, r["token"]
    check(post_msg(msg_body(), token=old).status_code == 401 and post_msg(msg_body()).status_code == 200, "rotated token works, old one is rejected")
    time.sleep(0.2)
    admin("PATCH", "/clients/alice", {"budget_usd": 0.001})
    r = post_msg(msg_body())
    check(r.status_code == 402 and "budget" in r.json()["error"]["message"].lower(), "client over budget -> 402")
    admin("PATCH", "/clients/alice", {"budget_usd": None})
    check(post_msg(msg_body()).status_code == 200, "clearing the budget re-allows requests")
    admin("PATCH", "/clients/alice", {"enabled": False})
    check(post_msg(msg_body()).status_code == 401, "disabled client rejected")
    admin("PATCH", "/clients/alice", {"enabled": True})
    tmp = admin("POST", "/clients", {"name": "tmp"}).json()["token"]
    admin("DELETE", "/clients/tmp")
    check(post_msg(msg_body(), token=tmp).status_code == 401 and "tmp" not in [c["name"] for c in admin("GET", "/clients").json()], "removed client rejected")

    print("removing a key")
    admin("DELETE", f"/keys/{owner}")
    check(owner not in keys(), f"{owner} removed")
    r = httpx.get(PROXY + f"/v1/files/{fid}/content", headers={"x-api-key": CLIENT})
    check(r.status_code == 404 and "no longer in the pool" in r.json()["error"]["message"], "resources of a removed key give a clear 404")
    check(post_msg(msg_body("after removal")).status_code == 200, "traffic continues on remaining keys")

    print("reporting")
    sdk = anthropic.Anthropic(base_url=PROXY, api_key=CLIENT, max_retries=0)
    check(sdk.models.list().data[0].id == "claude-opus-5-5", "GET /v1/models passes through")
    recent = admin("GET", "/requests?limit=5").json()
    check(len(recent) == 5 and recent[0]["ts_ms"] >= recent[-1]["ts_ms"], "recent requests feed for the UI")
    u = httpx.get(PROXY + "/proxy/usage?group_by=key", headers=ADMIN).json()
    check(len(u["rows"]) >= 2, "usage report grouped by key")
    log = httpx.get(MOCK + "/mock/log").json()
    check(all(e["key"] not in (CLIENT, old, tmp) for e in log), "client tokens never forwarded upstream")


if __name__ == "__main__":
    main()
