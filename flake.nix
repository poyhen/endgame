{
  description = "A simple development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            git
            python312Full
            python312Packages.pyrogram
            python312Packages.tgcrypto
            python312Packages.uvloop
            python312Packages.curl-cffi
            python312Packages.cffi
            curl-impersonate-chrome
            curl-impersonate-ff
            curl-impersonate
            ffmpeg
            yt-dlp
          ];
        };
      }
    );
}
