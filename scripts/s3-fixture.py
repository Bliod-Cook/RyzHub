#!/usr/bin/env python3
"""Local S3 wire server for tests.

Implements the object and multipart-upload calls the server's object store client makes. AWS
botocore independently recomputes every SigV4 signature over the headers the client signed, so
a request with a wrong key, a tampered body or an unsigned header is rejected with 403.
Presigned URLs (query signatures, as handed to clients for direct part uploads) are checked
the same way, including their expiry.
"""
import hashlib
import hmac
import os
import uuid
from datetime import datetime, timezone
from email.utils import formatdate
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch
from urllib.parse import parse_qs, parse_qsl, urlencode, urlsplit

from botocore.auth import S3SigV4Auth, S3SigV4QueryAuth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

CREDENTIALS = Credentials("local-s3-access", "local-s3-secret-key")
REGION = "us-east-1"

objects = {}  # path -> bytes
uploads = {}  # upload id -> (path, {part number: bytes})


def etag(data):
    return '"%s"' % hashlib.md5(data).hexdigest()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def presigned(self):
        """Recompute a query signature with botocore over the URL without it."""
        try:
            url = urlsplit(self.path)
            pairs = parse_qsl(url.query, keep_blank_values=True)
            given = dict(pairs)
            stamp = datetime.strptime(given["X-Amz-Date"], "%Y%m%dT%H%M%SZ").replace(
                tzinfo=timezone.utc
            )
            expires = int(given["X-Amz-Expires"])
            if (datetime.now(timezone.utc) - stamp).total_seconds() > expires:
                return False
            if given["X-Amz-SignedHeaders"] != "host":
                return False
            rest = [(k, v) for k, v in pairs if not k.startswith("X-Amz-")]
            request = AWSRequest(
                method=self.command,
                url="http://" + self.headers["host"] + url.path + ("?" + urlencode(rest) if rest else ""),
                headers={"host": self.headers["host"]},
            )
            with patch("botocore.auth.get_current_datetime", return_value=stamp):
                S3SigV4QueryAuth(CREDENTIALS, "s3", REGION, expires=expires).add_auth(request)
            expected = dict(parse_qsl(urlsplit(request.url).query))["X-Amz-Signature"]
            return hmac.compare_digest(expected, given["X-Amz-Signature"])
        except (IndexError, KeyError, TypeError, ValueError):
            return False

    def authorized(self, body):
        """Recompute the signature with botocore from exactly the headers the client signed."""
        if "X-Amz-Signature=" in self.path:
            return self.presigned()
        if self.headers.get("x-amz-content-sha256") != hashlib.sha256(body).hexdigest():
            return False
        try:
            authorization = self.headers["authorization"]
            signed = authorization.split("SignedHeaders=")[1].split(",")[0].split(";")
            stamp = datetime.strptime(self.headers["x-amz-date"], "%Y%m%dT%H%M%SZ")
            request = AWSRequest(
                method=self.command,
                url="http://" + self.headers["host"] + self.path,
                data=body,
                headers={name: self.headers[name] for name in signed},
            )
            with patch(
                "botocore.auth.get_current_datetime",
                return_value=stamp.replace(tzinfo=timezone.utc),
            ):
                S3SigV4Auth(CREDENTIALS, "s3", REGION).add_auth(request)
            return hmac.compare_digest(request.headers["Authorization"], authorization)
        except (IndexError, KeyError, TypeError, ValueError):
            return False

    def reply(self, status, body=b"", headers=None):
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def xml(self, body):
        self.reply(200, body.encode(), {"Content-Type": "application/xml"})

    def handle_request(self):
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        if not self.authorized(body):
            return self.reply(403)
        url = urlsplit(self.path)
        path, query = url.path, parse_qs(url.query, keep_blank_values=True)
        upload = query.get("uploadId", [None])[0]

        if self.command == "POST" and "uploads" in query:
            upload = uuid.uuid4().hex
            uploads[upload] = (path, {})
            return self.xml(
                "<InitiateMultipartUploadResult><UploadId>%s</UploadId>"
                "</InitiateMultipartUploadResult>" % upload
            )
        if upload is not None:
            if upload not in uploads or uploads[upload][0] != path:
                return self.reply(404)
            parts = uploads[upload][1]
            if self.command == "PUT":
                parts[int(query["partNumber"][0])] = body
                return self.reply(200, headers={"ETag": etag(body)})
            if self.command == "POST":
                data = b"".join(parts[n] for n in sorted(parts))
                objects[path] = data
                del uploads[upload]
                return self.xml(
                    "<CompleteMultipartUploadResult><ETag>%s</ETag>"
                    "</CompleteMultipartUploadResult>" % etag(data)
                )
            if self.command == "DELETE":
                del uploads[upload]
                return self.reply(204)

        if self.command == "POST":
            return self.reply(400)
        if self.command == "PUT":
            objects[path] = body
            return self.reply(200, headers={"ETag": etag(body)})
        if self.command == "DELETE":
            objects.pop(path, None)
            return self.reply(204)
        if path not in objects:
            return self.reply(404)
        data = objects[path]
        headers = {"ETag": etag(data), "Last-Modified": formatdate(usegmt=True)}
        if self.command == "HEAD":
            self.send_response(200)
            for name, value in headers.items():
                self.send_header(name, value)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            return
        return self.reply(200, data, headers)

    do_GET = do_HEAD = do_PUT = do_POST = do_DELETE = handle_request


port = int(os.environ.get("RYZHUB_TEST_S3_PORT", "59000"))
ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
