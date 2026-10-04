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
              # Shared compilation cache: every workspace compiles the same
              # registry crates with the same flags, so sccache turns a fresh
              # workspace's cold build into cache hits. Wired up by
              # .cargo/config.toml; see AGENTS.md, "Compile caching".
              sccache
              # Reclaims stale artifacts from per-workspace target dirs.
              cargo-sweep
              python3
              uv
              pkg-config
              cmake
              ninja
              gdb
              tracy
            ];

            buildInputs = with pkgs; [
              cuda.cudatoolkit
              alsa-lib
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
              # One cache shared by every workspace and every agent. Capped so
              # it cannot grow without bound on a disk that is already tight;
              # sccache evicts least-recently-used entries past the cap. The
              # dependency build for this workspace is ~2G, so 10G holds it
              # with room for a second toolchain.
              export SCCACHE_DIR="''${SCCACHE_DIR:-$HOME/.cache/sccache}"
              export SCCACHE_CACHE_SIZE="''${SCCACHE_CACHE_SIZE:-10G}"
              export LD_LIBRARY_PATH="/run/opengl-driver/lib:${
                pkgs.lib.makeLibraryPath [
                  cuda.cudatoolkit
                  pkgs.alsa-lib
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
