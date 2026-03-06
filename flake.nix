{
  description = "shm2-rs: SHM-only transport + GStreamer plugin + shm2_relayd wrapper";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        version = cargoToml.package.version;

        shm2 = pkgs.rustPlatform.buildRustPackage {
          pname = "shm2-rs";
          inherit version;

          src = ./.;

          # NOTE: you must commit Cargo.lock to the repo for reproducible Nix builds.
          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          # Split outputs: binaries in "out", GStreamer plugin in "plugin"
          outputs = [ "out" "plugin" ];

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.makeWrapper
          ];

          buildInputs = [
            pkgs.glib
            pkgs.gst_all_1.gstreamer
            pkgs.gst_all_1.gst-plugins-base

            # Helpful at runtime for typical pipelines / examples
            pkgs.gst_all_1.gst-plugins-good
            pkgs.gst_all_1.gst-plugins-bad
            pkgs.gst_all_1.gst-plugins-ugly
          ];

          doCheck = false;

          # We want the cdylib (plugin) + bins.
          #cargoBuildFlags = [ "--release" ];

          installPhase = ''
            runHook preInstall

            mkdir -p $out/bin

            for b in shm2_producer shm2_consumer shm2_relayd; do
              p="$(find target -type f -name "$b" -perm -111 | head -n1)"
              if [ -z "$p" ]; then
                echo "Missing binary $b" >&2
                exit 1
              fi
              install -Dm755 "$p" "$out/bin/$b"
            done

            # Install plugin cdylib. On Linux this should be libgstshm2.so.
            mkdir -p $plugin/lib/gstreamer-1.0
            so="$(find target -type f \( -name 'libgstshm2.so' -o -name 'libgstshm2.dylib' -o -name 'gstshm2.dll' \) | head -n1)"
            if [ -z "$so" ]; then
              echo "Missing plugin library (libgstshm2.* / gstshm2.dll)" >&2
              exit 1
            fi
            install -Dm755 "$so" "$plugin/lib/gstreamer-1.0/$(basename "$so")"

            # Make shm2_relayd usable out-of-the-box: point it at this plugin + common gst plugin dirs.
            wrapProgram "$out/bin/shm2_relayd" \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "$plugin/lib/gstreamer-1.0" \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.gst_all_1.gstreamer}/lib/gstreamer-1.0" \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.gst_all_1.gst-plugins-base}/lib/gstreamer-1.0" \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.gst_all_1.gst-plugins-good}/lib/gstreamer-1.0" \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.gst_all_1.gst-plugins-bad}/lib/gstreamer-1.0" \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.gst_all_1.gst-plugins-ugly}/lib/gstreamer-1.0"

            runHook postInstall
          '';

          meta = with lib; {
            description = cargoToml.package.description or "shm2-rs";
            homepage = "https://github.com/peigongdsd/shm2-rs";
            license = licenses.mit;
            platforms = platforms.linux ++ platforms.darwin;
            mainProgram = "shm2_relayd";
          };
        };
      in
      {
        packages = {
          shm2-rs = shm2;
          default = shm2;
        };

        # `nix run` will execute shm2_relayd
        apps.default = flake-utils.lib.mkApp {
          drv = shm2;
          exePath = "/bin/shm2_relayd";
        };

        # Keep a dev shell similar to what the repo already had
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustc cargo rustfmt clippy
            pkg-config
            glib
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            gst_all_1.gst-plugins-bad
            gst_all_1.gst-plugins-ugly
            gst_all_1.icamerasrc-ipu6epmtl
          ];
        };
      });
}
