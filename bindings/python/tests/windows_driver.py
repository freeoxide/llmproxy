"""Run-time verification of the Windows wheel under wine: the same shared
contract cases the pytest suite replays, driven by the embeddable Windows
Python with the wheel's contents installed on sys.path.

Usage (from this machine): wine <py313>/python.exe Z:\\tmp\\pywine\\driver.py
"""

import http.server
import json
import pathlib
import queue
import sys
import threading
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
SHARED = REPO_ROOT / "bindings" / "shared"

from llm_proxy import Proxy, ProxyError  # noqa: E402 - installed wheel

shared = json.loads((SHARED / "cases.json").read_text())

failures = []


def check(name, condition, detail=""):
    if condition:
        print("ok -", name)
    else:
        print("FAIL -", name, detail)
        failures.append(name)


def normalize(value):
    if isinstance(value, dict):
        return {
            k: ("<created-timestamp>" if k == "created" and isinstance(v, (int, float)) else normalize(v))
            for k, v in value.items()
        }
    if isinstance(value, list):
        return [normalize(item) for item in value]
    return value


class CannedUpstream:
    def __init__(self, canned, gap_ms=0):
        self.canned = canned
        self.gap = gap_ms / 1000.0
        self.capture = None
        upstream = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802 - stdlib naming
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length)
                try:
                    body = json.loads(raw)
                except ValueError:
                    body = {}
                with upstream.lock:
                    upstream.capture = {
                        "headers": {k.lower(): v for k, v in self.headers.items()},
                        "body": body,
                    }
                if upstream.canned["kind"] == "json":
                    payload = json.dumps(upstream.canned["body"]).encode()
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                else:
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                    self.end_headers()
                    for chunk in upstream.canned["events"]:
                        self.wfile.write(chunk.encode())
                        self.wfile.flush()
                        if upstream.gap:
                            time.sleep(upstream.gap)

            def log_message(self, *args):
                pass

        self.lock = threading.Lock()
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def base_url(self):
        return "http://127.0.0.1:%d" % self.server.server_address[1]


def provider_for(name, base_url):
    return json.loads(json.dumps(shared["providers"][name]).replace("{upstream}", base_url))


def header_subset(headers):
    return {n: headers[n] for n in shared["header_subset"] if n in headers}


def error_info(failure):
    return {
        "kind": failure.kind,
        "status": failure.status,
        "message": failure.message,
        "retry_after_secs": failure.retry_after_seconds,
        "should_retry": failure.should_retry,
    }


def replay_case(case):
    upstream = CannedUpstream(shared["upstream_responses"][case["upstream_response"]])
    proxy = Proxy({"providers": [provider_for(case["provider"], upstream.base_url)]})
    body = json.loads((REPO_ROOT / shared["fixture_root"] / case["fixture"]).read_text())
    response, events, error = None, None, None
    try:
        if case["stream"]:
            events = list(proxy.execute_stream(case["provider"], case["dialect"], body, case.get("options")))
        else:
            response = proxy.execute(case["provider"], case["dialect"], body, case.get("options"))
    except ProxyError as failure:
        error = error_info(failure)
    snapshot = {
        "upstream_request": upstream.capture["body"] if upstream.capture else None,
        "upstream_headers": header_subset(upstream.capture["headers"]) if upstream.capture else None,
        "response": response,
        "stream_events": events,
        "error": error,
    }
    expected = json.loads((SHARED / "snapshots" / ("%s.json" % case["name"])).read_text())
    check("case %s matches the shared snapshot" % case["name"], normalize(snapshot) == expected,
          "" if normalize(snapshot) == expected else "see pytest run for the diff")


