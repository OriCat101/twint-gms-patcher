{
  description = "TWINT GMS-gate patcher: removes ch.twint.payment's Google Play services launch gate";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "aarch64-linux";
      # apktool 3.0.3 is flagged unsupported-system / insecure in nixpkgs; allow it here only.
      pkgs = import nixpkgs {
        inherit system;
        config = {
          allowUnsupportedSystem = true;
          permittedInsecurePackages = [ "apktool-3.0.3" ];
        };
      };
    in {
      devShells.${system}.default = pkgs.mkShell {
        name = "twint-gms-patcher";
        packages = with pkgs; [
          cargo
          rustc
          gcc              # cc, needed as Rust's linker
          apktool          # smali decode + dex assembly
          apksigner        # v2/v3 APK signing
          zip              # inject the rebuilt dex / strip old v1 signature
          unzip
          jdk              # keytool, for the throwaway signing key
          android-tools    # adb
          # zipalign is intentionally omitted: it ships only in the unfree Android
          # build-tools, and the APK has extractNativeLibs="true" so alignment is not
          # required to install. The patcher signs without it when it's not on PATH.
        ];
        shellHook = ''
          echo "twint-gms-patcher devshell — tools: apktool apksigner zipalign keytool adb cargo"
          echo "usage: cargo run -- <pull|decode|patch|build|sign|all|install --install>"
        '';
      };
    };
}
