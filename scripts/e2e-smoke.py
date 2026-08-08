#!/usr/bin/env python3

import argparse
import ctypes
import ctypes.util
import fcntl
import hashlib
import http.server
import os
import secrets
import signal
import subprocess
import struct
import sys
import threading
import time
from pathlib import Path

import gi
from PIL import ImageChops, ImageGrab

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi


class SmokeFailure(RuntimeError):
    pass


class XFixesCursorImage(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_short),
        ("y", ctypes.c_short),
        ("width", ctypes.c_ushort),
        ("height", ctypes.c_ushort),
        ("xhot", ctypes.c_ushort),
        ("yhot", ctypes.c_ushort),
        ("cursor_serial", ctypes.c_ulong),
        ("pixels", ctypes.POINTER(ctypes.c_ulong)),
        ("atom", ctypes.c_ulong),
        ("name", ctypes.c_char_p),
    ]


class XCursorProbe:
    def __init__(self):
        x11_name = ctypes.util.find_library("X11")
        xfixes_name = ctypes.util.find_library("Xfixes")
        if not x11_name or not xfixes_name:
            raise SmokeFailure("X11 cursor inspection libraries are unavailable")
        self.x11 = ctypes.CDLL(x11_name)
        self.xfixes = ctypes.CDLL(xfixes_name)
        self.x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x11.XOpenDisplay.restype = ctypes.c_void_p
        self.x11.XFree.argtypes = [ctypes.c_void_p]
        self.x11.XFree.restype = ctypes.c_int
        self.xfixes.XFixesGetCursorImage.argtypes = [ctypes.c_void_p]
        self.xfixes.XFixesGetCursorImage.restype = ctypes.POINTER(XFixesCursorImage)
        self.display = self.x11.XOpenDisplay(None)
        if not self.display:
            raise SmokeFailure("XOpenDisplay failed while preparing cursor inspection")

    def snapshot(self):
        pointer = self.xfixes.XFixesGetCursorImage(self.display)
        if not pointer:
            raise SmokeFailure("XFixesGetCursorImage returned no cursor")
        try:
            image = pointer.contents
            byte_length = (
                image.width * image.height * ctypes.sizeof(ctypes.c_ulong)
            )
            pixels = ctypes.string_at(image.pixels, byte_length)
            return (
                image.width,
                image.height,
                image.xhot,
                image.yhot,
                hashlib.sha256(pixels).hexdigest(),
                image.name.decode("utf-8", errors="replace") if image.name else "",
            )
        finally:
            self.x11.XFree(pointer)


class InputId(ctypes.Structure):
    _fields_ = [
        ("bustype", ctypes.c_ushort),
        ("vendor", ctypes.c_ushort),
        ("product", ctypes.c_ushort),
        ("version", ctypes.c_ushort),
    ]


class UInputSetup(ctypes.Structure):
    _fields_ = [
        ("id", InputId),
        ("name", ctypes.c_char * 80),
        ("ff_effects_max", ctypes.c_uint),
    ]


class InputAbsInfo(ctypes.Structure):
    _fields_ = [
        ("value", ctypes.c_int),
        ("minimum", ctypes.c_int),
        ("maximum", ctypes.c_int),
        ("fuzz", ctypes.c_int),
        ("flat", ctypes.c_int),
        ("resolution", ctypes.c_int),
    ]


class UInputAbsSetup(ctypes.Structure):
    _fields_ = [
        ("code", ctypes.c_ushort),
        ("padding", ctypes.c_ushort),
        ("absinfo", InputAbsInfo),
    ]


