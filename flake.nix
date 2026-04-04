{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };
  };
  outputs = { self, nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          overlays = [
            (import rust-overlay)
          ];
          pkgs = import nixpkgs { inherit system overlays; config = {}; };
        in {
          packages = {
            fcast-sender = pkgs.callPackage ./senders/desktop/fcast-sender.nix { };
            fcast-receiver = pkgs.callPackage ./receivers/experimental/desktop/fcast-receiver.nix {
              rustPlatform = pkgs.makeRustPlatform {
                cargo = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
                rustc = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
              };
            };
          };
        }
      );
}
