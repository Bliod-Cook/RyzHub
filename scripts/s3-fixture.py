#!/usr/bin/env python3
"""Local S3 wire server. AWS botocore independently verifies every SigV4 request."""
import hashlib,hmac,os
from datetime import datetime,timezone
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from unittest.mock import patch
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials
objects={}
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def handle_object(self):
  data=self.rfile.read(int(self.headers.get('content-length','0')))
  if self.headers.get('x-amz-content-sha256')!=hashlib.sha256(data).hexdigest():self.send_error(403);return
  try:
   stamp=datetime.strptime(self.headers['x-amz-date'],'%Y%m%dT%H%M%SZ').replace(tzinfo=timezone.utc)
   request=AWSRequest(method=self.command,url='http://'+self.headers['host']+self.path,data=data,headers={n:self.headers[n] for n in ['host','x-amz-content-sha256','x-amz-date']})
   with patch('botocore.auth.get_current_datetime',return_value=stamp):S3SigV4Auth(Credentials('local-s3-access','local-s3-secret-key'),'s3','us-east-1').add_auth(request)
   assert hmac.compare_digest(request.headers['Authorization'],self.headers['authorization'])
  except (AssertionError,ValueError,KeyError,TypeError):self.send_error(403);return
  if self.command=='PUT':objects[self.path]=data;out=b''
  elif self.command=='DELETE':objects.pop(self.path,None);out=b''
  else:
   if self.path not in objects:self.send_error(404);return
   out=objects[self.path]
  self.send_response(200);self.send_header('Content-Length',str(len(out)));self.end_headers();self.wfile.write(out)
 do_PUT=handle_object
 do_GET=handle_object
 do_DELETE=handle_object
ThreadingHTTPServer(('127.0.0.1',int(os.environ.get('RYZHUB_TEST_S3_PORT','59000'))),Handler).serve_forever()
