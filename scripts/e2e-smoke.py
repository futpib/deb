#!/usr/bin/env python3

import argparse
import http.server
import os
import secrets
import signal
import subprocess
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
}}
body.clicked #click-target {{ color: #101522; background: #ff00ff; }}
</style>
</head>
<body>
<div id="marker"></div>
<div id="status">Waiting for the cross-engine cookie</div>
<button id="click-target" type="button">Click this page target</button>
<script>
const cookieName = {self.cookie_name!r};
const token = {self.token!r};
const status = document.getElementById("status");
const clickTarget = document.getElementById("click-target");
const clickTitle = {f"deb-e2e {engine} click received {self.token}"!r};
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
    def __init__(self, process, artifact_directory):
        self.process = process
        self.artifact_directory = artifact_directory
        self.application = None

    def wait_until(self, description, probe, timeout=20.0):
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
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
            red < 16 and green > 239 and blue < 16 for red, green, blue in pixels
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
        def probe():
            pixels = self.surface_image(accessible_id).get_flattened_data()
            marker_pixels = sum(
                red > 239 and green < 16 and blue > 239
                for red, green, blue in pixels
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
        after = self.capture()
        changed = ImageChops.difference(before.crop(overlap), after.crop(overlap))
        changed_pixels = sum(
            any(channel != 0 for channel in pixel)
            for pixel in changed.get_flattened_data()
        )
        if changed_pixels < 32:
            raise SmokeFailure("the Qt tooltip did not change composed browser pixels")
        return changed_pixels

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
    return parser.parse_args()


def main():
    arguments = parse_arguments()
    arguments.artifacts.mkdir(parents=True, exist_ok=True)
    site = E2ESite()
    site.start()
    with arguments.log.open("wb") as log:
        process = subprocess.Popen(
            [str(arguments.binary)],
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        driver = Driver(process, arguments.artifacts)

        def terminate(_signal_number, _frame):
            raise SmokeFailure("the external E2E timeout expired")

        signal.signal(signal.SIGTERM, terminate)
        try:
            print("deb-e2e: waiting for the production accessibility tree", flush=True)
            driver.application = driver.wait_until(
                "deb to appear on AT-SPI", driver.find_application
            )
            driver.wait_for_name("browser.status.1", "Chromium")
            chromium_initial_marker, chromium_initial_variants = driver.wait_for_surface(
                "browser.surface.1", "initial Chromium"
            )
            print("deb-e2e: clicking New Firefox tab through XTEST", flush=True)
            driver.click("browser.new.firefox.1")
            driver.wait_for_id("browser.tab.default.2")
            driver.wait_for_name("browser.status.1", "Gecko")
            driver.wait_for_name("browser.tab.default.2", "New tab · deb")
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

            print("deb-e2e: reselecting both retained inactive frames", flush=True)
            driver.click("browser.tab.default.1")
            driver.wait_for_name(
                "browser.tab.default.1",
                f"deb-e2e chromium click received {site.token}",
            )
            retained_chromium_marker, retained_chromium_variants = (
                driver.wait_for_surface("browser.surface.1", "retained Chromium")
            )
            driver.click("browser.tab.default.2")
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox click received {site.token}",
            )
            retained_firefox_marker, retained_firefox_variants = driver.wait_for_surface(
                "browser.surface.1", "retained Firefox"
            )

            print("deb-e2e: opening a second production window", flush=True)
            driver.click("browser.new-window.1")
            driver.wait_for_id("browser.view.2")
            driver.wait_for_id("browser.tab.default.3")
            driver.wait_for_name("browser.status.2", "Chromium")
            second_window_marker, second_window_variants = driver.wait_for_surface(
                "browser.surface.2", "second-window Chromium"
            )

            print("deb-e2e: moving the live Firefox tab into the second window", flush=True)
            driver.click("browser.move.1")
            driver.click("browser.move-target.2", focus_window=False)
            driver.wait_for_descendant("browser.tab.default.2", "browser.view.2")
            driver.wait_for_name("browser.status.2", "Gecko")
            driver.wait_for_name(
                "browser.tab.default.2",
                f"deb-e2e firefox click received {site.token}",
            )
            moved_firefox_marker, moved_firefox_variants = driver.wait_for_surface(
                "browser.surface.2", "moved Firefox"
            )
            moved_tooltip_pixels = driver.verify_tooltip_overlay(
                "browser.tab.default.2", "browser.surface.2"
            )
            driver.wait_for_name("browser.status.1", "Chromium")
            main_after_move_marker, main_after_move_variants = driver.wait_for_surface(
                "browser.surface.1", "main-window Chromium after the tab move"
            )

            print(
                "deb-smoke: PASS: external AT-SPI selectors and XTEST input drove "
                "both engines, trusted page clicks, cookie sync, retained frames, and two windows "
                f"(Chromium {chromium_variants} colors/{chromium_marker} marker pixels, "
                f"Firefox {firefox_variants} colors/{firefox_marker} marker pixels, "
                f"initial Chromium {chromium_initial_variants}/{chromium_initial_marker}, "
                f"Firefox internal page {firefox_internal_variants}/{firefox_internal_marker}, "
                f"retained Chromium {retained_chromium_variants}/{retained_chromium_marker}, "
                f"retained Firefox {retained_firefox_variants}/{retained_firefox_marker}, "
                f"second window {second_window_variants}/{second_window_marker}, "
                f"moved Firefox {moved_firefox_variants}/{moved_firefox_marker}, "
                f"main after move {main_after_move_variants}/{main_after_move_marker}, "
                f"page clicks {chromium_click_pixels}/{firefox_click_pixels} pixels, "
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


if __name__ == "__main__":
    raise SystemExit(main())