class VirtualTouchscreen:
    EV_SYN = 0
    EV_KEY = 1
    EV_ABS = 3
    SYN_REPORT = 0
    BTN_TOUCH = 0x14A
    ABS_X = 0
    ABS_Y = 1
    ABS_MT_SLOT = 0x2F
    ABS_MT_POSITION_X = 0x35
    ABS_MT_POSITION_Y = 0x36
    ABS_MT_TRACKING_ID = 0x39
    ABS_MT_PRESSURE = 0x3A
    INPUT_PROP_DIRECT = 1

    @staticmethod
    def ioctl(direction, number, size=0):
        return (direction << 30) | (size << 16) | (ord("U") << 8) | number

    UI_DEV_CREATE = ioctl.__func__(0, 1)
    UI_DEV_DESTROY = ioctl.__func__(0, 2)
    UI_DEV_SETUP = ioctl.__func__(1, 3, ctypes.sizeof(UInputSetup))
    UI_ABS_SETUP = ioctl.__func__(1, 4, ctypes.sizeof(UInputAbsSetup))
    UI_SET_EVBIT = ioctl.__func__(1, 100, ctypes.sizeof(ctypes.c_int))
    UI_SET_KEYBIT = ioctl.__func__(1, 101, ctypes.sizeof(ctypes.c_int))
    UI_SET_PROPBIT = ioctl.__func__(1, 110, ctypes.sizeof(ctypes.c_int))

    def __init__(self, width, height):
        try:
            self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        except OSError as error:
            raise SmokeFailure(
                "raw touch E2E needs write access to /dev/uinput; "
                "grant it to the test user or run the smoke test with suitable privileges"
            ) from error
        self.width = width
        self.height = height
        try:
            self.set_bit(self.UI_SET_EVBIT, self.EV_KEY)
            self.set_bit(self.UI_SET_EVBIT, self.EV_ABS)
            self.set_bit(self.UI_SET_KEYBIT, self.BTN_TOUCH)
            self.set_bit(self.UI_SET_PROPBIT, self.INPUT_PROP_DIRECT)
            self.set_axis(self.ABS_X, width - 1)
            self.set_axis(self.ABS_Y, height - 1)
            self.set_axis(self.ABS_MT_SLOT, 9)
            self.set_axis(self.ABS_MT_POSITION_X, width - 1)
            self.set_axis(self.ABS_MT_POSITION_Y, height - 1)
            self.set_axis(self.ABS_MT_TRACKING_ID, 65535)
            self.set_axis(self.ABS_MT_PRESSURE, 1024)
            setup = UInputSetup(
                id=InputId(bustype=3, vendor=0x1D6B, product=0xD3B, version=1),
                name=b"deb-e2e-raw-touchscreen",
                ff_effects_max=0,
            )
            fcntl.ioctl(self.fd, self.UI_DEV_SETUP, bytes(setup))
            fcntl.ioctl(self.fd, self.UI_DEV_CREATE)
        except Exception:
            os.close(self.fd)
            self.fd = None
            raise
        time.sleep(1.0)

    def set_bit(self, operation, value):
        fcntl.ioctl(self.fd, operation, value)

    def set_axis(self, code, maximum):
        setup = UInputAbsSetup(
            code=code,
            absinfo=InputAbsInfo(
                value=0,
                minimum=0,
                maximum=maximum,
                fuzz=0,
                flat=0,
                resolution=1,
            ),
        )
        fcntl.ioctl(self.fd, self.UI_ABS_SETUP, bytes(setup))

    def emit(self, event_type, code, value):
        os.write(self.fd, struct.pack("@llHHi", 0, 0, event_type, code, value))

    def sync(self):
        self.emit(self.EV_SYN, self.SYN_REPORT, 0)

    def begin(self, slot, tracking_id, x, y, primary=False):
        self.emit(self.EV_ABS, self.ABS_MT_SLOT, slot)
        self.emit(self.EV_ABS, self.ABS_MT_TRACKING_ID, tracking_id)
        self.emit(self.EV_ABS, self.ABS_MT_POSITION_X, x)
        self.emit(self.EV_ABS, self.ABS_MT_POSITION_Y, y)
        self.emit(self.EV_ABS, self.ABS_MT_PRESSURE, 768)
        if primary:
            self.emit(self.EV_ABS, self.ABS_X, x)
            self.emit(self.EV_ABS, self.ABS_Y, y)
            self.emit(self.EV_KEY, self.BTN_TOUCH, 1)
        self.sync()

    def move_pair(self, first, second):
        for slot, (x, y) in enumerate((first, second)):
            self.emit(self.EV_ABS, self.ABS_MT_SLOT, slot)
            self.emit(self.EV_ABS, self.ABS_MT_POSITION_X, x)
            self.emit(self.EV_ABS, self.ABS_MT_POSITION_Y, y)
            self.emit(self.EV_ABS, self.ABS_MT_PRESSURE, 768)
            if slot == 0:
                self.emit(self.EV_ABS, self.ABS_X, x)
                self.emit(self.EV_ABS, self.ABS_Y, y)
        self.sync()

    def pinch(self, x, y):
        self.begin(0, 100, x - 24, y, primary=True)
        self.begin(1, 101, x + 24, y)
        for distance in (40, 60, 80, 100, 80, 60, 40, 24):
            self.move_pair((x - distance, y), (x + distance, y))
            time.sleep(0.04)
        self.emit(self.EV_ABS, self.ABS_MT_SLOT, 1)
        self.emit(self.EV_ABS, self.ABS_MT_TRACKING_ID, -1)
        self.emit(self.EV_ABS, self.ABS_MT_SLOT, 0)
        self.emit(self.EV_ABS, self.ABS_MT_TRACKING_ID, -1)
        self.emit(self.EV_KEY, self.BTN_TOUCH, 0)
        self.sync()

    def close(self):
        if self.fd is None:
            return
        try:
            fcntl.ioctl(self.fd, self.UI_DEV_DESTROY)
        finally:
            os.close(self.fd)
            self.fd = None


class E2ESite:
    def __init__(self):
        self.cookie_name = "deb_e2e_cross_engine"
        self.token = secrets.token_hex(12)
        site = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                route = self.path.split("?", 1)[0]
                if route == f"/set/{site.token}":
                    mode = "set"
                elif route == f"/observe/{site.token}":
                    mode = "observe"
                else:
                    self.send_error(404)
                    return
                body = site.page(mode).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, _format, *_arguments):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def page(self, mode):
        engine = "chromium" if mode == "set" else "firefox"
        if mode == "set":
            action = f"""
document.cookie = `${{cookieName}}=${{token}}; Path=/; SameSite=Lax`;
if (hasCookie()) {{
  document.title = `deb-e2e chromium set ${{token}}`;
  status.textContent = "Chromium set the synchronization cookie";
  document.body.classList.add("complete");
}} else {{
  document.title = "deb-e2e Chromium cookie set failed";
  status.textContent = "Chromium did not retain the synchronization cookie";
}}
"""
        else:
            action = """
function observeCookie() {
  if (hasCookie()) {
    document.title = `deb-e2e firefox synced ${token}`;
    status.textContent = "Firefox received the Chromium cookie";
    document.body.classList.add("complete");
  } else {
    document.title = "deb-e2e waiting for synchronized cookie";
    window.setTimeout(observeCookie, 100);
  }
}
observeCookie();
"""
        return f"""<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>deb-e2e loading</title>
<style>
html, body {{ width: 100%; height: 100%; margin: 0; }}
body {{
  display: grid;
  place-items: center;
  touch-action: none;
  color: white;
  background: linear-gradient(135deg, #172033, #51246d 48%, #a23b72);
  font: 24px sans-serif;
}}
body.complete {{ background: linear-gradient(135deg, #123020, #176b3a, #49a078); }}
#marker {{ position: fixed; top: 12px; left: 12px; width: 96px; height: 64px; background: #00ff00; }}
#status {{ padding: 32px; border: 3px solid #f4d35e; background: #101522; }}
#click-target {{
  position: fixed;
  z-index: 2;
  left: 30%;
  top: 72%;
  width: 40%;
  height: 16%;
  border: 4px solid white;
  color: white;
  background: #2457c5;
  font: 700 24px sans-serif;
  cursor: pointer;
}}
#text-target {{
  position: fixed;
  z-index: 2;
  left: 30%;
  top: 18%;
  width: 40%;
  height: 10%;
  box-sizing: border-box;
  padding: 12px;
  font: 24px sans-serif;
}}
body.chromium.clicked #click-target {{ color: #101522; background: #ff00ff; }}
body.firefox.clicked #click-target {{ color: #101522; background: #00ffff; }}
body.touched #marker {{ background: #ff8c00; }}
</style>
</head>
<body class="{engine}">
<div id="marker"></div>
<div id="status">Waiting for the cross-engine cookie</div>
<input id="text-target" type="text" value="Hover this text input">
<button id="click-target" type="button">Click this page target</button>
<script>
const cookieName = {self.cookie_name!r};
const token = {self.token!r};
const status = document.getElementById("status");
const clickTarget = document.getElementById("click-target");
const clickTitle = {f"deb-e2e {engine} click received {self.token}"!r};
const touchTitle = {f"deb-e2e {engine} raw gesture received {self.token}"!r};
let initialTouchDistance = null;
let rawGestureMoved = false;
let rawGestureTrusted = true;
function touchDistance(touches) {{
  return Math.hypot(
    touches[0].clientX - touches[1].clientX,
    touches[0].clientY - touches[1].clientY
  );
}}
function hasCookie() {{
  return document.cookie.split("; ").includes(`${{cookieName}}=${{token}}`);
}}
{action}
clickTarget.addEventListener("click", event => {{
  if (!event.isTrusted) {{
    document.title = `deb-e2e {engine} rejected untrusted click ${{token}}`;
    return;
  }}
  document.title = clickTitle;
  status.textContent = "The page received a trusted browser click";
  document.body.classList.add("clicked");
}});
document.addEventListener("touchstart", event => {{
  rawGestureTrusted = rawGestureTrusted && event.isTrusted;
  if (event.touches.length === 2) {{
    initialTouchDistance = touchDistance(event.touches);
  }}
  event.preventDefault();
}}, {{ passive: false }});
document.addEventListener("touchmove", event => {{
  rawGestureTrusted = rawGestureTrusted && event.isTrusted;
  if (event.touches.length === 2 && initialTouchDistance !== null) {{
    rawGestureMoved = rawGestureMoved ||
      Math.abs(touchDistance(event.touches) - initialTouchDistance) > 40;
  }}
  event.preventDefault();
}}, {{ passive: false }});
document.addEventListener("touchend", event => {{
  rawGestureTrusted = rawGestureTrusted && event.isTrusted;
  if (event.touches.length === 0 && rawGestureMoved) {{
    document.title = rawGestureTrusted
      ? touchTitle
      : `deb-e2e {engine} rejected untrusted touch ${{token}}`;
    status.textContent = "The page received a trusted raw two-contact gesture";
    document.body.classList.add("touched");
  }}
  event.preventDefault();
}}, {{ passive: false }});
</script>
</body>
</html>
"""

    @property
    def origin(self):
        host, port = self.server.server_address
        return f"http://{host}:{port}"

    def start(self):
        self.thread.start()

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def descendants(accessible):
    yield accessible
    try:
        child_count = accessible.get_child_count()
    except Exception:
        return
    for index in range(child_count):
        try:
            child = accessible.get_child_at_index(index)
        except Exception:
            continue
        if child is not None:
            yield from descendants(child)


