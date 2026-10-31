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
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
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
              outputHashes."grammers-client-0.9.0" = "sha256-ByVea3aFAWB2EQqUemU2P1BeMyFo50EPKJuur1NN8AU=";
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
