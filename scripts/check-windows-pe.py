#!/usr/bin/env python3
"""Inspect Windows release PE headers without executing them (Python stdlib only)."""
import argparse
import struct
import sys
import tempfile
import unittest
from pathlib import Path

DEFAULT_ICON = Path(__file__).resolve().parents[1] / "assets/pixel-art-logo.ico"


def unpack(data, offset, encoding):
    if offset < 0 or offset + struct.calcsize(encoding) > len(data):
        raise ValueError("truncated/out-of-bounds PE or icon data")
    return struct.unpack_from(encoding, data, offset)


def ico_frames(data):
    reserved, kind, count = unpack(data, 0, "<HHH")
    if reserved != 0 or kind != 1 or count == 0:
        raise ValueError("invalid/empty canonical ICO")
    frames = []
    for index in range(count):
        entry = 6 + index * 16
        _, _, _, reserved, _, _, size, offset = unpack(data, entry, "<BBBBHHII")
        if reserved or not size or offset < 6 + count * 16 or offset + size > len(data):
            raise ValueError("invalid canonical ICO frame")
        frames.append((data[entry:entry + 12], data[offset:offset + size]))
    return frames


def icon_resources(path):
    subsystem(path)  # Validate the executable header first.
    data = Path(path).read_bytes()
    header, = unpack(data, 0x3C, "<I")
    optional = header + 24
    optional_size, = unpack(data, header + 20, "<H")
    count, = unpack(data, header + 6, "<H")
    if optional_size < 136 or unpack(data, optional + 108, "<I")[0] < 3:
        raise ValueError("no resource directory")
    rva, resource_size = unpack(data, optional + 128, "<II")
    if not rva or resource_size < 16:
        raise ValueError("no resource directory")
    sections = []
    for index in range(count):
        entry = optional + optional_size + index * 40
        _, address, size, start = unpack(data, entry + 8, "<IIII")
        sections.append((address, size, start))

    def file_offset(address, size):
        for virtual, raw_size, raw_offset in sections:
            delta = address - virtual
            if 0 <= delta and delta + size <= raw_size and raw_offset + delta + size <= len(data):
                return raw_offset + delta
        raise ValueError("resource RVA/size outside backed PE sections")

    base = file_offset(rva, resource_size)

    def relative(offset, size):
        if offset < 0 or offset + size > resource_size:
            raise ValueError("resource directory offset out of bounds")
        return base + offset

    def directory(offset):
        start = relative(offset, 16)
        named, numeric = unpack(data, start + 12, "<HH")
        relative(offset + 16, (named + numeric) * 8)
        entries = {}
        for index in range(named + numeric):
            identifier, target = unpack(data, start + 16 + index * 8, "<II")
            if identifier & 0x80000000:
                continue
            if identifier in entries:
                raise ValueError("duplicate numeric resource identifier")
            entries[identifier] = target
        return entries

    resources = {}
    for kind, target in directory(0).items():
        if kind not in (3, 14):  # RT_ICON / RT_GROUP_ICON
            continue
        if not target & 0x80000000:
            raise ValueError("resource type must reference a directory")
        resources[kind] = {}
        for identifier, languages_target in directory(target & 0x7FFFFFFF).items():
            if not languages_target & 0x80000000:
                raise ValueError("resource identifier must reference a language directory")
            languages = {}
            for language, leaf in directory(languages_target & 0x7FFFFFFF).items():
                if leaf & 0x80000000:
                    raise ValueError("resource language must reference data")
                address, size, _, _ = unpack(data, relative(leaf, 16), "<IIII")
                start = file_offset(address, size)
                languages[language] = data[start:start + size]
            resources[kind][identifier] = languages
    return resources


def check_icon(path, canonical):
    expected = ico_frames(canonical)
    resources = icon_resources(path)
    icons, groups = resources.get(3, {}), resources.get(14, {})
    if not icons or not groups:
        raise ValueError(f"{path}: missing ICON/GROUP_ICON resources")
    for languages in groups.values():
        if not languages:
            raise ValueError(f"{path}: empty GROUP_ICON resource")
        for language, group in languages.items():
            reserved, kind, count = unpack(group, 0, "<HHH")
            if (reserved, kind, count) != (0, 1, len(expected)) or len(group) != 6 + count * 14:
                raise ValueError(f"{path}: GROUP_ICON does not match canonical ICO")
            for index, (metadata, payload) in enumerate(expected):
                entry = 6 + index * 14
                identifier, = unpack(group, entry + 12, "<H")
                variants = icons.get(identifier, {})
                actual = variants.get(language, variants.get(0))
                if actual is None and len(variants) == 1:
                    actual = next(iter(variants.values()))
                if group[entry:entry + 12] != metadata or actual != payload:
                    raise ValueError(f"{path}: icon frame {index} does not match canonical ICO")
    print(f"{path}: native ICON/GROUP_ICON payloads match original-logo ICO")


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


