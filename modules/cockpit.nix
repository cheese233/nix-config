{ config, lib, pkgs, ... }:

{
  services.cockpit = {
    enable = true;
    settings = {
      WebService = {
        AllowUnencrypted = true;
      };
    };
  };
  services.nginx.enable = true;

  services.nginx.virtualHosts."lan" = {
    locations."= /" = {
      return = "301 https://$host:9090/";
    };
  };
}
