{
  description = "hledger MCP server for Claude Cowork — dev environment (hledger 1.52 + Rust + git)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Toolchain comes from rust-toolchain.toml so nix and rustup agree.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # The project pins hledger 1.52 (docs §16). The flake.lock pins the exact
        # nixpkgs revision; the shellHook flags a mismatch so a drifted pin is loud.
        expectedHledger = "1.52";
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.hledger
            pkgs.git
            pkgs.hledger-web   # optional read-only live GUI (docs §8); enable when needed
            # Cargo dev tools. mise [tools] pins these for the outside-nix sandboxed loop;
            # the flake also provides them so `nix develop` (and CI) has the gate tooling
            # without a separate `mise install`.
            pkgs.cargo-nextest
            pkgs.cargo-llvm-cov
          ];

          # Stable path to the pinned binary for the adapter / tests (iiAtlas convention).
          HLEDGER_EXECUTABLE_PATH = "${pkgs.hledger}/bin/hledger";

          shellHook = ''
            have="$(${pkgs.hledger}/bin/hledger --version 2>/dev/null | sed -n 's/^hledger \([0-9.]*\).*/\1/p')"
            case "$have" in
              ${expectedHledger}*)
                printf '[flake] hledger %s ✓  rust %s\n' "$have" "$(rustc --version | cut -d" " -f2)"
                ;;
              *)
                printf '\033[33m[flake] WARNING: hledger %s present, project pins %s.\n         Repin the nixpkgs input to a revision shipping %s.\033[0m\n' \
                  "$have" "${expectedHledger}" "${expectedHledger}"
                ;;
            esac
          '';
        };
      });
}
