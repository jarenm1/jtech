{
  description = "Rust, Python, and CUDA development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            config.allowUnfree = true;
          };
          cuda = pkgs.cudaPackages_12_9;
        in
        {
          formatter = pkgs.nixfmt;

          devShells.default = (pkgs.mkShell.override { stdenv = cuda.backendStdenv; }) {
            packages = with pkgs; [
              rustc
              cargo
              rustfmt
              clippy
              rust-analyzer
              python3
              uv
              pkg-config
              cmake
              ninja
              gdb
            ];

            buildInputs = with pkgs; [
              cuda.cudatoolkit
              wayland
              libxkbcommon
              vulkan-loader
              libGL
              libx11
              libxcursor
              libxi
              libxrandr
            ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            CUDA_PATH = "${cuda.cudatoolkit}";
            CUDA_HOME = "${cuda.cudatoolkit}";
            UV_PYTHON_DOWNLOADS = "never";
            UV_PYTHON = "${pkgs.python3}/bin/python3";

            # Use the host driver; the toolkit does not provide a kernel driver.
            shellHook = ''
              export LD_LIBRARY_PATH="/run/opengl-driver/lib:${
                pkgs.lib.makeLibraryPath [
                  cuda.cudatoolkit
                  pkgs.stdenv.cc.cc
                  pkgs.wayland
                  pkgs.libxkbcommon
                  pkgs.vulkan-loader
                  pkgs.libGL
                  pkgs.libx11
                  pkgs.libxcursor
                  pkgs.libxi
                  pkgs.libxrandr
                ]
              }''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            '';
          };
        };
    };
}
