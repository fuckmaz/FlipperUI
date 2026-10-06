#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "Usage: scripts/validate-appimage.sh <file.AppImage> [...]" >&2
  exit 64
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

  filename=$(basename "$appimage")
  lowercase_filename=$(printf '%s' "$filename" | tr '[:upper:]' '[:lower:]')
  if [[ "$lowercase_filename" == *linux* ]]; then
    echo "AppImage filename must not contain 'Linux': $filename" >&2
    exit 1
  fi

  if [ ! -x "$appimage" ]; then
    echo "AppImage is not executable: $appimage" >&2
    exit 1
  fi

  (
    extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/flipperui-appimage.XXXXXX")
    trap 'rm -rf "$extract_dir"' EXIT

    cd "$extract_dir"
    "$appimage" --appimage-extract >/dev/null

    appdir="$extract_dir/squashfs-root"
    for launcher in "$appdir/AppRun" "$appdir/AppRun.wrapped"; do
      if [ ! -f "$launcher" ] || [ -n "$(find "$launcher" ! -perm -001 -print -quit)" ]; then
        echo "AppImage launcher must be executable by all users: $launcher" >&2
        exit 1
      fi
    done

    shopt -s nullglob
    desktop_files=("$appdir"/*.desktop)
    if [ "${#desktop_files[@]}" -eq 0 ]; then
      echo "AppImage is missing a root desktop file: $filename" >&2
      exit 1
    fi

    # `-e` deliberately follows symlinks. Tauri before 2.11.4 emitted an
    # absolute build-machine path here, so the link existed but was broken
    # after the AppImage moved to another machine.
    if [ ! -e "$appdir/.DirIcon" ]; then
      echo "AppImage is missing a resolvable root .DirIcon: $filename" >&2
      exit 1
    fi

    if [ -L "$appdir/.DirIcon" ]; then
      diricon_target=$(readlink "$appdir/.DirIcon")
      case "$diricon_target" in
        /*)
          echo "AppImage .DirIcon must use a relative target, got: $diricon_target" >&2
          exit 1
          ;;
      esac
    fi

    printf 'Validated AppImage catalog metadata: %s\n' "$filename"
  )
done
