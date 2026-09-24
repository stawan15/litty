{
  description = "litty: a tiny, fast, zero-config terminal emulator";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAll (pkgs:
        let
          lib = pkgs.lib;
          # winit loads these at run time on Linux.
          runtimeLibs = with pkgs; [ libxkbcommon wayland libGL xorg.libX11 xorg.libXcursor xorg.libXi xorg.libXrandr ];
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "litty";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = lib.optionals pkgs.stdenv.isLinux [ pkgs.patchelf ];
            postInstall = lib.optionalString pkgs.stdenv.isLinux ''
              patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/litty
              install -Dm644 packaging/litty.desktop $out/share/applications/litty.desktop
              install -Dm644 packaging/litty.png $out/share/icons/hicolor/512x512/apps/litty.png
            '';
            meta = {
              description = "Tiny, fast, zero-config terminal emulator";
              homepage = "https://github.com/stawan15/litty";
              license = lib.licenses.mit;
              mainProgram = "litty";
              platforms = systems;
            };
          };
        });
      apps = forAll (pkgs: {
        default = { type = "app"; program = "${self.packages.${pkgs.system}.default}/bin/litty"; };
      });
    };
}
