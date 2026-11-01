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
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [
            (_final: prev: {
              # Keep extractor fixes current even when nixos-unstable lags a release.
              yt-dlp =
                if prev.lib.versionOlder prev.yt-dlp.version "2026.10.26" then
                  prev.yt-dlp.overridePythonAttrs (_old: rec {
                    version = "2026.10.26";
                    src = prev.fetchFromGitHub {
                      owner = "yt-dlp";
                      repo = "yt-dlp";
                      tag = version;
                      hash = "sha256-0gX99Q45+1L4eH0YK0942DVTq5Wwscq3Qkj+N/EjHek=";
                    };
                  })
                else
                  prev.yt-dlp;
            })
          ];
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        rec {
          default = endgame;
          endgame = pkgs.rustPlatform.buildRustPackage {
            pname = "endgame";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./config.example.json
                ./src
              ];
            };

            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes."grammers-client-0.11.0" = "sha256-h6yio6phpmayaWAI4WjMqjSP7guq48MazUaQdsjPSxI=";
            };

            nativeBuildInputs = [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram "$out/bin/endgame" --prefix PATH : ${
                pkgs.lib.makeBinPath (
                  with pkgs;
                  [
                    curl-impersonate
                    deno
                    ffmpeg
                    gallery-dl
                    yt-dlp
                  ]
                )
              }
            '';

            meta.mainProgram = "endgame";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
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
