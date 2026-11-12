{
  description = "OpenMouse Bridge for NixOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      packages = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
          nativeHid = pkgs.buildNpmPackage {
            pname = "openmouse-bridge-native-hid";
            version = "0.1.0";
            src = "${self}/native-hid";
            npmDepsHash = "sha256-JMsspevf1MunaufbYZY2t8kCX6WuxowlFi4wKn/F1Qw=";
            makeCacheWritable = true;
            forceGitDeps = true;
            npmInstallFlags = [ "--ignore-scripts=false" ];
            nativeBuildInputs = with pkgs; [ nodejs_24 pkg-config python3 ];
            buildInputs = with pkgs; [ libusb1 systemd ];
            dontNpmBuild = true;
          };
          bridge = pkgs.rustPlatform.buildRustPackage {
            pname = "openmouse-bridge";
            version = "1.0.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgs; [ pkg-config cmake clang makeWrapper ];
            buildInputs = with pkgs; [
              openssl systemd wayland libxkbcommon
              libX11 libxcb libGL fontconfig freetype alsa-lib vulkan-loader
            ];
            preCheck = ''
              export OPENMOUSE_TEST_SLEEP=${pkgs.coreutils}/bin/sleep
            '';
            postInstall = ''
              mkdir -p $out/bin/native-hid $out/share/openmouse-bridge $out/share/icons/hicolor/512x512/apps $out/share/applications
              cp -r ${self}/native-hid/. $out/bin/native-hid/
              cp -r ${nativeHid}/lib/node_modules/openmouse-bridge-native-hid/node_modules $out/bin/native-hid/
              mkdir -p $out/bin/native-hid/node/bin
              cp -r ${self}/assets/. $out/share/openmouse-bridge/
              ln -s ${pkgs.nodejs_24}/bin/node $out/bin/native-hid/node/bin/node
              cp ${self}/assets/openmouse-app-icon.png $out/share/icons/hicolor/512x512/apps/io.openmouse.bridge.png
              cat > $out/share/applications/io.openmouse.bridge.desktop <<DESKTOP
              [Desktop Entry]
              Type=Application
              Name=OpenMouse Bridge
              Comment=Local companion for the OpenMouse control panel
              Exec=$out/bin/openmouse-bridge
              Icon=io.openmouse.bridge
              Categories=Utility;
              Terminal=false
              DESKTOP
              wrapProgram $out/bin/openmouse-bridge \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [
                  pkgs.systemd pkgs.wayland pkgs.libxkbcommon
                  pkgs.libX11 pkgs.libxcb pkgs.libGL pkgs.fontconfig pkgs.freetype
                  pkgs.alsa-lib pkgs.vulkan-loader pkgs.libusb1 pkgs.stdenv.cc.cc.lib
                ]} \
                --prefix XDG_DATA_DIRS : /run/opengl-driver/share
            '';
            nativeHID = null;
            meta = with pkgs.lib; {
              description = "Local native HID and desktop companion for OpenMouse";
              homepage = "https://openmouse.app/";
              license = licenses.mit;
              platforms = platforms.linux;
              mainProgram = "openmouse-bridge";
            };
          };
        in {
          default = bridge;
          openmouse-bridge = bridge;
        });

      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/openmouse-bridge";
        };
      });
    };
}
