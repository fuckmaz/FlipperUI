#!/usr/bin/env bash
# Remove the libwayland-* copies that linuxdeploy bundles into the AppImage and
# repack it, so FlipperUI always uses the host's libwayland (issue #8).
#
# The bundled copies come from the Ubuntu build machine. With current Mesa, a
# bundled libwayland-client makes eglGetDisplay(EGL_DEFAULT_DISPLAY) fail, and
# WebKitGTK aborts with "Could not create default EGL display:
# EGL_BAD_PARAMETER", which leaves a blank window. Mixing host and bundled
# libwayland versions also trips NVIDIA's GBM backend. Every system that runs
# GTK 3 already ships libwayland, so nothing is lost by not bundling it.
#
# Usage: scripts/strip-appimage-libs.sh <file.AppImage> [...]
# Set APPIMAGETOOL / APPIMAGE_RUNTIME to use local copies instead of
# downloading the pinned ones.

set -euo pipefail

# Both pinned: appimagetool would otherwise fetch the "continuous" runtime.
APPIMAGETOOL_VERSION=1.9.1
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RUNTIME_VERSION=20251108
RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d

if [ "$#" -eq 0 ]; then
  echo "Usage: scripts/strip-appimage-libs.sh <file.AppImage> [...]" >&2
  exit 64
fi

# CI runners and containers may lack FUSE; unpack-and-run works everywhere.
export APPIMAGE_EXTRACT_AND_RUN=1

tool_dir=$(mktemp -d "${TMPDIR:-/tmp}/appimagetool.XXXXXX")
trap 'rm -rf "$tool_dir"' EXIT

appimagetool="${APPIMAGETOOL:-}"
if [ -z "$appimagetool" ]; then
  appimagetool="$tool_dir/appimagetool"
  curl -fsSL -o "$appimagetool" \
    "https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
  echo "${APPIMAGETOOL_SHA256}  $appimagetool" | sha256sum -c --quiet -
  chmod +x "$appimagetool"
fi

runtime="${APPIMAGE_RUNTIME:-}"
if [ -z "$runtime" ]; then
  runtime="$tool_dir/runtime-x86_64"
  curl -fsSL -o "$runtime" \
    "https://github.com/AppImage/type2-runtime/releases/download/${RUNTIME_VERSION}/runtime-x86_64"
  echo "${RUNTIME_SHA256}  $runtime" | sha256sum -c --quiet -
fi

for input_path in "$@"; do
  if [ ! -f "$input_path" ]; then
    echo "AppImage not found: $input_path" >&2
    exit 1
  fi
  case "$input_path" in
    /*) appimage="$input_path" ;;
    *) appimage="$PWD/${input_path#./}" ;;
  esac

  work_dir=$(mktemp -d "${TMPDIR:-/tmp}/flipperui-strip.XXXXXX")
  (cd "$work_dir" && "$appimage" --appimage-extract >/dev/null)
  appdir="$work_dir/squashfs-root"

  removed=$(find "$appdir/usr/lib" -maxdepth 1 -name 'libwayland-*.so*' -print -delete)
  if [ -z "$removed" ]; then
    echo "No bundled libwayland in $(basename "$appimage"); leaving it unchanged"
    rm -rf "$work_dir"
    continue
  fi
  echo "Removed from $(basename "$appimage"):"
  printf '%s\n' "$removed" | sed "s#^$appdir/#  #"

  ARCH="${ARCH:-x86_64}" "$appimagetool" --no-appstream --runtime-file "$runtime" \
    "$appdir" "$appimage.tmp" >/dev/null
  chmod 755 "$appimage.tmp"
  mv -f "$appimage.tmp" "$appimage"
  rm -rf "$work_dir"
done
