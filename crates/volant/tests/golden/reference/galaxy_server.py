# SPDX-License-Identifier: GPL-3.0-or-later
"""A fake Galaxy server for the `galaxy` kind and for Volant's galaxy tests, standard library only.

It answers the Galaxy API v3 for collections and v1 for roles, over plain HTTP on 127.0.0.1, from
fixtures it builds itself: every archive is made in memory with fixed times and owners, so its bytes
and its SHA-256 are the same on every machine and every run. Each request is logged as
`METHOD /path?query`; `GET /_log` returns the log as JSON without adding itself to it.

Run as a script, it prints the port on the first line of stdout and serves until stdin closes.
"""
import gzip
import hashlib
import http.server
import io
import json
import socketserver
import sys
import tarfile
import threading
from urllib.parse import parse_qs, urlparse

README = "# {name}\n\nA fixture of Volant's galaxy tests.\n"

# namespace.name -> version -> dependencies. Three versions of `demo.base` take two pages of the
# version list at `PAGE` entries a page.
COLLECTIONS = {
    "demo.base": {"1.0.0": {}, "1.1.0": {}, "2.0.0": {}},
    "demo.app": {"1.0.0": {"demo.base": ">=1.0.0,<2.0.0"}},
}
# owner.name -> versions, oldest first.
ROLES = {"demo.web": ["1.0.0", "1.1.0"]}
PAGE = 2
# Stands for every date the API reports.
DATE = "2026-01-01T00:00:00.000000Z"


