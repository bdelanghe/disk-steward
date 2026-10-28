{ lib }:

# Renders the disk-steward.conf text from the module's settings. Pure (no pkgs), so the flake checks it directly.
# Format (src/main.rs parse_config): `key=value` globals, then one `[consumer NAME]` section per consumer.
cfg:
let
  val = v: if builtins.isList v then lib.concatStringsSep "," v else toString v;
  line = k: v: "${k}=${val v}\n";
  consumer = name: c:
    "[consumer ${name}]\n"
    + line "kind" c.kind
    # A watch consumer is never swept, so it carries no enabled flag.
    + lib.optionalString (c.kind != "watch") (line "enabled" (if c.enable then "true" else "false"))
    + lib.concatStrings (lib.mapAttrsToList line c.settings);
in
lib.concatStrings [
  (line "mode" (if cfg.live then "live" else "dry-run"))
  (line "volume" cfg.volume)
  (line "floor_gb" cfg.floorGb)
  (line "state_dir" cfg.stateDir)
  (line "lsof" cfg.lsof)
  (lib.optionalString (cfg.ntfy != null) (line "ntfy" cfg.ntfy))
  (lib.optionalString (cfg.chitin.package != null) (line "chitin" "${cfg.chitin.package}/bin/chitin" + line "chitin_as" cfg.chitin.as))
  (line "scan_roots" cfg.scanRoots)
  (line "undeclared_gb" cfg.undeclaredGb)
]
+ lib.concatStrings (lib.mapAttrsToList consumer cfg.consumers)
