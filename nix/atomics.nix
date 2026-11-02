{ lib, rustPlatform, pkg-config, makeWrapper, alsa-lib, wayland, libxkbcommon
, vulkan-loader, mesa, fontconfig, git, libx11, libxcursor, libxrandr, libxi }:

let
  runtimeLibraries = [
    alsa-lib
    wayland
    libxkbcommon
    vulkan-loader
    mesa
    fontconfig
    libx11
    libxcursor
    libxrandr
    libxi
  ];
in
rustPlatform.buildRustPackage {
  pname = "atomics";
  version = "0.1.0";
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      let name = baseNameOf path;
      in !(builtins.elem name [ ".git" ".atomics" "target" ]);
  };

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "-p" "atomics" ];
  doCheck = false;

  nativeBuildInputs = [ pkg-config makeWrapper ];
  buildInputs = runtimeLibraries;

  postInstall = ''
    wrapProgram "$out/bin/atomics" \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibraries}" \
      --prefix PATH : "${lib.makeBinPath [ git ]}"
  '';

  meta = {
    description = "Review-first workspace for coding agents";
    homepage = "https://github.com/xmagit/ATOMICS";
    license = lib.licenses.mit;
    mainProgram = "atomics";
    platforms = [ "aarch64-linux" "x86_64-linux" ];
  };
}
