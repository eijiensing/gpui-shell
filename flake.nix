{
  description = "GPUI shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      system = "x86_64-linux";

      pkgs = import nixpkgs {
        inherit system;
        config.allowUnfree = true;
      };

      # gpui on Linux loads these at runtime instead of linking them:
      #  - wayland: gpui_linux enables the wayland `client_system` + `dlopen`
      #    backend, so it dlopen()s libwayland-client.so.0 (otherwise you get
      #    `NoWaylandLib`).
      #  - vulkan-loader / libglvnd: the wgpu renderer dlopens the Vulkan
      #    loader and the GL/EGL dispatch libraries to create the surface.
      # dlopen() does not reliably honor the RUNPATH the toolchain bakes into
      # the binary (it only searches RPATH), so put these on LD_LIBRARY_PATH.
      runtimeLibs = with pkgs; [
        wayland
        vulkan-loader
        libglvnd
        mesa
      ];

      runtimeLibraryPath =
        pkgs.lib.makeLibraryPath runtimeLibs
        + ":/run/opengl-driver/lib";

    in {
      packages.${system}.default =
        pkgs.rustPlatform.buildRustPackage {
          pname = "gpui-shell";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            fontconfig
            libxkbcommon
          ] ++ runtimeLibs;

          postInstall = ''
            wrapProgram $out/bin/gpui-shell \
              --set LD_LIBRARY_PATH "${runtimeLibraryPath}"
          '';
        };

      devShells.${system}.default = pkgs.mkShell {
        buildInputs = with pkgs; [
          cargo
          rustc
          rust-analyzer
          rustfmt
          pkg-config
          fontconfig
          libxkbcommon
        ] ++ runtimeLibs;

        shellHook = ''
          export LD_LIBRARY_PATH="${runtimeLibraryPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        '';
      };
    };
}
