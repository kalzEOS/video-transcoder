# Linux desktop and AppImage identity

Video Transcoder uses `com.kalzi.videotranscoder` as its stable application ID.
The displayed name remains **Video Transcoder**, and its executable remains
`video-transcoder`. Do not change these into an absolute, machine-specific
`Icon=` path.

`app.enableGTKAppId` sets the GtkApplication ID; GTK 3 also requires
`g_set_prgname()` to use that ID before GTK initializes. Without both,
the previous AppImage advertised `video-transcoder` (confirmed in a
Wayland protocol trace), while Tauri generated `Video Transcoder.desktop`
and the separate Flatpak metadata already used the reverse-DNS identifier.
`StartupWMClass` by itself does not set the Wayland application ID.

The AppImage finalizer gives the desktop file, root PNG, `.DirIcon`, theme
icons, pixmap fallback and AppStream launchable the same stable identifier.
It keeps PNG dimensions consistent with hicolor size directories and uses
only relocatable links. It does not ship a partial hicolor `index.theme`
that could mask other applications' icon directories.

## Build and verify

After the normal Tauri build, finalize its AppImage before distributing it:

```sh
python3 scripts/repack_appimage.py \
  --image 'src-tauri/target/release/bundle/appimage/Video Transcoder_2.7.1_amd64.AppImage' \
  --output 'dist/Video-Transcoder-2.7.1-linux-test.AppImage'
```

The command needs `appimagetool` on PATH, or `--appimagetool /path/to/tool`.
It reuses the source AppImage's runtime, works in destination-side temporary
storage, validates the icon layout and emits a SHA-256 sidecar. Existing
outputs are refused by default. `--replace` is for the explicit atomic
replacement of an intermediate CI artifact. Both normal and full Linux
artifacts in the build workflow use this finalizer. A raw Tauri artifact
has not passed this project-specific finalization step.

For a cached AppDir, use `--appdir`, `--runtime-from` and optionally
`--binary` to supply a freshly compiled executable. Never modify an installed
AppImage or desktop entry to prepare a build.

```sh
python3 scripts/test_appimage_layout.py
python3 scripts/appimage_layout.py /path/to/extracted/AppDir --check
desktop-file-validate /path/to/extracted/AppDir/com.kalzi.videotranscoder.desktop
# Optional, in an existing Wayland session; creates only isolated test state:
python3 scripts/test_wayland_identity.py /path/to/local-test.AppImage
```

## Installer and KDE boundaries

AppImage managers may rename exported desktop files/icons and must refresh
their host icon/desktop caches. No application can guarantee arbitrary
installer rewrites or repair an already cached entry merely by containing
a PNG. Reinstalling must export the finalized package, rather than the old
raw Tauri artifact. This project does not write to the user's installed
desktop entries or icon themes at application startup.

During investigation the existing installed PNG was valid and a fresh Qt
lookup resolved the manager-generated icon name. The remaining desktop
entry's Exec/TryExec referred to a deleted AppImage. Consequently, a stale
Plasma cache/installer failure was not reproduced or attributed to filename
capitalization. The confirmed project defect was the inconsistent desktop
and runtime identities; the finalizer also removes ambiguous icon aliases.
Real Shelly/AppManager reinstallation remains an end-user check; tests use
isolated data/cache directories and do not edit the installed entries.

References:

- https://docs.appimage.org/reference/appdir.html
- https://specifications.freedesktop.org/icon-theme/
- https://v2.tauri.app/reference/config/#enablegtkappid
- https://docs.gtk.org/gtk4/migrating-3to4.html#set-a-proper-application-id

## Local verification (2026-10-28)

- Before: Wayland advertised `video-transcoder`. After: both the compiled GUI
  and packaged AppImage advertised `com.kalzi.videotranscoder` in actual
  `xdg_toplevel.set_app_id` traces. Enabling the GTK application ID alone was
  insufficient on GTK 3; setting its program name before initialization was
  also verified necessary.
- KDE's native KService/KIconLoader loaded the canonical desktop entry and
  resolved valid packaged PNGs at 16, 32, 48, 64 and 128 requested pixels in
  isolated XDG directories. No installed entry or icon theme was modified.
- `desktop-file-validate`, desktop/AppStream ID alignment, icon size checks,
  relocatable links and packaged executable SHA-256 matching passed. The
  single root PNG is 512×512 and all themed sizes use matching directories.
- Five packaging regression tests, the existing window-permission regression,
  GUI release build, Clippy and workflow YAML parsing passed. Both AppDir and
  image-to-image/atomic-replacement packaging paths were exercised locally;
  GitHub Actions itself was not run or changed remotely.
- Test artifact: `Video-Transcoder-2.7.1-kde-icon-fix-cachyos-x86_64.AppImage`.
  SHA-256: `ed1bbef87abb81e6433bec978fb7232374d3e3baa6d3118d7332f9c3b09b97e8`.
  It includes the already-verified sidebar fix. No encoding code changed.
- appimagetool still warns about its expected legacy `.appdata.xml` filename;
  the standard modern `.metainfo.xml` with matching ID/launchable is present.
  This nonfatal warning did not prevent packaging or startup.
