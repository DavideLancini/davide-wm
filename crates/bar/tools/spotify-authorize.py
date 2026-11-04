#!/usr/bin/env python3
"""One-time Spotify authorisation, so the bar can read what you are playing.

The bar reads MPRIS while the desktop client is open, which needs no
permission at all. Seeing what plays on *another* device means asking
Spotify's servers instead, and `/v1/me/player/currently-playing` needs a token
that identifies **you** rather than the application — which only you can grant,
once, from a browser.

What happens here:

1. a local server opens on the redirect port and waits;
2. the browser opens Spotify's consent page;
3. Spotify redirects back with a one-time code;
4. the code is exchanged for an access token and a **refresh token**.

The refresh token is the durable part: from then on the bar renews its own
access token indefinitely without asking again. It is written to
`~/.local/state/topbar/spotify-token`, readable only by you.

Run: tools/spotify-authorize.py
"""

import base64
import http.server
import json
import os
import pathlib
import secrets
import socket
import sys
import threading
import urllib.error
import urllib.parse
import urllib.request
import webbrowser

# Same application as the bar: the credentials are compiled into the binary
# and mirrored here, since this script has to speak for the same client id.
CLIENT_ID = "ce6a15ada15e491aaef04d1cd26fbc12"
CLIENT_SECRET = "9fa59106aeb34911aabbd1d27953a985"

PORT = 8888
# A literal loopback IP, not `localhost`. Since April 2025 Spotify rejects the
# hostname form as insecure — it answers "redirect_uri: Insecure" — and accepts
# only an IP literal over plain HTTP, or HTTPS. The two are also compared as
# strings, so this exact spelling has to be the one registered in the app's
# dashboard.
REDIRECT_URI = f"http://127.0.0.1:{PORT}/callback"

# The narrowest scopes that answer "what is playing": no playlist access, no
# write permissions, nothing about the library.
SCOPES = "user-read-currently-playing user-read-playback-state"

TOKEN_URL = "https://accounts.spotify.com/api/token"
AUTH_URL = "https://accounts.spotify.com/authorize"

STATE_DIR = pathlib.Path.home() / ".local/state/topbar"
TOKEN_FILE = STATE_DIR / "spotify-token"

# Filled in by the request handler once Spotify redirects back.
result = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        query = urllib.parse.urlparse(self.path).query
        params = urllib.parse.parse_qs(query)
        result.update({k: v[0] for k, v in params.items()})

        ok = "code" in result
        body = (
            "<h2>Done.</h2><p>You can close this tab and go back to the terminal.</p>"
            if ok
            else f"<h2>Authorisation refused</h2><p>{result.get('error', '?')}</p>"
        )
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        self.wfile.write(body.encode())

    def log_message(self, *args):
        pass  # the server's own logging would clutter the terminal


def basic_auth() -> str:
    return base64.b64encode(f"{CLIENT_ID}:{CLIENT_SECRET}".encode()).decode()


def exchange(code: str) -> dict:
    """Trades the one-time code for a token pair."""
    data = urllib.parse.urlencode(
        {
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": REDIRECT_URI,
        }
    ).encode()
    req = urllib.request.Request(
        TOKEN_URL,
        data=data,
        headers={
            "Authorization": f"Basic {basic_auth()}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    return json.load(urllib.request.urlopen(req, timeout=20))


def verify(access_token: str) -> str:
    """Reads the player once, so the setup proves itself before finishing."""
    req = urllib.request.Request(
        "https://api.spotify.com/v1/me/player/currently-playing",
        headers={"Authorization": f"Bearer {access_token}"},
    )
    try:
        resp = urllib.request.urlopen(req, timeout=20)
    except urllib.error.HTTPError as e:
        return f"error {e.code}: {e.read().decode()[:120]}"
    if resp.status == 204:
        return "nothing playing (the token works)"
    payload = json.load(resp)
    item = payload.get("item") or {}
    artists = ", ".join(a["name"] for a in item.get("artists", []))
    device = (payload.get("device") or {}).get("name", "?")
    return f"{artists} - {item.get('name')}  [on: {device}]"


def main() -> int:
    # A used port means something else answers the redirect, and the code
    # would be handed to that instead.
    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1", PORT)) == 0:
            print(f"Port {PORT} is in use. Close the program holding it.")
            return 1

    # `state` guards against a redirect that did not originate here.
    state = secrets.token_urlsafe(16)
    params = urllib.parse.urlencode(
        {
            "client_id": CLIENT_ID,
            "response_type": "code",
            "redirect_uri": REDIRECT_URI,
            "scope": SCOPES,
            "state": state,
        }
    )
    url = f"{AUTH_URL}?{params}"

    server = http.server.HTTPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=server.handle_request, daemon=True).start()

    print("Opening the browser for the Spotify authorisation.")
    print("If it does not open, paste this address:\n")
    print(f"  {url}\n")
    webbrowser.open(url)
    print("Waiting for consent…")

    # handle_request serves exactly one request, so this joins as soon as
    # Spotify redirects back.
    for thread in threading.enumerate():
        if thread is not threading.current_thread() and thread.daemon:
            thread.join(timeout=300)

    if "code" not in result:
        print(f"\nAuthorisation not completed: {result.get('error', 'no answer')}")
        return 1
    if result.get("state") != state:
        print("\nUnexpected answer: the `state` does not match. Try again.")
        return 1

    try:
        tokens = exchange(result["code"])
    except urllib.error.HTTPError as e:
        detail = e.read().decode()[:200]
        print(f"\nCode exchange failed ({e.code}): {detail}")
        if "redirect_uri" in detail.lower():
            print(
                f"\nAdd {REDIRECT_URI} to the app's Redirect URIs at\n"
                "https://developer.spotify.com/dashboard and run again."
            )
        return 1

    refresh = tokens.get("refresh_token")
    if not refresh:
        print("\nSpotify returned no refresh token.")
        return 1

    STATE_DIR.mkdir(parents=True, exist_ok=True)
    TOKEN_FILE.write_text(refresh + "\n")
    # The refresh token is a long-lived credential: no reason for anyone else
    # on the machine to read it.
    os.chmod(TOKEN_FILE, 0o600)

    print(f"\nRefresh token saved to {TOKEN_FILE} (mode 600).")
    print("Check:", verify(tokens["access_token"]))
    print("\nDone: from now on the bar renews its token by itself.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
