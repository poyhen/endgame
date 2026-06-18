{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];

      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              just
              nixd
              just-lsp
              git
              curl-impersonate
              ffmpeg
              yt-dlp
              gallery-dl
              deno
            ];

            RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
          };
        }
      );
    };
}
