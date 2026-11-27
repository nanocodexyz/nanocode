{
  description = "Nanocode CLI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          nanocode = pkgs.rustPlatform.buildRustPackage {
            pname = "nanocode";
            inherit version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "nanocode-cli"
              "--bins"
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
              "-p"
              "nanocode-cua-driver"
            ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            RUSTC_WRAPPER = "";
            doCheck = false;
            postInstall = ''
              mkdir -p $out/libexec/nanocode
              mv $out/bin/* $out/libexec/nanocode/
              for program in $out/libexec/nanocode/*; do
                makeWrapper $program $out/bin/$(basename $program) \
                  --set-default CUA_REPL_NODE ${pkgs.nodejs}/bin/node
              done
              ln -s nanocode $out/bin/nc
            '';
          };
          default = nanocode;
        }
      );
    };
}
