{
  description = "reaction_bot";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
    devshell.url = "github:numtide/devshell";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, fenix, naersk, devshell, flake-utils }:
    flake-utils.lib.eachSystem ["armv7l-linux" "armv7a-linux" "x86_64-linux"] (system:
      let
        inherit (nixpkgs) lib;
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          config.allowBroken = true;
          overlays = [
            devshell.overlays.default
            # self.overlay
          ];
        };
        toUpCase = (s: pkgs.lib.toUpper (builtins.replaceStrings ["-"] ["_"] s));
        buildFor = ({target, cross}:
          let
            toolchain = with fenix.packages.${system}; combine [
              minimal.cargo
              minimal.rustc
              targets.${target}.latest.rust-std
            ];
            packages = nixpkgs.legacyPackages.${system};
          in
            (naersk.lib.${system}.override {
              cargo = toolchain;
              rustc = toolchain;
              pkgs = packages.pkgsCross.${cross};
            }).buildPackage {
              src = ./.;
              autoCrateSpecificOverrides = true;
              CARGO_BUILD_TARGET = target;
              TARGET_CC =
                let
                  inherit (packages.pkgsCross.${cross}.stdenv) cc;
                in
                  "${cc}/bin/${cc.targetPrefix}cc";
              "CARGO_TARGET_${toUpCase target}_LINKER" =
                let
                  inherit (packages.pkgsCross.${cross}.stdenv) cc;
                in
                  "${cc}/bin/${cc.targetPrefix}cc";
            });
      in
        {
          packages = {
            armv7 = buildFor {target = "armv7-unknown-linux-musleabihf"; cross= "armv7l-hf-multiplatform";};
            aarch64 = buildFor {target = "aarch64-unknown-linux-musl"; cross = "aarch64-multiplatform-musl";};
          };

          devShell = import ./devshell.nix { inherit pkgs; };
        }
    );
}
