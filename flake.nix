{
  description = "disk-steward: keeps host free space above a floor by sweeping declared consumers (dry-run by default), with a Home Manager module";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    # Only for the module-eval check; consumers already carry home-manager and should `follows` it.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, home-manager }:
    let
      lib = nixpkgs.lib;
      systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAll = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        disk-steward = pkgs.rustPlatform.buildRustPackage {
          pname = "disk-steward";
          version = "0.1.0";
          src = lib.fileset.toSource { root = ./.; fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ]; };
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = true; # the 9 unit tests: dry-run, enabled gating, recency, .git, open files, budget, rotation, age, undeclared
          meta.mainProgram = "disk-steward";
          meta.license = lib.licenses.mit;
        };
        default = disk-steward;
      });

      homeManagerModules.default = import ./nix/module.nix self;

      checks = forAll (pkgs:
        let
          render = import ./nix/conf.nix { inherit lib; };
          fixture = {
            live = false; volume = "/v"; floorGb = 150; stateDir = "/s"; lsof = "/usr/sbin/lsof"; ntfy = null;
            chitin = { package = null; as = "disk-steward"; };
            scanRoots = [ "/a" "/b" ]; undeclaredGb = 5;
            consumers = {
              logs = { kind = "logs"; enable = false; settings = { files = [ "/l1" "/l2" ]; keep = "5"; }; };
              w = { kind = "watch"; enable = false; settings = { path = "/w"; budget_gb = 50; }; };
            };
          };
          expected = ''
            mode=dry-run
            volume=/v
            floor_gb=150
            state_dir=/s
            lsof=/usr/sbin/lsof
            scan_roots=/a,/b
            undeclared_gb=5
            [consumer logs]
            kind=logs
            enabled=false
            files=/l1,/l2
            keep=5
            [consumer w]
            kind=watch
            budget_gb=50
            path=/w
          '';
        in
        {
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          conf-render = assert render fixture == expected; pkgs.runCommand "disk-steward-conf-render" { } "touch $out";
          # Static safety properties of the crate and module (no deletion by shell, safe defaults).
          safety = pkgs.runCommand "disk-steward-safety" { } ''
            src=${./src/main.rs}; mod=${./nix/module.nix}
            fail=0; ok() { if [ "$2" = 1 ]; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }
            ok "crate is std-only" $( ! sed -n '/^\[dependencies\]/,/^\[/p' ${./Cargo.toml} | grep -v '^\[' | grep -q '[a-z]' && [ "$(grep -c '^name = ' ${./Cargo.lock})" = 1 ] && echo 1 || echo 0 )
            ok "no rm or shell for deletion" $( ! grep -q 'Command::new("rm")' $src && ! grep -q '"sh"\|"bash"' $src && echo 1 || echo 0 )
            ok "ntfy pointer is contentless (closed title, no URL)" $( grep -q '"DATA home stalled"' $src && ! grep -q 'http' $src && echo 1 || echo 0 )
            ok "defaults: live = false, consumer enable = false" $( grep -q 'live = lib.mkOption { type = lib.types.bool; default = false' $mod && grep -q 'enable = lib.mkOption { type = lib.types.bool; default = false' $mod && echo 1 || echo 0 )
            [ "$fail" = 0 ] && touch $out
          '';
        }
        // lib.optionalAttrs pkgs.stdenv.isDarwin {
          # The module evaluates under Home Manager with defaults, ships dry-run, and its agent runs the package on the rendered conf.
          module-eval =
            let
              hm = home-manager.lib.homeManagerConfiguration {
                inherit pkgs;
                modules = [
                  self.homeManagerModules.default
                  { home = { username = "t"; homeDirectory = "/Users/t"; stateVersion = "24.11"; }; programs.diskSteward.enable = true;
                    programs.diskSteward.consumers.t = { kind = "rust-target"; settings.roots = "/tmp/x"; }; }
                ];
              };
              agent = hm.config.launchd.agents.disk-steward.config;
              conf = builtins.elemAt agent.ProgramArguments 1;
            in
            pkgs.runCommand "disk-steward-module-eval" { } ''
              grep -qx 'mode=dry-run' ${conf}
              grep -qx 'state_dir=/Users/t/.local/state/disk-steward' ${conf}
              grep -qx 'enabled=false' ${conf}
              ! grep -q '^ntfy=\|^chitin=' ${conf}
              [ "${builtins.elemAt agent.ProgramArguments 0}" = "${lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default}" ]
              touch $out
            '';
        });
    };
}
