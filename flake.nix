{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
  };

  outputs =
    {
      nixpkgs,
      systems,
      ...
    }:
    let
      forAllSystems =
        function: nixpkgs.lib.genAttrs (import systems) (system: function nixpkgs.legacyPackages.${system});

    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "sarne";
          version = "0.1.0";

          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          buildInputs = with pkgs; [
            iconv
            openssl
          ];

          nativeBuildInputs = with pkgs; [
            protobuf
            pkg-config
          ];
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer

            protobuf
            postgresql
            libiconv
          ];

          shellHook = ''
            # https://github.com/NixOS/nixpkgs/issues/376958#issuecomment-3471021813
            unset DEVELOPER_DIR
          '';
        };

        workflow = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer

            protobuf
            libiconv
            openssl
            pkg-config
          ];
        };
      });
    };
}