def check_pair(cli, gui, icon=None):
    for path, expected in ((cli, 3), (gui, 2)):
        actual = subsystem(path)
        if actual != expected:
            raise ValueError(f"{path}: Subsystem {actual}, expected {expected}")
        if icon is not None:
            check_icon(path, icon)
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


def icon_fixture(value, canonical):
    """Real resource-tree layout in a non-executable, deterministic PE fixture."""
    data = fixture(value)
    frames = ico_frames(canonical)
    group = bytearray(struct.pack("<HHH", 0, 1, len(frames)))
    tree = {3: {}, 14: {1: {1033: group}}}
    for index, (metadata, payload) in enumerate(frames, 1):
        group.extend(metadata + struct.pack("<H", index))
        tree[3][index] = {1033: payload}
    section = bytearray()

    def append_directory(entries):
        offset = len(section)
        section.extend(bytes(16 + len(entries) * 8))
        struct.pack_into("<H", section, offset + 14, len(entries))
        for index, (identifier, child) in enumerate(sorted(entries.items())):
            if isinstance(child, dict):
                target = append_directory(child) | 0x80000000
            else:
                target = len(section)
                section.extend(bytes(16))
                start = len(section)
                section.extend(child)
                section.extend(bytes((-len(section)) % 4))
                struct.pack_into("<IIII", section, target, 0x1000 + start, len(child), 0, 0)
            struct.pack_into("<II", section, offset + 16 + index * 8, identifier, target)
        return offset

    append_directory(tree)
    struct.pack_into("<H", data, 70, 1)
    struct.pack_into("<I", data, 88 + 108, 16)
    struct.pack_into("<II", data, 88 + 128, 0x1000, len(section))
    data.extend(struct.pack("<8sIIIIIIHHI", b".rsrc\0\0\0", len(section), 0x1000,
                            len(section), 512, 0, 0, 0, 0, 0x40000040))
    data.extend(bytes(512 - len(data)))
    data.extend(section)
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

    def test_native_resource_frames_match_original_ico(self):
        canonical = DEFAULT_ICON.read_bytes()
        self.cli.write_bytes(icon_fixture(3, canonical))
        self.gui.write_bytes(icon_fixture(2, canonical))
        check_pair(self.cli, self.gui, canonical)

    def test_missing_icons_rejected(self):
        with self.assertRaisesRegex(ValueError, "no resource directory"):
            check_icon(self.gui, DEFAULT_ICON.read_bytes())

    def test_wrong_icon_pixels_rejected(self):
        canonical = DEFAULT_ICON.read_bytes()
        data = icon_fixture(2, canonical)
        payload = ico_frames(canonical)[0][1]
        offset = data.index(payload)
        data[offset + len(payload) - 1] ^= 1
        self.gui.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "does not match canonical"):
            check_icon(self.gui, canonical)

    def test_out_of_bounds_resource_directory_rejected(self):
        canonical = DEFAULT_ICON.read_bytes()
        data = icon_fixture(2, canonical)
        struct.pack_into("<I", data, 512 + 20, 0xFFFFFFFF)
        self.gui.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "out of bounds"):
            check_icon(self.gui, canonical)

    def test_missing_icon_group_rejected(self):
        canonical = DEFAULT_ICON.read_bytes()
        data = icon_fixture(2, canonical)
        struct.pack_into("<I", data, 512 + 24, 15)
        self.gui.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "missing ICON/GROUP_ICON"):
            check_icon(self.gui, canonical)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--gui", type=Path)
    parser.add_argument("--icon", type=Path, default=DEFAULT_ICON)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(PeChecks))
        return 0 if result.wasSuccessful() else 1
    if args.cli is None or args.gui is None:
        parser.error("--cli and --gui are required unless --self-test is used")
    try:
        check_pair(args.cli, args.gui, args.icon.read_bytes())
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
