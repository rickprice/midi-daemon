{ lib, rustPlatform, pkg-config, alsa-lib }:

rustPlatform.buildRustPackage {
  pname = "midi-daemon";
  version = "0.4.9";

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ alsa-lib ];

  meta = with lib; {
    description = "A Lua-scriptable MIDI routing daemon for Linux";
    homepage = "https://github.com/rickprice/midi-daemon";
    license = licenses.bsd3;
    platforms = platforms.linux;
    mainProgram = "midi-daemon";
  };
}
