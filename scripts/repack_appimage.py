#!/usr/bin/env python3
"""Repack a Tauri AppImage/AppDir with the project's canonical desktop layout."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from appimage_layout import prepare


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        while block := stream.read(1024 * 1024):
            result.update(block)
    return result.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--image', type=Path)
    source.add_argument('--appdir', type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--runtime-from', type=Path)
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--appimagetool', default='appimagetool')
    parser.add_argument('--replace', action='store_true', help='Atomically replace an existing CI artifact')
    args = parser.parse_args()
    output = args.output.absolute()
    checksum = Path(str(output) + '.sha256')
    if not args.replace and (output.exists() or checksum.exists()):
        parser.error('Output already exists; choose a new filename')
    runtime_source = args.runtime_from or args.image
    if runtime_source is None:
        parser.error('--appdir requires --runtime-from (an existing AppImage)')
    runtime_source = runtime_source.resolve()
    environment = {**os.environ, 'APPIMAGE_EXTRACT_AND_RUN': '1'}
    with tempfile.TemporaryDirectory(prefix='.vt-package-', dir=output.parent) as work:
        work = Path(work)
        if args.image:
            subprocess.run([str(args.image.resolve()), '--appimage-extract'], cwd=work,
                           env=environment, stdout=subprocess.DEVNULL, check=True)
            appdir = work / 'squashfs-root'
        else:
            appdir = work / 'Video Transcoder.AppDir'
            shutil.copytree(args.appdir, appdir, symlinks=True)
        if args.binary:
            shutil.copy2(args.binary, appdir / 'usr/bin/video-transcoder')
        prepare(appdir)
        offset = int(subprocess.check_output([str(runtime_source), '--appimage-offset']))
        if not 0 < offset < min(runtime_source.stat().st_size, 4 * 1024 * 1024):
            raise ValueError('Invalid source AppImage runtime offset')
        runtime = work / 'runtime'
        with runtime_source.open('rb') as stream:
            runtime.write_bytes(stream.read(offset))
        staged = work / 'output.AppImage'
        subprocess.run([args.appimagetool, '--runtime-file', str(runtime),
                        '--mksquashfs-opt=-processors', '--mksquashfs-opt=2',
                        str(appdir), str(staged)], env=environment, check=True)
        checksum_text = f'{digest(staged)}  {output.name}\n'
        if args.replace:
            os.replace(staged, output)
        else:
            os.link(staged, output)  # Same-filesystem atomic no-clobber install.
        checksum.write_text(checksum_text)
    print(f'Created {output}\n{checksum_text}', end='')


if __name__ == '__main__':
    main()
