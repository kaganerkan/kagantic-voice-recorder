#!/usr/bin/env python3
"""Inspect Windows release PE headers without executing them (Python stdlib only)."""
import argparse
import struct
import sys
import tempfile
import unittest
from pathlib import Path


def subsystem(path):
    data = Path(path).read_bytes()
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError(f"{path}: missing/truncated DOS header")
    offset = struct.unpack_from("<I", data, 0x3C)[0]
    if offset < 64 or offset + 24 > len(data) or data[offset:offset + 4] != b"PE\0\0":
        raise ValueError(f"{path}: missing/truncated PE header")
    machine, _, _, _, _, optional_size, characteristics = struct.unpack_from("<HHIIIHH", data, offset + 4)
    optional = offset + 24
    if machine != 0x8664:
        raise ValueError(f"{path}: expected x86-64 machine 0x8664, got {machine:#x}")
    if not characteristics & 0x0002 or characteristics & 0x2000:
        raise ValueError(f"{path}: expected executable, not DLL")
    if optional_size < 112 or optional + optional_size > len(data):
        raise ValueError(f"{path}: missing/truncated PE optional header")
    if struct.unpack_from("<H", data, optional)[0] != 0x20B:
        raise ValueError(f"{path}: expected PE32+ optional header")
    return struct.unpack_from("<H", data, optional + 68)[0]


def check_pair(cli, gui):
    for path, expected in ((cli, 3), (gui, 2)):
        actual = subsystem(path)
        if actual != expected:
            raise ValueError(f"{path}: Subsystem {actual}, expected {expected}")
        print(f"{path}: x86-64 PE32+ Subsystem {actual} (static inspection only)")


def fixture(value):
    # Header-only fixture, never represented as a runnable Windows executable.
    data = bytearray(64 + 24 + 240)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 64)
    data[64:68] = b"PE\0\0"
    struct.pack_into("<HHIIIHH", data, 68, 0x8664, 0, 0, 0, 0, 240, 2)
    struct.pack_into("<H", data, 88, 0x20B)
    struct.pack_into("<H", data, 88 + 68, value)
    return data


class PeChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.cli = Path(self.directory.name) / "kvr.exe"
        self.gui = Path(self.directory.name) / "kvr-gui.exe"
        self.cli.write_bytes(fixture(3))
        self.gui.write_bytes(fixture(2))

    def test_correct_pair(self):
        check_pair(self.cli, self.gui)

    def test_gui_console_rejected(self):
        self.gui.write_bytes(fixture(3))
        with self.assertRaisesRegex(ValueError, "Subsystem 3, expected 2"):
            check_pair(self.cli, self.gui)

    def test_cli_gui_rejected(self):
        self.cli.write_bytes(fixture(2))
        with self.assertRaisesRegex(ValueError, "Subsystem 2, expected 3"):
            check_pair(self.cli, self.gui)

    def test_malformed_headers_rejected(self):
        for data in (b"", b"MZ", fixture(2)[:64], fixture(2)[:-1]):
            with self.subTest(length=len(data)):
                self.gui.write_bytes(data)
                with self.assertRaises(ValueError):
                    subsystem(self.gui)

    def test_machine_magic_offsets_and_dll_rejected(self):
        for offset, encoding, value in ((68, "<H", 0x14C), (88, "<H", 0x10B),
                                        (0x3C, "<I", 0xFFFFFFFF), (84, "<H", 70),
                                        (86, "<H", 0x2002)):
            with self.subTest(offset=offset):
                data = fixture(2)
                struct.pack_into(encoding, data, offset, value)
                self.gui.write_bytes(data)
                with self.assertRaises(ValueError):
                    subsystem(self.gui)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--gui", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PeChecks))
        return 0 if result.wasSuccessful() else 1
    if args.cli is None or args.gui is None:
        parser.error("--cli and --gui are required unless --self-test is used")
    try:
        check_pair(args.cli, args.gui)
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
