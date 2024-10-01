#!/bin/env python3

import time
import socket

# create an INET, STREAMing socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
# now connect to the web server on port 80 - the normal http port
s.connect(("192.168.2.94", 9000))

print("Connected!")

while True:
    for r in b"rgb":
        b = r.to_bytes(1, 'little')
        print(f"send: {r} = {b}")
        sent = s.send(b)
        if sent == 0:
            raise RuntimeError("socket connection broken")
        time.sleep(1)