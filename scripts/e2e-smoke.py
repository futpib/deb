#!/usr/bin/env python3

import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import gi
from PIL import ImageChops, ImageGrab

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi


class SmokeFailure(RuntimeError):
    pass


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
            name = accessible.get_name()
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

    def raise_windows(self):
        result = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--pid", str(self.process.pid)],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        for window in result.stdout.split():
            self.xdotool("windowraise", window)
            subprocess.run(
                ["xdotool", "windowfocus", window],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

    def move_pointer(self, accessible):
        rectangle = self.rectangle(accessible)
        x = rectangle.x + rectangle.width // 2
        y = rectangle.y + rectangle.height // 2
        self.raise_windows()
        self.xdotool("mousemove", str(x), str(y))
        location = self.xdotool("getmouselocation", "--shell", capture=True)
        coordinates = dict(
            line.split("=", 1) for line in location.splitlines() if "=" in line
        )
        actual_x = int(coordinates.get("X", -1))
        actual_y = int(coordinates.get("Y", -1))
        if abs(actual_x - x) > 2 or abs(actual_y - y) > 2:
            raise SmokeFailure(
                "XTEST pointer warp was rejected "
                f"(wanted {x},{y}, reached {actual_x},{actual_y}); "
                "run the E2E test on a native Xorg display"
            )
        return x, y

    def click(self, accessible_id):
        accessible = self.wait_for_id(accessible_id)
        self.move_pointer(accessible)
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

    def surface_statistics(self, accessible_id):
        surface = self.wait_for_id(accessible_id)
        rectangle = self.rectangle(surface)
        image = self.capture().crop(
            (
                rectangle.x,
                rectangle.y,
                rectangle.x + rectangle.width,
                rectangle.y + rectangle.height,
            )
        )
        pixels = list(image.convert("RGB").get_flattened_data())
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
            print("deb-e2e: clicking New Firefox tab through XTEST", flush=True)
            driver.click("browser.new.firefox.1")
            driver.wait_for_id("browser.tab.default.2")
            print("deb-e2e: typing the Firefox URL through XTEST", flush=True)
            driver.type_address("browser.address.1", "deb://new-tab/#deb-smoke")
            driver.wait_for_name("browser.status.1", "Gecko")
            print("deb-e2e: sampling the Firefox surface", flush=True)
            firefox_marker, firefox_variants = driver.wait_for_surface(
                "browser.surface.1", "Firefox"
            )

            print("deb-e2e: clicking the Chromium tab through XTEST", flush=True)
            driver.click("browser.tab.default.1")
            driver.wait_for_name("browser.status.1", "Chromium")
            print("deb-e2e: sampling the retained Chromium surface", flush=True)
            chromium_marker, chromium_variants = driver.wait_for_surface(
                "browser.surface.1", "Chromium"
            )
            print("deb-e2e: hovering the real tab tooltip through XTEST", flush=True)
            changed_pixels = driver.verify_tooltip_overlay(
                "browser.tab.default.1", "browser.surface.1"
            )
            print(
                "deb-smoke: PASS: external AT-SPI selectors and XTEST input drove "
                "Chromium and Firefox direct surfaces "
                f"(Chromium {chromium_variants} colors/{chromium_marker} marker pixels, "
                f"Firefox {firefox_variants} colors/{firefox_marker} marker pixels, "
                f"tooltip changed {changed_pixels} pixels)"
            )
            return 0
        except Exception as error:
            driver.save_failure_artifacts()
            print(f"deb-smoke: FAIL: {error}", file=sys.stderr)
            return 1
        finally:
            stop_process(process)


if __name__ == "__main__":
    raise SystemExit(main())
