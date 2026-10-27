{
  description = "Linux GUI autoclicker using evdev and uinput";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      cargoToml = lib.importTOML ./Cargo.toml;

      # Only the files the build needs, so editing the README or flake doesn't
      # trigger a rebuild.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
        ];
      };

      # winit and wgpu load these with dlopen at runtime rather than linking them.
      # Wayland is the primary target; the X11 set is only used as a fallback
      # when there's no WAYLAND_DISPLAY.
      waylandLibs =
        pkgs: with pkgs; [
          wayland
          libxkbcommon
          vulkan-loader
          libGL
        ];
      x11Libs =
        pkgs: with pkgs; [
          libx11
          libxcursor
          libxi
          libxrandr
        ];
      runtimeLibs = pkgs: withX11: waylandLibs pkgs ++ lib.optionals withX11 (x11Libs pkgs);

      mkPackage =
        pkgs:
        {
          withX11 ? true,
        }:
        pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          inherit (cargoToml.package) version;
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postFixup = ''
            wrapProgram $out/bin/${cargoToml.package.name} \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath (runtimeLibs pkgs withX11)}
          '';

          meta = {
            inherit (cargoToml.package) description;
            homepage = cargoToml.package.repository;
            license = lib.licenses.mit;
            mainProgram = cargoToml.package.name;
            platforms = lib.platforms.linux;
          };
        };
    in
    {
      packages = forAllSystems (pkgs: {
        # Wayland, with an X11 fallback.
        default = mkPackage pkgs { };
        # Wayland only: smaller closure, won't start in an X11 session.
        wayland-only = mkPackage pkgs { withX11 = false; };
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      });

      checks = forAllSystems (
        pkgs:
        let
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          # Builds the app and runs `cargo test`.
          inherit package;

          # Clippy with the pedantic set from Cargo.toml's [lints]; any warning fails.
          clippy = package.overrideAttrs (old: {
            pname = "${old.pname}-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --frozen --all-targets -- --deny warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
            postFixup = "";
          });

          fmt =
            pkgs.runCommand "${package.pname}-fmt"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
              }
              ''
                cd ${src}
                cargo fmt --check
                touch $out
              '';
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
          ];
          LD_LIBRARY_PATH = lib.makeLibraryPath (runtimeLibs pkgs true);
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
