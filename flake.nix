{
  description = "GPUI shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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

      runtimeLibraryPath = pkgs.lib.makeLibraryPath runtimeLibs + ":/run/opengl-driver/lib";

      # The island renders both text and Nerd Font icons with the patched
      # Caskaydia Mono family. Vendoring it keeps the package and devshell
      # working even if it is removed from the host font configuration.
      caskaydiaMono = pkgs.nerd-fonts.caskaydia-mono;

      # Merge the host font configuration with the vendored font. Including
      # /etc/fonts/fonts.conf keeps system fallbacks (symbols, emoji, CJK)
      # available; `ignore_missing` means a host without that file still falls
      # back to just the bundled font. Merely adding the package to
      # buildInputs would not work: fontconfig only scans the directories in
      # its configuration.
      fontConfig = pkgs.writeText "gpui-shell-fonts.conf" ''
        <?xml version="1.0"?>
        <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
        <fontconfig>
          <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
          <dir>${caskaydiaMono}/share/fonts</dir>
        </fontconfig>
      '';

    in
    {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "gpui-shell";
        version = "0.1.0";

        src = ./.;

        env = {
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };

        cargoLock = {
          lockFile = ./Cargo.lock;
          outputHashes = {
            "collections-0.1.0" = "sha256-KIdwv14mh8ugwjswEotTEqNcuHChxwIWOp2biwa4jlM=";
            "wasm_thread-0.3.3" = "sha256-+lRLCIk0S6Y5ORYjDKsYYHia2FtoSoh+rWkQh7mnPBE=";
            "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
          };
        };

        nativeBuildInputs = with pkgs; [
          pkg-config
          makeWrapper
          clang
          llvmPackages.libclang
        ];

        buildInputs =
          with pkgs;
          [
            fontconfig
            libxkbcommon
            pipewire
          ]
          ++ runtimeLibs;

        postInstall = ''
          wrapProgram $out/bin/gpui-shell \
            --set LD_LIBRARY_PATH "${runtimeLibraryPath}" \
            --set FONTCONFIG_FILE "${fontConfig}"
        '';
      };

      devShells.${system}.default = pkgs.mkShell {
        buildInputs =
          with pkgs;
          [
            cargo
            rustc
            rust-analyzer
            rustfmt
            pkg-config
            fontconfig
            libxkbcommon
            pipewire
            clang
            llvmPackages.libclang
          ]
          ++ runtimeLibs;

        shellHook = ''
          export LD_LIBRARY_PATH="${runtimeLibraryPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
          export FONTCONFIG_FILE="${fontConfig}"
        '';
      };
    };
}
