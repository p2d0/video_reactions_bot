{ pkgs }:

with pkgs;

# Configure your development environment.
devshell.mkShell {
  name = "reaction_bot";
  motd = ''
Entered reaction_bot app development environment.
'';
  env = [
    {
      name = "PKG_CONFIG_PATH";
      value = "${openssl.dev}/lib/pkgconfig";
    }
  ];
  packages = [
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
