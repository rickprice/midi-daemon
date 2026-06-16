{
  description = "A Lua-scriptable MIDI routing daemon for Linux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in {
      # 'nix build' / 'nix run'
      packages = forAllSystems (system: {
        midi-daemon = (pkgsFor system).callPackage ./nix/package.nix {};
        default = self.packages.${system}.midi-daemon;
      });

      # NixOS module — wires up the systemd service, user, and group.
      # Usage in configuration.nix:
      #   inputs.midi-daemon.nixosModules.default
      nixosModules.midi-daemon = import ./nix/module.nix;
      nixosModules.default = self.nixosModules.midi-daemon;

      # Dev shell: 'nix develop'
      devShells = forAllSystems (system:
        let pkgs = pkgsFor system; in {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [ rustc cargo pkg-config ];
            buildInputs = with pkgs; [ alsa-lib ];
          };
        }
      );
    };
}