class Driver:
    def __init__(self, process, artifact_directory, log_path):
        self.process = process
        self.artifact_directory = artifact_directory
        self.log_path = log_path
        self.application = None
        self.cursor_probe = XCursorProbe()

    def assert_no_process_failures(self, context):
        try:
            output = self.log_path.read_text(encoding="utf-8", errors="replace")
        except FileNotFoundError:
            return
        failures = [
            line
            for line in output.splitlines()
            if "deb: failure:" in line or "deb: tab controller failed:" in line
        ]
        if failures:
            details = "\n".join(failures[-8:])
            raise SmokeFailure(f"process failure during {context}:\n{details}")

    def wait_until(self, description, probe, timeout=20.0):
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
            self.assert_no_process_failures(f"waiting for {description}")
            if self.process.poll() is not None:
                raise SmokeFailure(
                    f"deb exited with status {self.process.returncode} while waiting for {description}"
                )
            try:
                value = probe()
                if value:
                    return value
            except Exception as error:
                last_error = error
            time.sleep(0.1)
        detail = f": {last_error}" if last_error is not None else ""
        raise SmokeFailure(f"timed out waiting for {description}{detail}")

    def find_application(self):
        desktop = Atspi.get_desktop(0)
        for index in range(desktop.get_child_count()):
            application = desktop.get_child_at_index(index)
            try:
                if application.get_process_id() == self.process.pid:
                    return application
            except Exception:
                continue
        return None

    def find_id(self, accessible_id):
        if self.application is None:
            return None
        for accessible in descendants(self.application):
            try:
                if accessible.get_accessible_id() == accessible_id:
                    return accessible
            except Exception:
                continue
        return None

    def wait_for_id(self, accessible_id, timeout=20.0):
        return self.wait_until(
            f"accessible object {accessible_id}",
            lambda: self.find_id(accessible_id),
            timeout,
        )

    def wait_for_name(self, accessible_id, expected, timeout=30.0):
        def probe():
            accessible = self.find_id(accessible_id)
            if accessible is None:
                return None
            name = accessible.get_name() or ""
            return accessible if expected in name else None

        return self.wait_until(
            f"{accessible_id} to contain {expected!r}", probe, timeout
        )

    def wait_for_text(self, accessible_id, expected, timeout=30.0):
        def probe():
            accessible = self.find_id(accessible_id)
            if accessible is None:
                return None
            text = accessible.get_text_iface()
            if text is None:
                return None
            actual = Atspi.Text.get_text(text, 0, -1)
            return accessible if actual == expected else None

        return self.wait_until(
            f"{accessible_id} text to equal {expected!r}", probe, timeout
        )

    def wait_for_selected(self, accessible_id, timeout=20.0):
        def probe():
            accessible = self.find_id(accessible_id)
            if accessible is None:
                return None
            states = accessible.get_state_set()
            return accessible if states.contains(Atspi.StateType.SELECTED) else None

        return self.wait_until(f"{accessible_id} to become selected", probe, timeout)

    def wait_for_one_selected(self, accessible_ids, timeout=20.0):
        def probe():
            selected = []
            for accessible_id in accessible_ids:
                accessible = self.find_id(accessible_id)
                if accessible is None:
                    return None
                if accessible.get_state_set().contains(Atspi.StateType.SELECTED):
                    selected.append(accessible_id)
            return selected[0] if len(selected) == 1 else None

        return self.wait_until("exactly one stress tab to be selected", probe, timeout)

    def wait_for_process_quiet(self, description, duration):
        deadline = time.monotonic() + duration
        return self.wait_until(
            description,
            lambda: time.monotonic() >= deadline,
            timeout=duration + 5.0,
        )

    def find_tooltip(self):
        if self.application is None:
            return None
        for accessible in descendants(self.application):
            try:
                if accessible.get_role_name() == "tool tip":
                    return accessible
            except Exception:
                continue
        return None

    def wait_for_descendant(self, accessible_id, ancestor_id, timeout=20.0):
        def probe():
            accessible = self.find_id(accessible_id)
            while accessible is not None:
                try:
                    if accessible.get_accessible_id() == ancestor_id:
                        return accessible
                    accessible = accessible.get_parent()
                except Exception:
                    return None
            return None

        return self.wait_until(
            f"{accessible_id} to move below {ancestor_id}", probe, timeout
        )

    def wait_for_descendant_id_count(
        self, ancestor_id, accessible_id_prefix, expected, timeout=20.0
    ):
        def probe():
            ancestor = self.find_id(ancestor_id)
            if ancestor is None:
                return None
            count = 0
            for accessible in descendants(ancestor):
                try:
                    if accessible.get_accessible_id().startswith(accessible_id_prefix):
                        count += 1
                except Exception:
                    continue
            return ancestor if count == expected else None

        return self.wait_until(
            f"{ancestor_id} to contain {expected} {accessible_id_prefix} objects",
            probe,
            timeout,
        )

    @staticmethod
    def rectangle(accessible):
        component = accessible.get_component_iface()
        if component is None:
            raise SmokeFailure(
                f"{accessible.get_accessible_id()} has no accessible component geometry"
            )
        rectangle = component.get_extents(Atspi.CoordType.SCREEN)
        if rectangle.width <= 0 or rectangle.height <= 0:
            raise SmokeFailure(
                f"{accessible.get_accessible_id()} has empty screen geometry"
            )
        return rectangle

    def xdotool(self, *arguments, capture=False):
        result = subprocess.run(
            ["xdotool", *arguments],
            check=True,
            text=True,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        return result.stdout.strip() if capture else ""

    @staticmethod
    def shell_values(output):
        return dict(
            line.split("=", 1) for line in output.splitlines() if "=" in line
        )

    def visible_windows(self):
        result = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--pid", str(self.process.pid)],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        return result.stdout.split()

    def top_level(self, accessible):
        current = accessible
        while current is not None:
            try:
                parent = current.get_parent()
            except Exception:
                break
            if parent is None or parent == self.application:
                return current
            current = parent
        return accessible

    def focus_accessible_window(self, accessible):
        top_level = self.top_level(accessible)
        top_rectangle = self.rectangle(top_level)
        try:
            top_name = top_level.get_name() or ""
        except Exception:
            top_name = ""
        candidates = []
        for window in self.visible_windows():
            try:
                values = self.shell_values(
                    self.xdotool("getwindowgeometry", "--shell", window, capture=True)
                )
                rectangle = (
                    int(values["X"]),
                    int(values["Y"]),
                    int(values["WIDTH"]),
                    int(values["HEIGHT"]),
                )
                window_name = self.xdotool("getwindowname", window, capture=True)
            except Exception:
                continue
            score = sum(
                abs(first - second)
                for first, second in zip(
                    rectangle,
                    (
                        top_rectangle.x,
                        top_rectangle.y,
                        top_rectangle.width,
                        top_rectangle.height,
                    ),
                )
            )
            candidates.append((window_name != top_name, score, window))
        if not candidates:
            raise SmokeFailure("deb has no visible X11 window for an accessible control")
        _, _, window = min(candidates)
        self.xdotool("windowraise", window)
        subprocess.run(
            ["xdotool", "windowfocus", window],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return window

    def wait_for_actionable(self, accessible_id):
        def probe():
            accessible = self.find_id(accessible_id)
            if accessible is None:
                return None
            self.rectangle(accessible)
            return accessible

        return self.wait_until(f"actionable control {accessible_id}", probe)

    def move_pointer_to(self, x, y):
        self.xdotool("mousemove", str(x), str(y))
        location = self.xdotool("getmouselocation", "--shell", capture=True)
        coordinates = self.shell_values(location)
        actual_x = int(coordinates.get("X", -1))
        actual_y = int(coordinates.get("Y", -1))
        if abs(actual_x - x) > 2 or abs(actual_y - y) > 2:
            raise SmokeFailure(
                "XTEST pointer warp was rejected "
                f"(wanted {x},{y}, reached {actual_x},{actual_y}); "
                "run the E2E test on a native Xorg display"
            )
        return x, y

    def move_pointer(self, accessible, focus_window=True):
        rectangle = self.rectangle(accessible)
        x = rectangle.x + rectangle.width // 2
        y = rectangle.y + rectangle.height // 2
        if focus_window:
            self.focus_accessible_window(accessible)
        return self.move_pointer_to(x, y)

    def click(self, accessible_id, focus_window=True):
        accessible = self.wait_for_actionable(accessible_id)
        self.move_pointer(accessible, focus_window)
        self.xdotool("click", "1")

    def click_button(self, accessible_id, button, focus_window=True):
        accessible = self.wait_for_actionable(accessible_id)
        self.move_pointer(accessible, focus_window)
        self.xdotool("click", str(button))

    def drag(self, source_id, target_id):
        source = self.wait_for_actionable(source_id)
        target = self.wait_for_actionable(target_id)
        self.focus_accessible_window(source)
        source_rectangle = self.rectangle(source)
        target_rectangle = self.rectangle(target)
        source_x = source_rectangle.x + source_rectangle.width // 2
        source_y = source_rectangle.y + source_rectangle.height // 2
        target_x = target_rectangle.x + target_rectangle.width // 2
        target_y = target_rectangle.y + target_rectangle.height // 2
        self.move_pointer_to(source_x, source_y)
        self.xdotool("mousedown", "1")
        time.sleep(0.1)
        self.move_pointer_to((source_x + target_x) // 2, (source_y + target_y) // 2)
        time.sleep(0.1)
        self.move_pointer_to(target_x, target_y)
        time.sleep(0.1)
        self.xdotool("mouseup", "1")

    def arrange_windows_side_by_side(self, left_control_id, right_control_id):
        left = self.wait_for_actionable(left_control_id)
        right = self.wait_for_actionable(right_control_id)
        left_window = self.focus_accessible_window(left)
        right_window = self.focus_accessible_window(right)
        display_width, display_height = (
            int(value)
            for value in self.xdotool("getdisplaygeometry", capture=True).split()
        )
        gap = 20
        left_width = max(900, (display_width - gap) // 2)
        right_width = max(760, display_width - gap - left_width)
        if left_width + gap + right_width > display_width:
            raise SmokeFailure(
                "the E2E display is too narrow to expose both production windows"
            )
        height = max(560, display_height - 80)
        self.xdotool("windowsize", left_window, str(left_width), str(height))
        self.xdotool("windowmove", left_window, "0", "0")
        self.xdotool("windowsize", right_window, str(right_width), str(height))
        self.xdotool("windowmove", right_window, str(left_width + gap), "0")

        def separated():
            left_rectangle = self.rectangle(self.find_id(left_control_id))
            right_rectangle = self.rectangle(self.find_id(right_control_id))
            return (
                left_rectangle.x + left_rectangle.width
                <= right_rectangle.x + gap // 2
            )

        self.wait_until("the two browser windows to be side by side", separated)

    def wait_for_missing_id(self, accessible_id, timeout=20.0):
        return self.wait_until(
            f"accessible object {accessible_id} to disappear",
            lambda: self.find_id(accessible_id) is None,
            timeout,
        )

    def wait_for_horizontal_order(self, left_id, right_id, timeout=20.0):
        def probe():
            left = self.find_id(left_id)
            right = self.find_id(right_id)
            if left is None or right is None:
                return None
            return left if self.rectangle(left).x < self.rectangle(right).x else None

        return self.wait_until(
            f"{left_id} to appear left of {right_id}", probe, timeout
        )

    def click_surface(self, accessible_id, x_fraction, y_fraction):
        if not 0.0 < x_fraction < 1.0 or not 0.0 < y_fraction < 1.0:
            raise SmokeFailure("surface click fractions must be inside the viewport")
        surface = self.wait_for_id(accessible_id)
        rectangle = self.rectangle(surface)
        self.focus_accessible_window(surface)
        x = rectangle.x + round(rectangle.width * x_fraction)
        y = rectangle.y + round(rectangle.height * y_fraction)
        self.move_pointer_to(x, y)
        self.xdotool("click", "1")

    def raw_pinch_surface(self, accessible_id, touchscreen):
        surface = self.wait_for_id(accessible_id)
        rectangle = self.rectangle(surface)
        self.focus_accessible_window(surface)
        touchscreen.pinch(
            rectangle.x + rectangle.width // 2,
            rectangle.y + rectangle.height // 2,
        )

    def move_over_surface(self, accessible_id, x_fraction, y_fraction):
        if not 0.0 < x_fraction < 1.0 or not 0.0 < y_fraction < 1.0:
            raise SmokeFailure("surface hover fractions must be inside the viewport")
        surface = self.wait_for_id(accessible_id)
        rectangle = self.rectangle(surface)
        self.focus_accessible_window(surface)
        return self.move_pointer_to(
            rectangle.x + round(rectangle.width * x_fraction),
            rectangle.y + round(rectangle.height * y_fraction),
        )

    def verify_page_cursors(self, accessible_id, engine):
        self.move_over_surface(accessible_id, 0.1, 0.5)
        time.sleep(0.2)
        default_cursor = self.cursor_probe.snapshot()

        self.move_over_surface(accessible_id, 0.5, 0.23)
        text_cursor = self.wait_until(
            f"the {engine} text-input cursor",
            lambda: (
                cursor
                if (cursor := self.cursor_probe.snapshot())[:5]
                != default_cursor[:5]
                else None
            ),
        )

        self.move_over_surface(accessible_id, 0.5, 0.8)
        pointer_cursor = self.wait_until(
            f"the {engine} CSS pointer cursor",
            lambda: (
                cursor
                if (cursor := self.cursor_probe.snapshot())[:5]
                not in (default_cursor[:5], text_cursor[:5])
                else None
            ),
        )
        return tuple(cursor[5] or cursor[4][:12] for cursor in (
            default_cursor,
            text_cursor,
            pointer_cursor,
        ))

    def send_shortcut(self, accessible_id, sequence):
        accessible = self.wait_for_id(accessible_id)
        self.focus_accessible_window(accessible)
        self.xdotool("key", "--clearmodifiers", sequence)

    def send_repeated_shortcut(self, accessible_id, sequence, repeat, repeat_delay):
        accessible = self.wait_for_id(accessible_id)
        self.focus_accessible_window(accessible)
        self.xdotool(
            "key",
            "--clearmodifiers",
            "--repeat",
            str(repeat),
            "--repeat-delay",
            str(repeat_delay),
            sequence,
        )

    def type_address(self, accessible_id, address):
        self.click(accessible_id)
        self.xdotool("key", "--clearmodifiers", "ctrl+a")
        self.xdotool("type", "--clearmodifiers", "--delay", "1", address)
        accessible = self.wait_for_id(accessible_id)
        text = accessible.get_text_iface()
        if text is None:
            raise SmokeFailure(f"{accessible_id} has no accessible text interface")
        actual = Atspi.Text.get_text(text, 0, -1)
        if actual != address:
            raise SmokeFailure(
                f"XTEST typed {actual!r} into {accessible_id}, expected {address!r}"
            )
        self.xdotool("key", "--clearmodifiers", "Return")

    def capture(self):
        return ImageGrab.grab()

    def surface_image(self, accessible_id):
        surface = self.wait_for_id(accessible_id)
        self.focus_accessible_window(surface)
        rectangle = self.rectangle(surface)
        return self.capture().crop(
            (
                rectangle.x,
                rectangle.y,
                rectangle.x + rectangle.width,
                rectangle.y + rectangle.height,
            )
        ).convert("RGB")

    def surface_statistics(self, accessible_id):
        pixels = list(self.surface_image(accessible_id).get_flattened_data())
        marker_pixels = sum(
            blue < 16
            and (
                (red < 16 and green > 239)
                or (red > 239 and 120 < green < 160)
            )
            for red, green, blue in pixels
        )
        variants = len(set(pixels[::97]))
        return marker_pixels, variants

    def wait_for_surface(self, accessible_id, engine):
        def probe():
            marker_pixels, variants = self.surface_statistics(accessible_id)
            if marker_pixels >= 128 and variants >= 8:
                return marker_pixels, variants
            return None

        return self.wait_until(
            f"a composed {engine} browser frame", probe, timeout=30.0
        )

    def wait_for_page_click(self, accessible_id, engine):
        if engine == "Chromium":
            target = (255, 0, 255)
        elif engine == "Firefox":
            target = (0, 255, 255)
        else:
            raise SmokeFailure(f"unknown page-click engine {engine}")

        def probe():
            pixels = self.surface_image(accessible_id).get_flattened_data()
            marker_pixels = sum(
                abs(pixel[0] - target[0]) < 16
                and abs(pixel[1] - target[1]) < 16
                and abs(pixel[2] - target[2]) < 16
                for pixel in pixels
            )
            return marker_pixels if marker_pixels >= 128 else None

        return self.wait_until(
            f"the {engine} page's trusted-click marker", probe, timeout=20.0
        )

    @staticmethod
    def intersection(first, second):
        left = max(first.x, second.x)
        top = max(first.y, second.y)
        right = min(first.x + first.width, second.x + second.width)
        bottom = min(first.y + first.height, second.y + second.height)
        if left >= right or top >= bottom:
            return None
        return left, top, right, bottom

    def verify_tooltip_overlay(self, tab_id, surface_id):
        surface = self.wait_for_id(surface_id)
        self.move_pointer(surface)
        self.wait_until("the previous tooltip to close", lambda: not self.find_tooltip())
        before = self.capture()
        tab = self.wait_for_id(tab_id)
        self.move_pointer(tab)
        tooltip = self.wait_until("the tab tooltip", self.find_tooltip)
        tooltip_rectangle = self.rectangle(tooltip)
        surface_rectangle = self.rectangle(surface)
        overlap = self.intersection(tooltip_rectangle, surface_rectangle)
        if overlap is None:
            raise SmokeFailure("the tab tooltip does not overlap the browser surface")
        last_capture = {}

        def probe_composition():
            after = self.capture()
            changed = ImageChops.difference(
                before.crop(overlap), after.crop(overlap)
            )
            changed_pixels = sum(
                any(channel != 0 for channel in pixel)
                for pixel in changed.get_flattened_data()
            )
            last_capture["after"] = after
            last_capture["difference"] = changed
            return changed_pixels if changed_pixels >= 32 else None

        try:
            return self.wait_until(
                "the Qt tooltip to change composed browser pixels",
                probe_composition,
                timeout=5.0,
            )
        except SmokeFailure as error:
            self.artifact_directory.mkdir(parents=True, exist_ok=True)
            before.save(self.artifact_directory / "tooltip-before.png")
            if after := last_capture.get("after"):
                after.save(self.artifact_directory / "tooltip-after.png")
            if changed := last_capture.get("difference"):
                changed.save(self.artifact_directory / "tooltip-difference.png")
            raise SmokeFailure(
                "the Qt tooltip did not change composed browser pixels "
                f"within {overlap}"
            ) from error

    def dump_accessibility_tree(self):
        if self.application is None:
            return
        lines = []

        def append(accessible, depth=0):
            try:
                lines.append(
                    f"{'  ' * depth}{accessible.get_accessible_id()!r} "
                    f"{accessible.get_name()!r} {accessible.get_role_name()}"
                )
                child_count = accessible.get_child_count()
            except Exception as error:
                lines.append(f"{'  ' * depth}<unavailable: {error}>")
                return
            for index in range(child_count):
                try:
                    append(accessible.get_child_at_index(index), depth + 1)
                except Exception as error:
                    lines.append(f"{'  ' * (depth + 1)}<unavailable: {error}>")

        append(self.application)
        (self.artifact_directory / "accessibility-tree.txt").write_text(
            "\n".join(lines) + "\n", encoding="utf-8"
        )

    def save_failure_artifacts(self):
        self.artifact_directory.mkdir(parents=True, exist_ok=True)
        try:
            self.capture().save(self.artifact_directory / "failure.png")
        except Exception:
            pass
        try:
            self.dump_accessibility_tree()
        except Exception:
            pass


def stop_process(process):
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=15)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def parse_arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--require-touch", action="store_true")
    return parser.parse_args()


def main():
    arguments = parse_arguments()
    arguments.artifacts.mkdir(parents=True, exist_ok=True)
    touchscreen = None
    if arguments.require_touch or os.access("/dev/uinput", os.W_OK):
        geometry = subprocess.run(
            ["xdotool", "getdisplaygeometry"],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout.split()
        touchscreen = VirtualTouchscreen(int(geometry[0]), int(geometry[1]))
    else:
        print(
            "deb-e2e: raw touch gesture coverage skipped because /dev/uinput is not writable",
            flush=True,
        )
    site = E2ESite()
    site.start()
    with arguments.log.open("wb") as log:
        process = subprocess.Popen(
            [str(arguments.binary)],
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        driver = Driver(process, arguments.artifacts, arguments.log)

        def terminate(_signal_number, _frame):
            raise SmokeFailure("the external E2E timeout expired")

        signal.signal(signal.SIGTERM, terminate)
        try:
            print("deb-e2e: waiting for the production accessibility tree", flush=True)
            driver.application = driver.wait_until(
                "deb to appear on AT-SPI", driver.find_application
            )
            driver.wait_for_name("browser.status.1", "Chromium")
            print("deb-e2e: clicking New Firefox tab through XTEST", flush=True)
            driver.click("browser.new-menu.1")
            driver.click("browser.new.firefox.1", focus_window=False)
            driver.wait_for_id("browser.tab.default.2")
            driver.wait_for_name("browser.status.1", "Gecko")
            driver.wait_for_name("browser.tab.default.2", "New tab · deb")
            driver.click("browser.tab.default.1")
            driver.wait_for_name("browser.status.1", "Chromium")
            chromium_initial_marker, chromium_initial_variants = driver.wait_for_surface(
                "browser.surface.1", "initial Chromium"
            )
            driver.click("browser.tab.default.2")
            driver.wait_for_name("browser.status.1", "Gecko")
            driver.type_address("browser.address.1", "deb://new-tab/#deb-smoke")
            firefox_internal_marker, firefox_internal_variants = driver.wait_for_surface(
                "browser.surface.1", "Firefox internal page"
            )

            print("deb-e2e: navigating Chromium through the real address field", flush=True)
            driver.click("browser.tab.default.1")
            driver.wait_for_name("browser.status.1", "Chromium")
            chromium_url = f"{site.origin}/set/{site.token}"
            driver.type_address("browser.address.1", chromium_url)
            driver.wait_for_name(
                "browser.tab.default.1", f"deb-e2e chromium set {site.token}"
            )
            chromium_marker, chromium_variants = driver.wait_for_surface(
                "browser.surface.1", "Chromium"
            )
            print("deb-e2e: checking Chromium page hover cursors", flush=True)
            chromium_cursors = driver.verify_page_cursors(
                "browser.surface.1", "Chromium"
            )
            if touchscreen is not None:
                print(
                    "deb-e2e: sending a raw two-contact Chromium gesture through uinput",
                    flush=True,
                )
                driver.raw_pinch_surface("browser.surface.1", touchscreen)
                driver.wait_for_name(
                    "browser.tab.default.1",
                    f"deb-e2e chromium raw gesture received {site.token}",
                )
                time.sleep(0.1)
            print("deb-e2e: clicking the Chromium page through XTEST", flush=True)
            driver.click_surface("browser.surface.1", 0.5, 0.8)
            driver.wait_for_name(
                "browser.tab.default.1",
                f"deb-e2e chromium click received {site.token}",
            )
            chromium_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Chromium"
            )
            print("deb-e2e: proving Qt composition over Chromium", flush=True)
            chromium_tooltip_pixels = driver.verify_tooltip_overlay(
                "browser.tab.default.1", "browser.surface.1"
            )

            print("deb-e2e: observing Chromium's cookie from Firefox", flush=True)
            driver.click("browser.tab.default.2")
            driver.wait_for_name("browser.status.1", "Gecko")
            driver.wait_for_text(
                "browser.address.1", "deb://new-tab/#deb-smoke"
            )
            firefox_url = f"{site.origin}/observe/{site.token}"
            driver.type_address("browser.address.1", firefox_url)
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox synced {site.token}",
                timeout=45.0,
            )
            firefox_marker, firefox_variants = driver.wait_for_surface(
                "browser.surface.1", "Firefox after cookie synchronization"
            )
            print("deb-e2e: checking Firefox page hover cursors", flush=True)
            firefox_cursors = driver.verify_page_cursors(
                "browser.surface.1", "Firefox"
            )
            if touchscreen is not None:
                print(
                    "deb-e2e: sending a raw two-contact Firefox gesture through uinput",
                    flush=True,
                )
                driver.raw_pinch_surface("browser.surface.1", touchscreen)
                driver.wait_for_name(
                    "browser.tab.default.2",
                    f"deb-e2e firefox raw gesture received {site.token}",
                )
                time.sleep(0.1)
            print("deb-e2e: clicking the Firefox page through XTEST", flush=True)
            driver.click_surface("browser.surface.1", 0.5, 0.8)
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox click received {site.token}",
            )
            firefox_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Firefox"
            )
            print("deb-e2e: proving Qt composition over Firefox", flush=True)
            firefox_tooltip_pixels = driver.verify_tooltip_overlay(
                "browser.tab.default.2", "browser.surface.1"
            )

            print("deb-e2e: switching retained tabs through their buttons", flush=True)
            driver.click("browser.tab.default.1")
            driver.wait_for_name("browser.status.1", "Chromium")
            driver.wait_for_text("browser.address.1", chromium_url)
            driver.wait_for_name(
                "browser.tab.default.1",
                f"deb-e2e chromium click received {site.token}",
            )
            retained_chromium_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Chromium"
            )
            retained_chromium_marker, retained_chromium_variants = (
                driver.wait_for_surface("browser.surface.1", "retained Chromium")
            )
            driver.click("browser.tab.default.2")
            driver.wait_for_name("browser.status.1", "Gecko")
            driver.wait_for_text("browser.address.1", firefox_url)
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox click received {site.token}",
            )
            retained_firefox_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Firefox"
            )
            retained_firefox_marker, retained_firefox_variants = driver.wait_for_surface(
                "browser.surface.1", "retained Firefox"
            )

            print("deb-e2e: reordering tabs through a real pointer drag", flush=True)
            driver.drag("browser.tab.default.1", "browser.tab.default.2")
            driver.wait_for_horizontal_order(
                "browser.tab.default.2", "browser.tab.default.1"
            )
            driver.click("browser.tab.default.1")
            driver.wait_for_name("browser.status.1", "Chromium")
            driver.click("browser.tab.default.2")
            driver.wait_for_name("browser.status.1", "Gecko")

            print("deb-e2e: switching tabs through both keyboard shortcuts", flush=True)
            driver.send_shortcut("browser.surface.1", "ctrl+shift+Tab")
            driver.wait_for_name("browser.status.1", "Chromium")
            driver.wait_for_text("browser.address.1", chromium_url)
            driver.wait_for_name(
                "browser.tab.default.1",
                f"deb-e2e chromium click received {site.token}",
            )
            shortcut_chromium_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Chromium"
            )
            driver.send_shortcut("browser.surface.1", "ctrl+Tab")
            driver.wait_for_name("browser.status.1", "Gecko")
            driver.wait_for_text("browser.address.1", firefox_url)
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox click received {site.token}",
            )
            shortcut_firefox_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Firefox"
            )

            print("deb-e2e: opening a second production window", flush=True)
            driver.click("browser.new-window.1")
            driver.wait_for_id("browser.view.2")
            driver.wait_for_id("browser.tab.default.3")
            driver.wait_for_name("browser.status.2", "Chromium")
            second_window_marker, second_window_variants = driver.wait_for_surface(
                "browser.surface.2", "second-window Chromium"
            )

            print(
                "deb-e2e: dragging the live Firefox tab into the second window",
                flush=True,
            )
            driver.arrange_windows_side_by_side("browser.view.1", "browser.view.2")
            driver.drag("browser.tab.default.2", "browser.tabs.2")
            driver.wait_for_descendant("browser.tab.default.2", "browser.view.2")
            driver.wait_for_name("browser.status.2", "Gecko")
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox click received {site.token}",
            )
            moved_firefox_marker, moved_firefox_variants = driver.wait_for_surface(
                "browser.surface.2", "moved Firefox"
            )
            moved_firefox_click_pixels = driver.wait_for_page_click(
                "browser.surface.2", "Firefox"
            )
            moved_tooltip_pixels = driver.verify_tooltip_overlay(
                "browser.tab.default.2", "browser.surface.2"
            )
            print("deb-e2e: closing the other tab with a middle click", flush=True)
            driver.click_button("browser.tab.default.3", 2)
            driver.wait_for_missing_id("browser.tab.default.3")
            driver.wait_for_descendant("browser.tab.default.2", "browser.view.2")
            print("deb-e2e: detaching Firefox through its real tab menu", flush=True)
            driver.click_button("browser.tab.default.2", 3)
            driver.click("browser.detach.default.2", focus_window=False)
            driver.wait_for_id("browser.view.3")
            driver.wait_for_descendant("browser.tab.default.2", "browser.view.3")
            driver.wait_for_descendant_id_count("browser.view.3", "browser.tab.", 1)
            driver.wait_for_name("browser.status.3", "Gecko")
            detached_firefox_marker, detached_firefox_variants = (
                driver.wait_for_surface("browser.surface.3", "detached Firefox")
            )
            driver.wait_for_id("browser.tab.default.4")
            driver.wait_for_descendant("browser.tab.default.4", "browser.view.2")
            driver.wait_for_name("browser.status.2", "Chromium")
            driver.wait_for_name("browser.status.1", "Chromium")
            driver.wait_for_text("browser.address.1", chromium_url)
            main_after_move_marker, main_after_move_variants = driver.wait_for_surface(
                "browser.surface.1", "main-window Chromium after the tab move"
            )
            main_after_move_click_pixels = driver.wait_for_page_click(
                "browser.surface.1", "Chromium"
            )

            print(
                "deb-e2e: stress-switching two live tabs from each engine",
                flush=True,
            )
            driver.click("browser.new.default.3")
            driver.wait_for_id("browser.tab.default.5")
            driver.wait_for_selected("browser.tab.default.5")
            driver.wait_for_name("browser.status.3", "Gecko")
            driver.click("browser.new-menu.3")
            driver.click("browser.new.chromium.3", focus_window=False)
            driver.wait_for_id("browser.tab.default.6")
            driver.wait_for_selected("browser.tab.default.6")
            driver.wait_for_name("browser.status.3", "Chromium")
            driver.click("browser.new.default.3")
            driver.wait_for_id("browser.tab.default.7")
            driver.wait_for_selected("browser.tab.default.7")
            driver.wait_for_name("browser.status.3", "Chromium")

            stress_tabs = (
                ("browser.tab.default.2", "Gecko"),
                ("browser.tab.default.6", "Chromium"),
                ("browser.tab.default.5", "Gecko"),
                ("browser.tab.default.7", "Chromium"),
            )
            for cycle in range(10):
                for tab_id, engine in stress_tabs:
                    driver.click(tab_id)
                    driver.wait_for_selected(tab_id)
                    driver.wait_for_name("browser.status.3", engine)
                    driver.assert_no_process_failures(
                        f"tab-switch stress cycle {cycle + 1} selecting {tab_id}"
                    )

            driver.send_shortcut("browser.surface.3", "ctrl+Tab")
            driver.wait_for_selected("browser.tab.default.2")
            driver.send_repeated_shortcut(
                "browser.surface.3", "ctrl+Tab", repeat=47, repeat_delay=20
            )
            driver.wait_for_process_quiet("rapid Ctrl+Tab processing", 2.0)
            selected_stress_tab = driver.wait_for_one_selected(
                tuple(tab_id for tab_id, _engine in stress_tabs)
            )
            selected_engine = dict(stress_tabs)[selected_stress_tab]
            driver.wait_for_name("browser.status.3", selected_engine)
            driver.assert_no_process_failures("rapid Ctrl+Tab stress")

            touch_summary = (
                " and trusted raw two-contact gestures"
                if touchscreen is not None
                else ""
            )
            print(
                "deb-smoke: PASS: external AT-SPI selectors and XTEST input drove "
                f"both engines, trusted page clicks{touch_summary}, tab buttons, drag reordering, cross-window dragging, middle-click closing, menu detaching, shortcuts, cookie sync, retained frames, three windows, and a four-tab dual-engine switch stress without process failures "
                f"(Chromium {chromium_variants} colors/{chromium_marker} marker pixels, "
                f"Firefox {firefox_variants} colors/{firefox_marker} marker pixels, "
                f"initial Chromium {chromium_initial_variants}/{chromium_initial_marker}, "
                f"Firefox internal page {firefox_internal_variants}/{firefox_internal_marker}, "
                f"Chromium cursors {chromium_cursors}, Firefox cursors {firefox_cursors}, "
                f"retained Chromium {retained_chromium_variants}/{retained_chromium_marker}, "
                f"retained Firefox {retained_firefox_variants}/{retained_firefox_marker}, "
                f"second window {second_window_variants}/{second_window_marker}, "
                f"detached Firefox {detached_firefox_variants}/{detached_firefox_marker}, "
                f"moved Firefox {moved_firefox_variants}/{moved_firefox_marker}, "
                f"main after move {main_after_move_variants}/{main_after_move_marker}, "
                f"page clicks {chromium_click_pixels}/{firefox_click_pixels} pixels, "
                f"retained clicks {retained_chromium_click_pixels}/{retained_firefox_click_pixels}, "
                f"shortcut clicks {shortcut_chromium_click_pixels}/{shortcut_firefox_click_pixels}, "
                f"moved/main clicks {moved_firefox_click_pixels}/{main_after_move_click_pixels}, "
                f"tooltips {chromium_tooltip_pixels}/{firefox_tooltip_pixels}/{moved_tooltip_pixels} pixels)"
            )
            return 0
        except Exception as error:
            driver.save_failure_artifacts()
            print(f"deb-smoke: FAIL: {error}", file=sys.stderr)
            return 1
        finally:
            stop_process(process)
            site.stop()
            if touchscreen is not None:
                touchscreen.close()


if __name__ == "__main__":
    raise SystemExit(main())
