{
  description = "A simple development environment (nixpkgs pinned to merged PR #432712)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        python = pkgs.python3;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            git
            curl-impersonate-chrome
            ffmpeg
            yt-dlp
            gallery-dl
            ruff
            (python.withPackages (ps: with ps; [
              pyrogram
              tgcrypto
              uvloop
              curl-cffi
              cffi
              requests
              aiohttp
            ]))
          ];

          shellHook = ''
            echo "Python development environment loaded"
            echo "Python version: $(python --version)"
            echo "Available packages: pyrogram, tgcrypto, uvloop, curl-cffi, cffi"
          '';
        };
      }
    );
}
