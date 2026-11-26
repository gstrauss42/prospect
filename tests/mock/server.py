#!/usr/bin/env python3
"""One mock for every upstream Opportunity talks to, so tests never spend real money.

    python3 tests/mock/server.py --port 9911

Point the app at it:

    DEEPSEEK_BASE_URL=http://127.0.0.1:9911
    OPENVERSE_URL=http://127.0.0.1:9911/openverse/v1
    COMMONS_API_URL=http://127.0.0.1:9911/commons/w/api.php
    MAILGUN_BASE_URL=http://127.0.0.1:9911/mailgun   (with any MAILGUN_API_KEY/DOMAIN/SENDER_EMAIL)

Routes by path prefix to one module per upstream (each exposes handle(req) -> Response):

    /v1/..., /user/...      deepseek_mock   chat completions, Search API, Fetch API, balance
    /openverse/, /commons/, /img/   media_mock   image search and image bytes
    /mailgun/               mailgun_mock    Mailgun messages API
    /bedrock-runtime/, /bedrock/            bedrock_mock     Converse API, model listing
    /slack/                                 slack_mock       OAuth + Web API (read methods)
    /mslogin/, /msgraph/                    microsoft_mock   Entra ID OAuth + Microsoft Graph
    /atlassian-auth/, /atlassian-api/, /confluence-site/   confluence_mock   OAuth + Confluence REST

The connector mocks also serve the OAuth *authorize* pages: they redirect straight back to the
redirect_uri with a code and the state (an instant "Allow"), so OAuth can be tested end to end.
Their data endpoints answer GET only; any write method gets 405 and is logged as a violation in
GET /_violations (the read-only guarantee is tested by asserting that list stays empty).

Control endpoints for tests:

    GET  /_health                       -> {"ok": true}
    GET  /_log                          -> every request seen: [{method, path, stage, body}]
    POST /_reset                        -> clears the log, injected failures and module state
    POST /_fail {"prefix": "/v1/tools/search", "status": 429, "times": 2, "delay": 0}
                                        -> the next `times` requests under `prefix` fail with
                                           `status` (status 0 = drop the connection; delay in
                                           seconds = answer slowly, for timeout tests)
    GET  /_mail                         -> messages the Mailgun mock received
    GET  /_violations                   -> write attempts against connector data APIs (must be [])
"""

import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

import bedrock_mock
import confluence_mock
import deepseek_mock
import mailgun_mock
import media_mock
import microsoft_mock
import slack_mock
from mocklib import Request, Response, error_response, json_response, not_found, reset_violations, violations

ROUTES = [
    ("/v1/", deepseek_mock),
    ("/user/", deepseek_mock),
    ("/openverse/", media_mock),
    ("/commons/", media_mock),
    ("/img/", media_mock),
    ("/mailgun/", mailgun_mock),
    ("/bedrock-runtime/", bedrock_mock),
    ("/bedrock/", bedrock_mock),
    ("/slack/", slack_mock),
    ("/mslogin/", microsoft_mock),
    ("/msgraph/", microsoft_mock),
    ("/atlassian-auth/", confluence_mock),
    ("/atlassian-api/", confluence_mock),
    ("/confluence-site/", confluence_mock),
]

LOCK = threading.Lock()
LOG = []
FAILURES = []  # dicts: prefix, status, times, delay


def reset():
    with LOCK:
        LOG.clear()
        FAILURES.clear()
    reset_violations()
    for module in (deepseek_mock, media_mock, mailgun_mock, bedrock_mock, slack_mock, microsoft_mock, confluence_mock):
        if hasattr(module, "reset"):
            module.reset()


def take_failure(path):
    with LOCK:
        for f in FAILURES:
            if path.startswith(f["prefix"]) and f["times"] > 0:
                f["times"] -= 1
                return dict(f)
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "OpportunityMock/1"

    def log_message(self, fmt, *args):  # quiet unless --verbose
        if self.server.verbose:
            sys.stderr.write("mock: " + (fmt % args) + "\n")

    def _request(self):
        parts = urlsplit(self.path)
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length > 0 else b""
        query = {k: v[0] for k, v in parse_qs(parts.query, keep_blank_values=True).items()}
        headers = {k.lower(): v for k, v in self.headers.items()}
        return Request(self.command, parts.path, query, headers, body)

    def _send(self, resp: Response):
        self.send_response(resp.status)
        self.send_header("Content-Type", resp.content_type)
        self.send_header("Content-Length", str(len(resp.body)))
        for k, v in resp.headers.items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(resp.body)

    def _handle(self):
        req = self._request()
        if req.path.startswith("/_"):
            return self._send(self._control(req))
        entry = {"method": req.method, "path": req.path, "stage": req.headers.get("x-opp-stage", ""), "body": req.body.decode("utf-8", "replace")[:20000], "at": time.time()}
        with LOCK:
            LOG.append(entry)
        failure = take_failure(req.path)
        if failure:
            if failure.get("delay"):
                time.sleep(float(failure["delay"]))
            if failure["status"] == 0:
                self.close_connection = True
                try:
                    self.connection.shutdown(2)
                except OSError:
                    pass
                return
            if failure["status"] >= 400:
                return self._send(error_response(failure["status"], f"mock: injected {failure['status']}"))
        for prefix, module in ROUTES:
            if req.path.startswith(prefix):
                try:
                    return self._send(module.handle(req))
                except Exception as e:  # a mock bug must show up loudly, not hang the client
                    import traceback

                    traceback.print_exc()
                    return self._send(error_response(500, f"mock crashed: {e}", "mock_error"))
        return self._send(not_found(req.path))

    def _control(self, req: Request) -> Response:
        if req.path == "/_health":
            return json_response({"ok": True})
        if req.path == "/_log":
            with LOCK:
                return json_response(list(LOG))
        if req.path == "/_reset" and req.method == "POST":
            reset()
            return json_response({"ok": True})
        if req.path == "/_fail" and req.method == "POST":
            spec = req.json() or {}
            f = {"prefix": spec.get("prefix", "/"), "status": int(spec.get("status", 500)), "times": int(spec.get("times", 1)), "delay": float(spec.get("delay", 0))}
            with LOCK:
                FAILURES.append(f)
            return json_response({"ok": True, "failure": f})
        if req.path == "/_mail":
            return json_response(mailgun_mock.sent())
        if req.path == "/_violations":
            return json_response(violations())
        return not_found(req.path)

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = _handle


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=9911)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.daemon_threads = True
    server.verbose = args.verbose
    print(f"mock upstreams listening on http://{args.host}:{server.server_address[1]}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
