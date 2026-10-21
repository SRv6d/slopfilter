{
  description = "slopfilter development and release environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs =
    {
      self,
      crane,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forEachSystem = nixpkgs.lib.genAttrs supportedSystems;
      mkPkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
      mkRustToolchain = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      devShells = forEachSystem (
        system:
        let
          pkgs = mkPkgs system;
          rustToolchain = mkRustToolchain pkgs;
        in
        {
          default = pkgs.mkShell {
            packages = [
              rustToolchain
              pkgs.rust-analyzer
              pkgs.cargo-deny
              pkgs.git
              pkgs.gh
              pkgs.just
              pkgs.nixfmt
            ];
          };
        }
      );

      packages = forEachSystem (
        system:
        let
          pkgs = mkPkgs system;
          rustToolchain = mkRustToolchain pkgs;
          build = import ./nix/build.nix {
            inherit pkgs crane rustToolchain;
          };
          packaging = import ./nix/package.nix {
            inherit pkgs;
            inherit (build) packages;
          };
        in
        {
          slopfilter = build.packages.${build.defaultTarget};
          default = build.packages.${build.defaultTarget};
          rust-toolchain = rustToolchain;
          release-artifacts = packaging.releaseArtifacts;
        }
        // build.packages
        // pkgs.lib.mapAttrs' (
          target: archive: pkgs.lib.nameValuePair "archive-${target}" archive
        ) packaging.archives
      );
    };
}
