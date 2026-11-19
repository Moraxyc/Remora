"""Talks to Remora like a client.

session.py BASE_URL SIGN_KEY_FILE session
    Plays a host and a viewer up to the point where a session starts, and the owner, who
    confirms the host's login with the password in $REMORA_OWNER_PASSWORD. Prints, as JSON, the
    user token, the host's device id, and the TURN server the viewer was given.
session.py BASE_URL SIGN_KEY_FILE devices TOKEN
    Prints the ids of the user's devices, as JSON.
"""

import hashlib
import hmac
import json
import os
import sys
import urllib.error
import urllib.request

from websockets.sync.client import connect

BASE = sys.argv[1]
with open(sys.argv[2], "rb") as key_file:
    SIGN_KEY = key_file.read().rstrip(b"\n")


def call(method, path, body=None, token=None, device=None, sign=True):
    """Calls the REST API like a client does; returns (code, data)."""
    data = b"" if body is None else json.dumps(body).encode()
    headers = {}
    if body is not None:
        headers["content-type"] = "application/json"
    if device:
        headers["x-param-device-id"] = device
    if token:
        headers["authorization"] = f"Bearer {token}"
    if sign:
        params = sorted(
            (name.lower(), value)
            for name, value in headers.items()
            if name.lower().startswith("x-param-")
        )
        message = method.encode() + path.encode()
        message += "&".join(f"{name}={value}" for name, value in params).encode()
        message += data
        headers["x-param-sign"] = hmac.new(SIGN_KEY, message, hashlib.sha256).hexdigest()
    request = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request) as response:
            envelope = json.load(response)
    except urllib.error.HTTPError as error:
        envelope = json.load(error)
    return envelope["code"], envelope["data"]


def ok(*args, **kwargs):
    code, data = call(*args, **kwargs)
    assert code == 0, (args[1], code)
    return data


def browse(method, path, body=None, cookie=None, csrf=None):
    """Calls the web app's API like the app does; returns (status, headers, JSON body)."""
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(BASE + path, data=data, method=method)
    if body is not None:
        request.add_header("content-type", "application/json")
    if cookie:
        request.add_header("cookie", cookie)
    if csrf:
        request.add_header("x-csrf-token", csrf)
    try:
        response = urllib.request.urlopen(request)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        text = response.read().decode()
        return response.status, response.headers, json.loads(text) if text else None


def confirm(qr_url):
    """Opens the QR code's page as the owner, signs in and confirms the login."""
    assert qr_url.startswith(BASE + "/login/"), "the QR code opens a page on Remora"
    with urllib.request.urlopen(qr_url) as page:
        assert b"<script" in page.read(), "the web app"
    login = "/api/web/logins/" + qr_url.rsplit("/", 1)[1]
    assert browse("GET", login)[0] == 200
    password = os.environ["REMORA_OWNER_PASSWORD"]
    assert browse("POST", "/api/web/session", {"password": "wrong"})[0] == 401
    status, headers, _ = browse("POST", "/api/web/session", {"password": password})
    assert status == 204, status
    cookie = headers["set-cookie"].split(";")[0]
    _, _, session = browse("GET", "/api/web/session", cookie=cookie)
    csrf = session["signed_in"]["csrf"]
    status, _, _ = browse("POST", login, {"decision": "confirm"}, cookie, csrf)
    assert status == 204, status


def open_socket(room_token):
    url = BASE.replace("http", "ws", 1) + "/socket.io/?EIO=4&transport=websocket"
    socket = connect(url, additional_headers={"X-NRD-AUTH": room_token})
    assert socket.recv().startswith("0"), "Engine.IO open"
    socket.send("40")
    assert socket.recv().startswith("40"), "Socket.IO connect"
    return socket


def recv(socket, prefix):
    """The next text frame, answering pings, which must start with `prefix`."""
    while True:
        frame = socket.recv(timeout=10)
        if frame == "2":
            socket.send("3")
            continue
        assert isinstance(frame, str) and frame.startswith(prefix), (prefix, frame[:40])
        return frame[len(prefix) :]


def session():
    assert call("POST", "/api/v1/qrcode/gen/login", sign=False)[0] == 1002, "unsigned calls fail"

    device = ok("POST", "/api/v1/device/windows/init", {"name": "vm", "client_id": "vm-host"})[
        "device_id"
    ]
    qr = ok("POST", "/api/v1/qrcode/gen/login", device=device)
    login = {"token": qr["status_query_ticket"], "qrcode_jump_url": qr["qrcode_jump_url"]}
    assert call("POST", "/api/v1/login/by_qrcode", login)[0] == 1190, "not confirmed yet"
    confirm(qr["qrcode_jump_url"])
    assert ok("POST", "/api/v1/qrcode/login/status", {**login, "login_status": 2})["status"] == 4
    user = ok("POST", "/api/v1/login/by_qrcode", login)["token"]

    room = ok("POST", "/api/v1/room/create", {}, token=user, device=device)
    host = open_socket(room["token"])
    host.send('421["room_info"]')
    recv(host, "431")
    recv(host, '42["bmsg_push"')

    joined = ok(
        "POST", f"/api/v1/room/join/by_device/{device}", {"force_join": False}, token=user
    )
    viewer = open_socket(joined["token"])
    viewer.send('451-5["control",{"app_data":{"_placeholder":true,"num":0}}]')
    viewer.send(b"\x08\x01")  # ConnectOptions { capture_type: CT_DESKTOP }
    outcome, grant = json.loads(recv(viewer, "435"))
    assert outcome == "success", grant
    recv(host, '451-["be-controlled"')

    turn = next(server for server in grant["iceServers"] if "username" in server)
    return {"token": user, "device": device, "force_relay": grant["force_relay"], "turn": turn}


def devices(token):
    listed = ok("GET", "/api/v1/device/list", token=token)["my_binded_devices"]
    return [entry["device_id"] for entry in listed]


match sys.argv[3:]:
    case ["session"]:
        print(json.dumps(session()))
    case ["devices", token]:
        print(json.dumps(devices(token)))
    case _:
        sys.exit(__doc__)
