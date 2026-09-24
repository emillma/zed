# Dev shell for the zed fork (jj backend work): build + run.
#
#   nix-shell /home/emil/mono/submodules/zed/shell.nix --run "cargo build -p zed"
#
# The shellHook exports the runtime library paths the built binary needs:
# gpui dlopens libwayland-client at startup (bare `nix-shell -p` builds fine
# but the binary panics with NoWaylandLib), and wgpu needs the Vulkan loader
# plus the system ICD under /run/opengl-driver.
{
  pkgs ? import <nixpkgs> { },
}:
let
  runtimeLibs = with pkgs; [
    wayland # libwayland-client — dlopen'd by gpui (NoWaylandLib without it)
    libxkbcommon # keymap
    libxcb
    libx11 # x11 fallback path
    vulkan-loader # wgpu
    alsa-lib # audio
    libva # video decode
  ];
in
pkgs.mkShell {
  packages = with pkgs; [
    pkg-config
    cmake
    clang
    lld
    llvm
    jq
    git
    curl
    gettext
    elfutils
    alsa-lib
    fontconfig
    glib
    openssl
    libva
    wayland
    libxcb
    libx11
    libxkbcommon
    zstd
    vulkan-loader
    sqlite
  ];

  shellHook = ''
    export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath runtimeLibs}:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    if [ -e /run/opengl-driver/share/vulkan/icd.d/nvidia_icd.json ]; then
      export VK_ICD_FILENAMES=/run/opengl-driver/share/vulkan/icd.d/nvidia_icd.json
    fi
  '';
}
