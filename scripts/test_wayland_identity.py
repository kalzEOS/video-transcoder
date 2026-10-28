#!/usr/bin/env python3
"""Opt-in startup/protocol regression; requires a live Wayland session."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('program', type=Path, help='Compiled GUI or local test AppImage')
    args = parser.parse_args()
    program = args.program.resolve(strict=True)
    if not os.environ.get('WAYLAND_DISPLAY'):
        parser.error('This opt-in test requires a live Wayland session')
    project = Path(__file__).resolve().parents[1]
    expected = json.loads((project / 'src-tauri/tauri.conf.json').read_text())['identifier']
    with tempfile.TemporaryDirectory(prefix='.vt-wayland-test-', dir=program.parent) as folder:
        folder = Path(folder)
        env = {**os.environ, 'GDK_BACKEND': 'wayland', 'WAYLAND_DEBUG': 'client',
               'APPIMAGE_EXTRACT_AND_RUN': '1'}
        for variable, name in [('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'),
                               ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state'),
                               ('TMPDIR', 'runtime')]:
            path = folder / name
            path.mkdir()
            env[variable] = str(path)
        log = folder / 'protocol.log'
        with log.open('wb') as stream:
            process = subprocess.Popen([str(program)], env=env, stdout=stream,
                                       stderr=stream, start_new_session=True)
            try:
                time.sleep(4)
                if process.poll() is not None:
                    raise RuntimeError('GUI exited during startup: ' + log.read_text(errors='replace')[-2000:])
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
        lines = [line for line in log.read_text(errors='replace').splitlines() if 'set_app_id(' in line]
        if not any(f'set_app_id("{expected}")' in line for line in lines):
            raise RuntimeError(f'Wayland app_id must be {expected}; observed {lines}')
        print('\n'.join(lines))
        print('Isolated Wayland startup and identity check passed.')


if __name__ == '__main__':
    main()