def _tar_gz(members):
    """A gzipped tarball of `members`, a list of (name, bytes or None for a directory), with fixed
    times, owners and modes."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            if data is None:
                info.type = tarfile.DIRTYPE
                info.mode = 0o755
                tar.addfile(info)
            else:
                info.mode = 0o644
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:
        gz.write(raw.getvalue())
    return out.getvalue()


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _json(value):
    return json.dumps(value, indent=4, sort_keys=True).encode()


def collection_artifact(fqcn, version, dependencies):
    """The tarball `ansible-galaxy collection build` would make for this fixture."""
    namespace, name = fqcn.split(".")
    files = {
        "README.md": README.format(name=fqcn).encode(),
        "plugins/README.md": b"No plugins.\n",
    }
    entries = [{"name": ".", "ftype": "dir", "chksum_type": None, "chksum_sha256": None, "format": 1}]
    for directory in ("plugins",):
        entries.append({"name": directory, "ftype": "dir", "chksum_type": None, "chksum_sha256": None, "format": 1})
    for path in sorted(files):
        entries.append({"name": path, "ftype": "file", "chksum_type": "sha256", "chksum_sha256": _sha256(files[path]), "format": 1})
    files_json = _json({"files": entries, "format": 1})
    manifest = _json({
        "collection_info": {
            "namespace": namespace,
            "name": name,
            "version": version,
            "authors": ["Volant"],
            "readme": "README.md",
            "tags": [],
            "description": f"The {fqcn} fixture.",
            "license": ["GPL-3.0-or-later"],
            "license_file": None,
            "dependencies": dependencies,
            "repository": None,
            "documentation": None,
            "homepage": None,
            "issues": None,
        },
        "file_manifest_file": {
            "name": "FILES.json",
            "ftype": "file",
            "chksum_type": "sha256",
            "chksum_sha256": _sha256(files_json),
            "format": 1,
        },
        "format": 1,
    })
    members = [("MANIFEST.json", manifest), ("FILES.json", files_json), ("plugins", None)]
    members += [(path, files[path]) for path in sorted(files)]
    return _tar_gz(members)


def role_archive(owner, name, version):
    """The archive a GitHub tag of the role would give: everything under `<name>-<version>/`."""
    top = f"{name}-{version}"
    meta = (
        "galaxy_info:\n"
        f"  author: {owner}\n"
        f"  role_name: {name}\n"
        f"  namespace: {owner}\n"
        f"  description: The {owner}.{name} fixture.\n"
        "  license: GPL-3.0-or-later\n"
        "  min_ansible_version: '2.19'\n"
        "  galaxy_tags: []\n"
        "dependencies: []\n"
    ).encode()
    tasks = f"- name: say {version}\n  ansible.builtin.debug:\n    msg: {owner}.{name} {version}\n".encode()
    return _tar_gz([
        (f"{top}", None),
        (f"{top}/meta", None),
        (f"{top}/meta/main.yml", meta),
        (f"{top}/tasks", None),
        (f"{top}/tasks/main.yml", tasks),
    ])


class Fixtures:
    """Every answer the server gives, keyed by path, built once."""

    def __init__(self):
        self.artifacts = {}
        self.versions = {}
        for fqcn, versions in COLLECTIONS.items():
            namespace, name = fqcn.split(".")
            for version, dependencies in versions.items():
                filename = f"{namespace}-{name}-{version}.tar.gz"
                data = collection_artifact(fqcn, version, dependencies)
                self.artifacts[f"/api/v3/collections/{namespace}/{name}/artifacts/{filename}"] = data
                self.versions[(namespace, name, version)] = (filename, data, dependencies)
        self.roles = {}
        for number, (key, versions) in enumerate(sorted(ROLES.items()), start=1):
            owner, name = key.split(".")
            self.roles[(owner, name)] = (number, versions)
            for version in versions:
                self.artifacts[f"/roles/{owner}/{name}/{version}.tar.gz"] = role_archive(owner, name, version)

    def answer(self, url, host):
        """(status, content type, body) for a GET of `url` sent to `host`, the `Host` header: a role
        version's `download_url` has to be absolute."""
        parsed = urlparse(url)
        path, query = parsed.path, parse_qs(parsed.query)
        if path in self.artifacts:
            return 200, "application/gzip", self.artifacts[path]
        parts = [p for p in path.split("/") if p]
        body = self._json(parts, query, host)
        if body is None:
            return 404, "application/json", _json({"detail": "Not found."})
        return 200, "application/json", _json(body)

    def _json(self, parts, query, host):
        if parts == ["api"]:
            return {"available_versions": {"v1": "v1/", "v3": "v3/"}, "current_version": "v1", "description": "Volant's fake Galaxy"}
        if parts[:3] == ["api", "v3", "collections"] and len(parts) >= 5:
            return self._collection(parts[3], parts[4], parts[5:], query)
        if parts == ["api", "v1", "roles"]:
            return self._role_search(query)
        if parts[:3] == ["api", "v1", "roles"] and len(parts) == 5 and parts[4] == "versions":
            return self._role_versions(parts[3], host)
        return None

    def _collection(self, namespace, name, rest, query):
        fqcn = f"{namespace}.{name}"
        if fqcn not in COLLECTIONS:
            return None
        versions = list(COLLECTIONS[fqcn])
        href = f"/api/v3/collections/{namespace}/{name}/"
        if not rest:
            return {
                "href": href,
                "namespace": namespace,
                "name": name,
                "highest_version": {"href": f"{href}versions/{versions[-1]}/", "version": versions[-1]},
                "versions_url": f"{href}versions/",
                "created_at": DATE,
                "updated_at": DATE,
                "deprecated": False,
            }
        if rest == ["versions"]:
            offset = int(query.get("offset", ["0"])[0])
            page = versions[offset:offset + PAGE]
            following = offset + PAGE
            return {
                "meta": {"count": len(versions)},
                "links": {
                    "first": f"{href}versions/?limit={PAGE}&offset=0",
                    "previous": None if offset == 0 else f"{href}versions/?limit={PAGE}&offset={max(0, offset - PAGE)}",
                    "next": f"{href}versions/?limit={PAGE}&offset={following}" if following < len(versions) else None,
                    "last": f"{href}versions/?limit={PAGE}&offset={(len(versions) - 1) // PAGE * PAGE}",
                },
                "data": [{"version": v, "href": f"{href}versions/{v}/", "created_at": DATE, "updated_at": DATE} for v in page],
            }
        if len(rest) == 2 and rest[0] == "versions" and (namespace, name, rest[1]) in self.versions:
            version = rest[1]
            filename, data, dependencies = self.versions[(namespace, name, version)]
            return {
                "href": f"{href}versions/{version}/",
                "namespace": {"name": namespace},
                "collection": {"name": name, "href": href},
                "name": name,
                "version": version,
                "download_url": f"{href}artifacts/{filename}",
                "artifact": {"filename": filename, "sha256": _sha256(data), "size": len(data)},
                "metadata": {"dependencies": dependencies},
                "signatures": [],
                "created_at": DATE,
                "updated_at": DATE,
            }
        return None

    def _role_search(self, query):
        owner = query.get("owner__username", [""])[0]
        name = query.get("name", [""])[0]
        if (owner, name) not in self.roles:
            return {"count": 0, "next": None, "previous": None, "results": []}
        number, versions = self.roles[(owner, name)]
        return {"count": 1, "next": None, "previous": None, "results": [{
            "id": number,
            "name": name,
            "namespace": owner,
            "github_user": owner,
            "github_repo": f"ansible-role-{name}",
            "github_branch": "main",
            "description": f"The {owner}.{name} fixture.",
            "summary_fields": {"namespace": {"name": owner}, "versions": [{"name": v} for v in reversed(versions)]},
            "created": DATE,
            "modified": DATE,
        }]}

    def _role_versions(self, number, host):
        for (owner, name), (known, versions) in self.roles.items():
            if str(known) == number:
                results = [{
                    "id": i,
                    "name": v,
                    "version": v,
                    "download_url": f"http://{host}/roles/{owner}/{name}/{v}.tar.gz",
                    "created": DATE,
                    "modified": DATE,
                } for i, v in enumerate(versions, start=1)]
                return {"count": len(results), "next_link": None, "previous_link": None, "results": results}
        return None


class Bound(http.server.ThreadingHTTPServer):
    """The standard server without the reverse lookup of its own address `HTTPServer.server_bind`
    makes, which takes seconds on some machines."""

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


class Server:
    """The fake server on 127.0.0.1 and a free port, in a thread of this process."""

    def __init__(self):
        fixtures = Fixtures()
        self.log = []
        log = self.log

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self):
                if self.path == "/_log":
                    status, kind, body = 200, "application/json", _json(log)
                else:
                    log.append(f"GET {self.path}")
                    status, kind, body = fixtures.answer(self.path, self.headers["Host"])
                self.send_response(status)
                self.send_header("Content-Type", kind)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.httpd = Bound(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()


if __name__ == "__main__":
    with Server() as server:
        print(server.port, flush=True)
        sys.stdin.read()
