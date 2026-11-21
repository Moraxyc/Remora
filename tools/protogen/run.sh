#!/usr/bin/env bash
# Run a tool script with lief + capstone available (Nix). Usage: ./run.sh protogen.py <args>
exec nix shell --impure --expr 'with import <nixpkgs> {}; python3.withPackages (p: [p.lief])' -c python3 "$@"
