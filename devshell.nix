{ pkgs }:

with pkgs;

# Configure your development environment.
devshell.mkShell {
  name = "standing_bot";
  motd = ''
Entered standing_bot app development environment.
'';
  env = [
    {
      name = "PKG_CONFIG_PATH";
      value = "${openssl.dev}/lib/pkgconfig";
    }
  ];
  packages = [
    sqlite
    pkg-config
    openssl.dev
    libiconv
    rustc
    cargo
    rustfmt
    clippy
    rust-analyzer
    gdb
  ];
}
