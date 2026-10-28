# disk-steward

A std-only Rust job that keeps a host's free space above a floor. One run measures free space, walks the **declared**
consumers and sweeps them by policy (smallest blast radius first: logs, scratch Rust `target/` dirs, age-limited files,
then `nix-collect-garbage`), writes `report.txt` / `report.json`, and optionally sends one contentless ntfy pointer per
incident and a chitin `disk.report`. Undeclared big directories are only listed, never deleted.

**Safe by default:** `live = false` and every consumer's `enable = false`. Deletion needs both. It never deletes a git
checkout (a dir holding `.git`), a dir with open files (per `lsof`), or anything newer than `min_age_hours`.

## Home Manager

```nix
inputs.disk-steward = { url = "github:bdelanghe/disk-steward"; inputs.nixpkgs.follows = "nixpkgs"; inputs.home-manager.follows = "home-manager"; };
# modules = [ disk-steward.homeManagerModules.default ];
programs.diskSteward = {
  enable = true;
  consumers.scratch-rust-targets = { kind = "rust-target"; settings = { roots = "/tmp/scratch"; budget_gb = 60; min_age_hours = 24; }; };
  consumers.nix-store = { kind = "nix-gc"; settings = { keep_days = 14; bin = "/nix/var/nix/profiles/default/bin/nix-collect-garbage"; }; };
  consumers.big-db = { kind = "watch"; settings = { path = "/var/db/x"; budget_gb = 50; }; };
};
```

Consumer kinds and their settings: `logs` (`files`, `rotate_mb`, `keep`), `rust-target` (`roots`, `budget_gb`,
`min_age_hours`), `age-files` (`path`, `max_age_days`), `watch` (`path`, `budget_gb`; report only), `nix-gc`
(`keep_days`, `bin`). Optional: `ntfy` (a binary called as `<bin> push -t TITLE MESSAGE`), `chitin.package` / `chitin.as`.

## Checks

`nix flake check`: the crate's unit tests (in the package build), the conf renderer, static safety properties, and on
macOS the module evaluated under Home Manager.
