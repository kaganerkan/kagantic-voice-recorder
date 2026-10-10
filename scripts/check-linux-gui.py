#!/usr/bin/env python3
"""Run the packaged GUI on X11 and verify its OS-facing logo and desktop identity.

Requires Pillow, xwininfo/xprop, and Xvfb unless --display selects an existing
X11 display. Optional screenshots require ImageMagick's import command.
This is an actual GUI smoke run, not a source/metadata assertion.
"""
import argparse
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import struct
import tempfile
import time

from PIL import Image

APP_ID = "com.github.kaganerkan.KaganticVoiceRecorder"


def stop_process(process):
    if process is not None and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=10)


def run(command, env):
    return subprocess.check_output(command, env=env, text=True, stderr=subprocess.STDOUT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("appimage", type=Path)
    parser.add_argument("--display", help="Use an existing X11 display instead of an isolated Xvfb")
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    appimage = args.appimage.resolve(strict=True)
    logo = Image.open(Path(__file__).resolve().parents[1] / "assets/pixel-art-logo.png").convert("RGBA")
    expected = [((a << 24) | (r << 16) | (g << 8) | b)
                for r, g, b, a in struct.iter_unpack("4B", logo.tobytes())]
    server = app = None
    with tempfile.TemporaryDirectory(prefix="kvr-gui-smoke-") as temporary:
        root = Path(temporary)
        env = os.environ.copy()
        env.pop("WAYLAND_DISPLAY", None)
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   WINIT_UNIX_BACKEND="x11", LIBGL_ALWAYS_SOFTWARE="1")
        try:
            if args.display:
                env["DISPLAY"] = args.display
            else:
                read_fd, write_fd = os.pipe()
                try:
                    server = subprocess.Popen(
                        ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1024x960x24", "-nolisten", "tcp"],
                        pass_fds=(write_fd,), stdout=subprocess.DEVNULL,
                        stderr=subprocess.PIPE, start_new_session=True)
                    os.close(write_fd)
                    write_fd = None
                    if not select.select([read_fd], [], [], 20)[0]:
                        raise RuntimeError("Xvfb did not become ready")
                    display = os.read(read_fd, 64).decode().strip()
                    if not display.isdigit():
                        raise RuntimeError("Xvfb failed to allocate an X11 display")
                    env["DISPLAY"] = ":" + display
                finally:
                    os.close(read_fd)
                    if write_fd is not None:
                        os.close(write_fd)
            with (root / "gui.log").open("wb") as log:
                app = subprocess.Popen([str(appimage), "--appimage-extract-and-run"],
                                       env=env, cwd=root, stdout=log, stderr=log, start_new_session=True)
                deadline = time.monotonic() + 40
                window = None
                while time.monotonic() < deadline:
                    if app.poll() is not None:
                        raise RuntimeError("GUI exited before rendering: " + (root / "gui.log").read_text(errors="replace"))
                    if args.display:
                        clients = run(["xprop", "-root", "_NET_CLIENT_LIST"], env)
                        for candidate in re.findall(r'0x[0-9a-fA-F]+', clients):
                            properties = run(["xprop", "-id", candidate, "WM_NAME", "_NET_WM_PID"], env)
                            pid = re.search(r'_NET_WM_PID.*=\s*(\d+)', properties)
                            if '"Kagantic Voice Recorder"' in properties and pid:
                                if os.getpgid(int(pid.group(1))) == app.pid:
                                    window = candidate
                                    break
                    else:
                        tree = run(["xwininfo", "-root", "-tree"], env)
                        match = re.search(r'(0x[0-9a-fA-F]+)\s+"Kagantic Voice Recorder"', tree)
                        if match:
                            window = match.group(1)
                    if window:
                        break
                    time.sleep(0.2)
                if window is None:
                    raise RuntimeError("GUI window did not appear: " + (root / "gui.log").read_text(errors="replace"))
                identity = run(["xprop", "-id", window, "WM_CLASS"], env)
                if APP_ID not in identity:
                    raise RuntimeError("Unexpected GUI desktop identity: " + identity)
                property_data = run(["xprop", "-id", window, "-notype", "-f", "_NET_WM_ICON", "32c", "_NET_WM_ICON"], env)
                if "=" not in property_data:
                    raise RuntimeError("GUI did not expose _NET_WM_ICON")
                values = [int(value) for value in re.findall(r'\d+', property_data.split("=", 1)[1])]
                offset = 0
                matched = False
                while offset + 2 <= len(values):
                    width, height = values[offset:offset + 2]
                    end = offset + 2 + width * height
                    if width <= 0 or height <= 0 or end > len(values):
                        raise RuntimeError("Malformed GUI _NET_WM_ICON")
                    pixels = values[offset + 2:end]
                    if (width, height) == logo.size:
                        matched = all(actual == reference or (actual >> 24 == reference >> 24 == 0)
                                      for actual, reference in zip(pixels, expected))
                        if matched:
                            break
                    offset = end
                if not matched:
                    raise RuntimeError("Running GUI icon does not match the original logo pixels")
                if args.screenshot:
                    args.screenshot.parent.mkdir(parents=True, exist_ok=True)
                    if args.screenshot.exists():
                        raise FileExistsError("Refusing to overwrite screenshot: " + str(args.screenshot))
                    time.sleep(1)
                    subprocess.run(["import", "-display", env["DISPLAY"], "-window", window,
                                    str(args.screenshot.resolve())], check=True, env=env)
                if app.poll() is not None:
                    raise RuntimeError("GUI exited during icon inspection")
                print("Actual AppImage GUI rendered; WM_CLASS and 32x32 OS icon match the original logo")
        finally:
            stop_process(app)
            stop_process(server)


if __name__ == "__main__":
    main()
