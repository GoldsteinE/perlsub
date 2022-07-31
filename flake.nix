{
  inputs = {
    nixpkgs.url      = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
    naersk.url       = "github:nix-community/naersk";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, naersk }:
    flake-utils.lib.eachDefaultSystem (system:
      let 
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust = (pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "cargo"
            "rustc"
            "rustfmt"
          ];
        });
        naersk-lib = naersk.lib."${system}".override {
          cargo = rust;
          rustc = rust;
        };
        envVars = with pkgs; {
          BWRAP = "${bubblewrap}/bin/bwrap";
          PERL = "${perl}/bin/perl";
          PRLIMIT = "${util-linux}/bin/prlimit";
          TIMEOUT = "${coreutils}/bin/timeout";
          ALLOW_DIRS = "${perl},${util-linux},${util-linux.lib},${glibc}";
        };
      in rec {
        packages.perlsub = naersk-lib.buildPackage {
          pname = "perlsub";
          root = ./.;
        };
        defaultPackage = packages.perlsub;

        apps.perlsub = packages.perlsub;
        defaultApp = apps.perlsub;

        nixosModules.default = with pkgs.lib; { config, ... }:
        let cfg = config.services.perlsub;
        in {
          options.services.perlsub = {
            enable = mkEnableOption "perlsub bot for Telegram";
            envFile = mkOption {
              type = types.str;
              default = "/etc/perlsub.env";
            };
          };
          config = mkIf cfg.enable {
            systemd.services.perlsub = {
              wantedBy = [ "multi-user.target" ];
              serviceConfig.ExecStart = "${self.defaultPackage.${system}}/bin/perlsub";
              serviceConfig.EnvironmentFile = cfg.envFile;
              serviceConfig.Environment = concatStringsSep " " (pkgs.lib.mapAttrsToList (name: value: name + "=" + value) envVars);
            };
          };
        };

        devShell = pkgs.mkShell ({
          buildInputs = [
            rust
            pkgs.rust-analyzer
          ];
          RUST_LOG = "info";
          DB_PATH = "db";
        } // envVars);
      }
    );
}
