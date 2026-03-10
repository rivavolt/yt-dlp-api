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

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.yt-dlp-api;
          ociImage = self.packages.${pkgs.stdenv.hostPlatform.system}.oci-image;
        in {
          options.services.yt-dlp-api = {
            enable = lib.mkEnableOption "yt-dlp HTTP API";

            tsAuthKeyFile = lib.mkOption {
              type = lib.types.path;
              description = "Path to file containing the Tailscale auth key";
            };

            loginServer = lib.mkOption {
              type = lib.types.str;
              default = "https://hs.avolt.net";
              description = "Headscale login server URL";
            };
          };

          config = lib.mkIf cfg.enable {
            virtualisation.oci-containers.backend = "docker";

            # Load OCI image into docker
            systemd.services.yt-dlp-api-image-load = {
              description = "Load yt-dlp-api OCI image into Docker";
              after = [ "docker.service" ];
              requires = [ "docker.service" ];
              before = [ "docker-yt-dlp-api.service" ];
              requiredBy = [ "docker-yt-dlp-api.service" ];
              serviceConfig = {
                Type = "oneshot";
                RemainAfterExit = true;
                ExecStart = "${pkgs.docker}/bin/docker load -i ${ociImage}";
              };
            };

            virtualisation.oci-containers.containers = {
              yt-dlp-api = {
                image = "yt-dlp-api:latest";
                extraOptions = [
                  "--cap-add=NET_ADMIN"
                  "--device=/dev/net/tun:/dev/net/tun"
                ];
              };

              yt-dlp-api-ts = {
                image = "tailscale/tailscale:latest";
                environment = {
                  TS_STATE_DIR = "/var/lib/tailscale";
                  TS_HOSTNAME = "yt-dlp-api";
                  TS_EXTRA_ARGS = "--login-server=${cfg.loginServer}";
                };
                environmentFiles = [ cfg.tsAuthKeyFile ];
                volumes = [
                  "yt-dlp-api-ts-state:/var/lib/tailscale"
                  "/dev/net/tun:/dev/net/tun"
                ];
                extraOptions = [
                  "--network=container:yt-dlp-api"
                  "--cap-add=NET_ADMIN"
                  "--cap-add=NET_RAW"
                ];
                dependsOn = [ "yt-dlp-api" ];
              };
            };
          };
        };
    };
}
