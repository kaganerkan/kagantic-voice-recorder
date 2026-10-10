#!/usr/bin/env python3
"""Copy distribution-supplied notices and source provenance into an AppDir."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
from urllib.parse import quote


def digest(path):
    checksum = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            checksum.update(block)
    return checksum.hexdigest()


def build_id(path):
    notes = subprocess.check_output(['readelf', '-n', str(path)], text=True)
    match = re.search(r'Build ID:\s*([0-9a-f]+)', notes)
    if not match:
        raise RuntimeError(f'Library has no distribution build ID: {path}')
    return match[1]


def collect(appdir):
    destination = appdir / 'usr/share/doc/kagantic-voice-recorder/libraries'
    destination.mkdir(parents=True, exist_ok=False)
    libraries = []
    for path in sorted((appdir / 'usr/lib').rglob('*')):
        if path.is_file():
            with path.open('rb') as stream:
                if stream.read(4) == b'\x7fELF':
                    libraries.append(path)
    if not libraries:
        raise RuntimeError('No deployed libraries to document')
    provenance = {'libraries': [{'path': str(path.relative_to(appdir)), 'sha256': digest(path)}
                                for path in libraries]}
    sdk_licenses = Path('/usr/share/licenses/freedesktop-sdk')
    sdk_manifest = Path('/usr/manifest.json')
    if sdk_licenses.is_dir() and sdk_manifest.is_file():
        # Dereference the SDK's common-license links so the image is self-contained.
        shutil.copytree('/usr/share/licenses', destination / 'freedesktop-sdk-licenses')
        shutil.copy2(sdk_manifest, destination / 'freedesktop-sdk-manifest.json')
        provenance['distribution'] = 'Freedesktop SDK'
        provenance['sources'] = 'freedesktop-sdk-manifest.json contains original source URLs and revisions'
    elif shutil.which('dpkg-query'):
        distro = dict(line.split('=', 1) for line in Path('/etc/os-release').read_text().splitlines()
                      if '=' in line).get('ID', '').strip('"')
        if distro not in ('ubuntu', 'debian'):
            raise RuntimeError(f'Unsupported distribution source archive: {distro}')
        source_base = ('https://launchpad.net/ubuntu/+source/' if distro == 'ubuntu'
                       else 'https://sources.debian.org/src/')
        packages = set()
        for library in libraries:
            # linuxdeploy changes RPATH/strips ELF files; GNU build IDs remain stable.
            identity = build_id(library)
            query = subprocess.run(['dpkg-query', '-S', '*/' + library.name],
                                   check=True, capture_output=True, text=True)
            matches = set()
            for line in query.stdout.splitlines():
                package, separator, filename = line.rpartition(': ')
                candidate = Path(filename)
                if separator and candidate.is_file() and build_id(candidate) == identity:
                    matches.add(package)
            if not matches:
                raise RuntimeError(f'No distribution package matches deployed library: {library}')
            packages.update(matches)
            next(item for item in provenance['libraries']
                 if item['path'] == str(library.relative_to(appdir)))['packages'] = sorted(matches)
        records = []
        for package in sorted(packages):
            metadata = subprocess.check_output(
                ['dpkg-query', '-W', '-f=${binary:Package}\t${Version}\t${source:Package}\t${source:Version}', package],
                text=True).split('\t')
            name, version, source, source_version = metadata
            copyright_file = Path('/usr/share/doc') / name.split(':')[0] / 'copyright'
            if not copyright_file.is_file():
                raise RuntimeError(f'Distribution copyright file missing: {copyright_file}')
            shutil.copy2(copyright_file, destination / (name + '.copyright'))
            records.append({'binary': name, 'version': version, 'source': source,
                            'source_version': source_version,
                            'source_url': f'{source_base}{quote(source, safe="")}/{quote(source_version, safe="")}'})
        shutil.copytree('/usr/share/common-licenses', destination / 'common-licenses')
        provenance['distribution'] = distro
        provenance['packages'] = records
        provenance['sources'] = 'Distribution sources linked by exact version; packaging changes ELF RPATH/stripping only'
    else:
        raise RuntimeError('AppImage notices require Debian/Ubuntu package metadata or Freedesktop SDK licenses and manifest')
    (destination / 'provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
    print(f'Deployed library notices and source provenance collected for {len(libraries)} ELF files')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('appdir', type=Path)
    collect(parser.parse_args().appdir.resolve(strict=True))
