let cargo_nix = callPackage ./Cargo.nix {};
in cargo_nix.workspaceMembers.gluon_repl.build
