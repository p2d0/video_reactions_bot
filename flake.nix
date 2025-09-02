{
  nixConfig = {
    extra-substituters = [
      "https://upgradegamma.cachix.org"
    ];
    extra-trusted-public-keys = [
      "upgradegamma.cachix.org-1:iIifduPUNZ9OrRYgaEcKTeRQxbqr2/FbiF1bboND05A="
    ];
  };

  description = "standing_bot";

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
              # RUSTFLAGS = [
              #   # "-L ${(pkgs.openssl).out}/lib"
              #   "-L ${packages.pkgsCross.${cross}.glibc.static}"
              #   # "-L ${(packages.pkgsCross.${cross}.openssl.override { static = true;}).out}/lib"
              #   # "-L ${(packages.pkgsCross.${cross}.openssl).out}/lib"
              # ];
              RUSTFLAGS = [
                "-C"
                "target-feature=+crt-static"
                "-L ${packages.pkgsCross.${cross}.glibc.static}/lib"
              ];

              # CFLAGS = " -flto=false";

              # PKG_CONFIG_PATH = "${(packages.pkgsCross.${cross}.openssl.override { static = true;}).dev}/lib/pkgconfig";

              # OPENSSL_DIR = "${(packages.pkgsCross.${cross}.openssl).dev}";


              # OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";

              "${toUpCase target}_OPENSSL_STATIC" =  1;

              "${toUpCase target}_OPENSSL_INCLUDE_DIR" = "${(packages.pkgsCross.${cross}.openssl.override { static = true;}).dev}/include";
              "${toUpCase target}_OPENSSL_LIB_DIR" = "${(packages.pkgsCross.${cross}.openssl.override { static = true;}).out}/lib";

              # "${toUpCase target}_OPENSSL_LIB_DIR" = "${(packages.pkgsCross.${cross}.openssl.override { static = true;}).out}/lib";

              "${toUpCase target}_GLIBC_LIB_DIR" = "${packages.pkgsCross.${cross}.glibc.static}/lib";
              "${toUpCase target}_LD_LIBRARY_PATH" = "${packages.pkgsCross.${cross}.glibc.static}/lib";

              X86_64_UNKNOWN_LINUX_GNU_OPENSSL_STATIC =  1;
              X86_64_UNKNOWN_LINUX_GNU_OPENSSL_INCLUDE_DIR = "${(pkgs.openssl.override { static = true;}).dev}/include";
              X86_64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR = "${(pkgs.openssl.override { static = true;}).out}/lib";

              # buildInputs = [
              #   packages.pkgsCross.${cross}.glibc.static
              #   # packages.pkgsCross.${cross}.sqlite.dev
              #   # (packages.pkgsCross.${cross}.openssl.override { static = true;}).dev
              # ];

              # nativeBuildInputs = [
              #   pkgs.glibc
              # ];

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
            armv7-gnu = buildFor {target = "armv7-unknown-linux-gnueabihf"; cross= "armv7l-hf-multiplatform";};
            aarch64 = buildFor {target = "aarch64-unknown-linux-musl"; cross = "aarch64-multiplatform-musl";};
          };

          devShell = import ./devshell.nix { inherit pkgs; };
        }
    );
}
