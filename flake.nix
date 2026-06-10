{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system:
        f { pkgs = import nixpkgs { inherit system; }; });
    in {
      packages = forAllSystems ({ pkgs }:
        let
          craneLib = crane.mkLib pkgs;
          src = craneLib.cleanCargoSource ./.;
          commonArgs = {
            inherit src;
            pname = "yt-dlp-api";
            version = "0.1.0";
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          binary = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });
        in {
          default = binary;

          oci-image = pkgs.dockerTools.buildLayeredImage {
            name = "yt-dlp-api";
            tag = "latest";
            contents = [
              binary
              pkgs.yt-dlp
              pkgs.ffmpeg-headless
              pkgs.cacert
            ];
            config = {
              Cmd = [ "${binary}/bin/yt-dlp-api" ];
              Env = [
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts."3000/tcp" = {};
            };
          };
        });
    };
}
