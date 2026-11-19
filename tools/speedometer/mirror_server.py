# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Serves the local Speedometer 3.1 mirror on 127.0.0.1 for headless runs (`run.ps1`).

    python tools/speedometer/mirror_server.py <port> [--mirror DIR]

A file the mirror does not have yet is fetched once from browserbench.org and kept (as
`window_ab.py` fills it), so a run waits on the internet at most once per file. Module scripts get a
JavaScript type: Windows' registry may say text/plain for `.mjs`, which a module script refuses.
"""

import argparse
import http.server
import os
import threading
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
import os as _os, sys as _sys  # noqa: E401
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), ".."))
import devenv  # noqa: E402

SITE = "https://browserbench.org/Speedometer3.1"


class Mirror(http.server.SimpleHTTPRequestHandler):
    extensions_map = {".mjs": "text/javascript", ".js": "text/javascript"}
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def fill(self, path):
        """Puts the file in the mirror if it is not there. False when the site has no such file."""
        local = Path(self.directory) / urllib.parse.unquote(path).lstrip("/")
        if local.exists() or ".." in path:
            return True
        request = urllib.request.Request(SITE + path, headers={"User-Agent": "speedometer-mirror"})
        try:
            with urllib.request.urlopen(request, timeout=60) as r:
                body = r.read()
        except urllib.error.HTTPError:
            return False
        local.parent.mkdir(parents=True, exist_ok=True)
        part = local.with_name(f"{local.name}.{threading.get_ident()}.part")
        part.write_bytes(body)
        os.replace(part, local)
        return True

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        if path.endswith("/"):
            path += "index.html"
        if not self.fill(path):
            return self.send_error(404)
        return super().do_GET()


class Quiet(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        pass  # (a run that ends resets its connections)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("--mirror", default=str(devenv.speedometer_dir()))
    a = ap.parse_args()
    handler = lambda *args, **kw: Mirror(*args, directory=a.mirror, **kw)  # noqa: E731
    Quiet(("127.0.0.1", a.port), handler).serve_forever()


if __name__ == "__main__":
    main()
