#!/usr/bin/env python3
"""Finalize the project's AppDir identity before every AppImage repack."""
import argparse
import configparser
import json
from pathlib import Path
import shutil
import struct

PROJECT = Path(__file__).resolve().parents[1]


def png_size(path):
    with Path(path).open('rb') as stream:
        header = stream.read(24)
    if len(header) != 24 or header[:8] != b'\x89PNG\r\n\x1a\n' or header[12:16] != b'IHDR':
        raise ValueError(f'Not a PNG: {path}')
    return struct.unpack('>II', header[16:24])


def identity(project):
    config = json.loads((project / 'src-tauri/tauri.conf.json').read_text())
    return config, config['identifier'], 'video-transcoder'


def desktop_values(path):
    config = configparser.ConfigParser(interpolation=None)
    config.optionxform = str
    config.read(path)
    return config['Desktop Entry']


def prepare(appdir, project=PROJECT):
    appdir = Path(appdir).resolve()
    config, app_id, executable = identity(project)
    if not (appdir / 'AppRun').exists() or not (appdir / 'usr/bin' / executable).is_file():
        raise ValueError('Expected a Video Transcoder AppDir with AppRun and its executable')
    aliases = {config['productName'], executable, app_id}
    # Never silently remove a second application's desktop entry.
    if any(path.stem not in aliases for path in appdir.glob('*.desktop')):
        raise ValueError('Unexpected desktop entry in this single-application AppDir')
    desktop = project / f'{app_id}.desktop'
    values = desktop_values(desktop)
    if values['Icon'] != app_id or values['StartupWMClass'] != app_id:
        raise ValueError('Project desktop entry does not match the application identifier')
    icons = {}
    for name in config['bundle']['icon']:
        path = project / 'src-tauri' / name
        if path.suffix == '.png':
            width, height = png_size(path)
            if width != height:
                raise ValueError(f'Application icon must be square: {path}')
            icons[width] = path
    if not icons:
        raise ValueError('No configured PNG application icons')
    applications = appdir / 'usr/share/applications'
    applications.mkdir(parents=True, exist_ok=True)
    for alias in aliases:
        for path in [appdir / f'{alias}.desktop', applications / f'{alias}.desktop', appdir / f'{alias}.png']:
            path.unlink(missing_ok=True)
    (appdir / '.DirIcon').unlink(missing_ok=True)
    shutil.copyfile(desktop, applications / f'{app_id}.desktop')
    (appdir / f'{app_id}.desktop').symlink_to(f'usr/share/applications/{app_id}.desktop')
    # Install theme fragments, not an index.theme that could override the
    # user's full hicolor directory list when an integrator copies the icons.
    hicolor = appdir / 'usr/share/icons/hicolor'
    # A cache from the old bundle would still index the pre-rename aliases.
    (hicolor / 'icon-theme.cache').unlink(missing_ok=True)
    for alias in aliases:
        for path in hicolor.glob(f'*/apps/{alias}.png'):
            path.unlink()
    for size, source in icons.items():
        target = hicolor / f'{size}x{size}/apps/{app_id}.png'
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    largest = icons[max(icons)]
    shutil.copyfile(largest, appdir / f'{app_id}.png')
    (appdir / '.DirIcon').symlink_to(f'{app_id}.png')
    pixmap = appdir / f'usr/share/pixmaps/{app_id}.png'
    pixmap.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(largest, pixmap)
    metainfo = appdir / f'usr/share/metainfo/{app_id}.metainfo.xml'
    metainfo.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(project / metainfo.name, metainfo)
    verify(appdir, project)


def verify(appdir, project=PROJECT):
    appdir = Path(appdir).resolve()
    config, app_id, executable = identity(project)
    if not config['app'].get('enableGTKAppId'):
        raise ValueError('GTK must advertise the desktop application ID on Wayland')
    if {p.name for p in appdir.glob('*.desktop')} != {f'{app_id}.desktop'}:
        raise ValueError('AppDir must have exactly the canonical desktop entry')
    values = desktop_values(appdir / f'{app_id}.desktop')
    expected = {'Name': config['productName'], 'Icon': app_id,
                'Exec': executable, 'StartupWMClass': app_id}
    if any(values.get(key) != value for key, value in expected.items()):
        raise ValueError('Desktop/icon/window identity mismatch')
    root = appdir / f'{app_id}.png'
    png_size(root)
    if (appdir / 'usr/share/icons/hicolor/icon-theme.cache').exists():
        raise ValueError('Remove stale icon cache after finalizing icon names')
    if (appdir / '.DirIcon').read_bytes() != root.read_bytes():
        raise ValueError('.DirIcon does not resolve to the application icon')
    for name in config['bundle']['icon']:
        if Path(name).suffix == '.png':
            size, _ = png_size(project / 'src-tauri' / name)
            path = appdir / f'usr/share/icons/hicolor/{size}x{size}/apps/{app_id}.png'
            if png_size(path) != (size, size):
                raise ValueError(f'Icon dimensions do not match its directory: {path}')
    for path in [appdir / f'{app_id}.desktop', root, appdir / '.DirIcon']:
        if not path.resolve().is_relative_to(appdir):
            raise ValueError(f'Non-relocatable AppDir link: {path}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('appdir', type=Path)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    (verify if args.check else prepare)(args.appdir)
    print('AppDir desktop, icon and GTK identity verified.')
