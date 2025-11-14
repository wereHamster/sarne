{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
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

        devShells.workflow = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            rust-analyzer
            protobuf
            libiconv
          ];
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "sarne";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          buildInputs = with pkgs; [
            iconv
            openssl
          ];

          nativeBuildInputs = with pkgs; [
            protobuf
            pkg-config
          ];
        };
      }
    );
}
