self:
{ config, lib, pkgs, ... }:

# Home Manager module: disk-steward as a launchd interval job (measure free space, sweep the DECLARED consumers by policy, write
# <stateDir>/report.{txt,json}, optionally send one ntfy pointer per incident and publish a chitin disk.report).
# SAFETY: live = false and every consumer's enable = false by default. Deletion needs live = true AND that consumer's enable = true.
# Machine-specific things (which dirs to watch, the ntfy and chitin binaries, the chitin identity) are options, set by the host config.
let
  cfg = config.programs.diskSteward;
  home = config.home.homeDirectory;
  conf = pkgs.writeText "disk-steward.conf" (import ./conf.nix { inherit lib; } cfg);
  setting = with lib.types; oneOf [ str int (listOf str) ];
in
{
  options.programs.diskSteward = {
    enable = lib.mkEnableOption "disk-steward (a launchd interval job that keeps host free space above a floor; dry-run by default)";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "disk-steward.packages.\${system}.default";
    };
    live = lib.mkOption { type = lib.types.bool; default = false; description = "false = read-only dry run. Deletion also needs the consumer's own enable."; };
    floorGb = lib.mkOption { type = lib.types.int; default = 150; description = "Warn / act below this much free space."; };
    volume = lib.mkOption { type = lib.types.str; default = "/System/Volumes/Data"; };
    interval = lib.mkOption { type = lib.types.int; default = 1800; description = "launchd StartInterval, seconds."; };
    stateDir = lib.mkOption { type = lib.types.str; default = "${home}/.local/state/disk-steward"; description = "Where report.txt, report.json and the incident flag live."; };
    lsof = lib.mkOption { type = lib.types.str; default = "/usr/sbin/lsof"; };
    scanRoots = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "${home}/.local/state" "${home}/.cache" ];
      description = "Directories whose big undeclared children are listed (never deleted).";
    };
    undeclaredGb = lib.mkOption { type = lib.types.int; default = 5; description = "Report undeclared dirs at least this big."; };
    ntfy = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; description = "An ntfy-style binary called as `<bin> push -t TITLE MESSAGE` once per incident; null = no pointer."; };
    chitin = {
      package = lib.mkOption { type = lib.types.nullOr lib.types.package; default = null; description = "chitin, for `chitin publish --as <as> disk.report`; null = no report."; };
      as = lib.mkOption { type = lib.types.str; default = "disk-steward"; description = "The chitin identity the report is published as (its key is the host's business)."; };
    };
    logFile = lib.mkOption { type = lib.types.str; default = "${home}/Library/Logs/disk-steward.log"; };
    consumers = lib.mkOption {
      default = { };
      description = "Declared consumers. kind: logs | rust-target | age-files | watch | nix-gc; settings are the kind's keys (see README).";
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          kind = lib.mkOption { type = lib.types.enum [ "logs" "rust-target" "age-files" "watch" "nix-gc" ]; };
          enable = lib.mkOption { type = lib.types.bool; default = false; description = "Let a live run act on this consumer. Ignored for watch."; };
          settings = lib.mkOption { type = lib.types.attrsOf setting; default = { }; };
        };
      });
    };
  };

  config = lib.mkIf cfg.enable {
    # A standing LaunchAgent. Dry-run: reads sizes and writes only stateDir. Live + enabled consumers delete only what their policy declares.
    launchd.agents.disk-steward = {
      enable = true;
      config = {
        ProgramArguments = [ "${lib.getExe cfg.package}" "${conf}" ];
        StartInterval = cfg.interval;
        RunAtLoad = false;
        KeepAlive = false;
        ProcessType = "Background";
        LowPriorityIO = true;
        Nice = 10;
        StandardOutPath = cfg.logFile;
        StandardErrorPath = cfg.logFile;
      };
    };
  };
}