def main():
    for case in shared["cases"]:
        replay_case(case)

    # Errors surface as ProxyError with the ledger's kind and status.
    upstream = CannedUpstream(shared["upstream_responses"]["anthropic_json"])
    proxy = Proxy({"providers": [provider_for("anthropic-upstream", upstream.base_url)]})
    try:
        proxy.execute("nope", "anthropic_messages", {"model": "m", "max_tokens": 4,
                                                     "messages": [{"role": "user", "content": "x"}]})
        check("unknown provider raises ProxyError", False)
    except ProxyError as failure:
        check("unknown provider raises ProxyError with kind/status",
              failure.kind == "UnknownProvider" and failure.status == 404)

    # Cancellation: stop mid-stream, settle as ClientCancelled, stay usable.
    scenario = shared["cancellation"]
    sse = CannedUpstream(shared["upstream_responses"][scenario["upstream_response"]], scenario.get("gap_ms", 250))
    followup = CannedUpstream(shared["upstream_responses"]["anthropic_json"])
    proxy = Proxy({
        "providers": [
            provider_for(scenario["stream_provider"], sse.base_url),
            provider_for(scenario["followup_provider"], followup.base_url),
        ],
        "record_usage": True,
    })
    stream = proxy.execute_stream(scenario["stream_provider"], scenario["dialect"], scenario["request"])
    first = next(iter(stream))
    check("first event is a tagged MessageStart", "MessageStart" in first)
    stream.cancel()
    try:
        next(iter(stream))
        check("cancelled stream stops delivering", False)
    except StopIteration:
        check("cancelled stream stops delivering", True)

    deadline = time.monotonic() + scenario.get("settle_timeout_secs", 5)
    settled = False
    while time.monotonic() < deadline:
        if any(row.get("kind") == "response_failed" and row.get("error_kind") == "ClientCancelled"
               for row in proxy.usage_events()):
            settled = True
            break
        time.sleep(0.05)
    check("ClientCancelled usage row settles after cancel", settled)

    reply = proxy.execute(scenario["followup_provider"], scenario["dialect"],
                          {"model": "claude-sonnet-4-5", "max_tokens": 64,
                           "messages": [{"role": "user", "content": "again"}]})
    check("proxy stays usable after cancellation", "id" in reply)

    # The watchdog shape: while a reader thread is blocked in next() on a
    # stalled upstream (one SSE event, then nothing for an hour), cancel()
    # from this thread must not raise 'Already borrowed', must release the
    # reader, and the request must settle as ClientCancelled.
    stalled = CannedUpstream(shared["upstream_responses"][scenario["upstream_response"]], 3_600_000)
    proxy = Proxy({
        "providers": [
            provider_for(scenario["stream_provider"], stalled.base_url),
            provider_for(scenario["followup_provider"], followup.base_url),
        ],
        "record_usage": True,
    })
    stream = proxy.execute_stream(scenario["stream_provider"], scenario["dialect"],
                                  scenario["request"])
    events = queue.Queue()

    def read_until_end():
        for event in stream:
            events.put(event)
        events.put(None)

    reader = threading.Thread(target=read_until_end, daemon=True)
    reader.start()
    first = events.get(timeout=3)
    check("stalled stream delivers its first event", "MessageStart" in first)
    try:
        events.get(timeout=0.3)
        parked = False
    except queue.Empty:
        parked = True
    check("reader is parked before cancel", parked)
    stream.cancel()
    check("cancelled getter readable while reader blocked", stream.cancelled)
    ended = events.get(timeout=5)
    check("cancel unblocks the parked reader", ended is None)
    reader.join(timeout=5)
    check("reader thread exits after cancel", not reader.is_alive())

    deadline = time.monotonic() + scenario.get("settle_timeout_secs", 5)
    settled = False
    while time.monotonic() < deadline:
        if any(row.get("kind") == "response_failed" and row.get("error_kind") == "ClientCancelled"
               for row in proxy.usage_events()):
            settled = True
            break
        time.sleep(0.05)
    check("ClientCancelled row settles after the watchdog cancel", settled)

    print("python", sys.version)
    if failures:
        print("FAILED: %d" % len(failures))
        sys.exit(1)
    print("ALL CHECKS PASSED")


main()
