let pkgs = import <nixpkgs> {};
let cargo_nix = pkgs.callPackage ./Cargo.nix {};
in cargo_nix.workspaceMembers.gluon_repl.build
