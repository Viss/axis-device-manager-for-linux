#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app_dir="$repo_dir/build/AxisDeviceManagerForLinux.AppDir"
dist_dir="$repo_dir"
tool="$repo_dir/build/appimagetool-x86_64.AppImage"

cd "$repo_dir"
stable_bin="/root/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin"
if [[ -x "$stable_bin/cargo" && -x "$stable_bin/rustc" ]]; then
  RUSTC="$stable_bin/rustc" "$stable_bin/cargo" build --release --locked
else
  cargo build --release --locked
fi
mkdir -p "$app_dir/usr/bin" "$app_dir/usr/share/applications" "$app_dir/usr/share/icons/hicolor/scalable/apps" "$dist_dir" "$repo_dir/build"
install -m 0755 target/release/axis-device-manager-for-linux "$app_dir/usr/bin/axis-device-manager-for-linux"
ln -sfn usr/bin/axis-device-manager-for-linux "$app_dir/AppRun"
install -m 0644 packaging/axis-device-manager-for-linux.desktop "$app_dir/axis-device-manager-for-linux.desktop"
install -m 0644 packaging/axis-device-manager-for-linux.desktop "$app_dir/usr/share/applications/axis-device-manager-for-linux.desktop"
install -m 0644 assets/axis-device-manager-for-linux.svg "$app_dir/axis-device-manager-for-linux.svg"
install -m 0644 assets/axis-device-manager-for-linux.png "$app_dir/axis-device-manager-for-linux.png"
install -m 0644 assets/axis-device-manager-for-linux.svg "$app_dir/usr/share/icons/hicolor/scalable/apps/axis-device-manager-for-linux.svg"

if [[ ! -x "$tool" ]]; then
  curl -fL "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" -o "$tool"
  chmod 0755 "$tool"
fi

APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" "$app_dir" "$dist_dir/Axis_Device_Manager_for_Linux-0.1.30-x86_64.AppImage"
(cd "$dist_dir" && sha256sum "Axis_Device_Manager_for_Linux-0.1.30-x86_64.AppImage" > "SHA256SUMS")
