# Edit this configuration file to define what should be installed on
# your system. Help is available in the configuration.nix(5) man page, on
# https://search.nixos.org/options and in the NixOS manual (`nixos-help`).

{ config, lib, pkgs, inputs, ... }:

{
  # Use the systemd-boot EFI boot loader.
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

  # Use latest kernel.
  # boot.kernelPackages = pkgs.linuxPackages_latest;

  networking.hostName = "nixos"; # Define your hostname.

  # Configure network connections interactively with nmcli or nmtui.
  # networking.networkmanager.enable = true;

  # Set your time zone.
  time.timeZone = "Asia/Shanghai";

  # Configure network proxy if necessary
  # networking.proxy.default = "http://user:password@proxy:port/";
  # networking.proxy.noProxy = "127.0.0.1,localhost,internal.domain";

  # Select internationalisation properties.
  # i18n.defaultLocale = "en_US.UTF-8";
  # console = {
  #   font = "Lat2-Terminus16";
  #   keyMap = "us";
  #   useXkbConfig = true; # use xkb.options in tty.
  # };

  # Enable the X11 windowing system.
  # services.xserver.enable = true;




  # Configure keymap in X11
  # services.xserver.xkb.layout = "us";
  # services.xserver.xkb.options = "eurosign:e,caps:escape";

  # Enable CUPS to print documents.
  # services.printing.enable = true;

  # Enable sound.
  # services.pulseaudio.enable = true;
  # OR
  # services.pipewire = {
  #   enable = true;
  #   pulse.enable = true;
  # };

  # Enable touchpad support (enabled default in most desktopManager).
  # services.libinput.enable = true;

  # Define a user account. Don't forget to set a password with ‘passwd’.
  # users.users.alice = {
  #   isNormalUser = true;
  #   extraGroups = [ "wheel" ]; # Enable ‘sudo’ for the user.
  #   packages = with pkgs; [
  #     tree
  #   ];
  # };

  # programs.firefox.enable = true;

  # List packages installed in system profile.
  # You can use https://search.nixos.org/ to find more packages (and options).
  environment.systemPackages = with pkgs; [
    nano # Do not forget to add an editor to edit configuration.nix! The Nano editor is also installed by default.
    git
    age
  ];

  age = {
    identityPaths = [ "/var/lib/agenix/key.txt" ];
    secrets.ppp-secrets = {
      file = ./secrets/ppp-secrets.age;
      path = "/etc/ppp/chap-secrets";
    };
    secrets.ppp-name = {
      file = ./secrets/ppp-name.age;
      path = "/etc/ppp/name";
    };
    secrets.dae-sub = {
      file = ./secrets/dae-sub.age;
      path = "/etc/dae/local.sub";
    };
  };
  # networking.useDHCP = false;
  services.pppd = {
    enable = true;
    peers = {
      pppoe = {
        autostart = true;
        enable = true;
        config = ''
          plugin pppoe.so enp2s0f0
          file /etc/ppp/name
          noauth
          persist
          maxfail 0
          holdoff 5
          defaultroute
          usepeerdns
          mtu 1492
          +ipv6
          ipv6cp-accept-local
          ifname ppp0
        '';
      };
    };
  };

  environment.etc."ppp/resolv.conf".source = "/run/ppp/resolv.conf";
  systemd.tmpfiles.rules = [ "d /run/ppp 0755 root root -" "f /run/ppp/resolv.conf 0644 root root -" ];

  systemd.services."pppd-pppoe" = {
    after = [ "agenix.service" ];
    wants = [ "agenix.service" ];
    preStart = "${pkgs.iproute2}/bin/ip link set enp2s0f0 up";
  };

  boot.kernel.sysctl = {
    "net.ipv6.conf.all.forwarding" = 1;
    "net.ipv6.conf.all.accept_ra" = 2;
    "net.ipv4.conf.all.forwarding" = 1;
    # see https://github.com/daeuniverse/dae/blob/main/docs/en/user-guide/kernel-parameters.md
    "net.ipv4.conf.br-lan.send_redirects" = 0;
    "net.ipv4.ip_forward" = 1;
  };

  networking.bridges = {
    "br-lan".interfaces = [ "enp2s0f1" ];
  };

  networking.interfaces."br-lan" = {
    ipv6.addresses = [ { address = "fdea:d:beef::1"; prefixLength = 64; } ];
  };

  networking.dhcpcd = {
    enable = true;
    allowInterfaces = [ "ppp0" ];
    extraConfig = ''
      noipv4
      interface ppp0
        ipv6rs
        ia_na
        iaid 1
        ia_pd 1 br-lan/0
    '';
  };

  services.radvd = {
    enable = true;
    config = ''
      interface br-lan {
        AdvSendAdvert on;
        MinRtrAdvInterval 30;
        MaxRtrAdvInterval 100;
        prefix ::/64 {
          AdvOnLink on;
          AdvAutonomous on;
          AdvRouterAddr on;
        };
        RDNSS fdea:d:beef::1 {
        };
      };
    '';
  };

  # Install china domain list from flake package
  environment.etc."smartdns/china-domain-list.txt" = {
    source = "${inputs.dnsmasq-china-list.packages.${pkgs.stdenv.hostPlatform.system}.default}/etc/smartdns/china-domain-list.txt";
  };

  # SmartDNS: upstream resolver for dae, handles DNS64 and anti-pollution
  services.smartdns = {
    enable = true;
    bindPort = 53;
    settings = {
      bind = "[::]:53";
      cache-size = 4096;
      server = [
        "223.5.5.5 -group alidns"
        "223.6.6.6 -group alidns"
        "8.8.8.8"
        "8.8.4.4"
      ];
      dns64 = "64:ff9b::/96";
      prefetch-domain = true;
      speed-check-mode = "none";
      dualstack-ip-selection = false;
      "force-AAAA-SOA" = false;
      # conf-file must come before domain-set/domain-rules alphabetically
      conf-file = "/etc/smartdns/china-rules.conf";
    };
  };

  # Separate file for domain-set and domain-rules to ensure ordering
  environment.etc."smartdns/china-rules.conf" = {
    text = ''
      domain-set -name china-list -type list -file /etc/smartdns/china-domain-list.txt
      domain-rules /domain-set:china-list/ -nameserver alidns
    '';
  };

  # TAYGA stateless NAT64 (Well-Known Prefix 64:ff9b::/96)
  services.tayga = {
    enable = true;
    ipv4 = {
      address = "192.168.255.1";
      router.address = "192.168.255.1";
      pool = {
        address = "192.168.255.0";
        prefixLength = 24;
      };
    };
    ipv6 = {
      # Source address of the TAYGA server; must NOT reside inside the NAT64 prefix
      address = "fdea:d:beef::1";
      router.address = "64:ff9b::1";
      pool = {
        address = "64:ff9b::";
        prefixLength = 96;
      };
    };
  };

  # Some programs need SUID wrappers, can be configured further or are
  # started in user sessions.
  # programs.mtr.enable = true;
  # programs.gnupg.agent = {
  #   enable = true;
  #   enableSSHSupport = true;
  # };

  # List services that you want to enable:

  # Enable the OpenSSH daemon.
  services.openssh = { enable = true; settings.PermitRootLogin = "yes"; };

  # Open ports in the firewall.
  # networking.firewall.allowedTCPPorts = [ ... ];
  # networking.firewall.allowedUDPPorts = [ ... ];
  # Or disable the firewall altogether.
  networking.firewall.enable = false;

  networking.nftables.firewall = {
    enable = true;
    snippets.nnf-common.enable = true;
    zones = {
      wan = { interfaces = [ "ppp0" ]; };
      lan = { interfaces = [ "br-lan" ]; };
      nat64 = { interfaces = [ "nat64" ]; };
    };
    rules = {
      lan-to-wan = { from = [ "lan" ]; to = [ "wan" ]; verdict = "accept"; };
      lan-to-nat64 = { from = [ "lan" ]; to = [ "nat64" ]; verdict = "accept"; };
      nat64-to-wan = { from = [ "nat64" ]; to = [ "wan" ]; verdict = "accept"; masquerade = true; };
      lan-to-fw-ipv6 = { from = [ "lan" ]; to = [ "fw" ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 from LAN\"" ]; };
      lan-to-fw-dns = { from = [ "lan" ]; to = [ "fw" ]; allowedUDPPorts = [ 53 ]; allowedTCPPorts = [ 53 ]; };
      wan-to-fw-ipv6 = { from = [ "wan" ]; to = [ "fw" ]; allowedUDPPorts = [ 546 ]; extraLines = [ "meta l4proto icmpv6 accept comment \"Allow ICMPv6 for RAs and ND\"" ]; };
    };
  };

  services.dae = {
    enable = true;
    openFirewall = {
      enable = true;
      port = 10800;
    };
    config = ''
      global {
        tproxy_port: 10800
        wan_interface: ppp0 # Use "auto" to auto detect WAN interface.
        lan_interface: br-lan,nat64

        log_level: info
        allow_insecure: false
        auto_config_kernel_parameter: false
      }

      subscription {
        'file://local.sub'
      }

      group {
        proxy {
          policy: fixed(0)
        }
      }
      dns {
        upstream {
          smartdns: 'udp://127.0.0.1:53'
        }
        routing {
          request {
            fallback: smartdns
          }
        }
      }
      routing {
        pname(NetworkManager) -> direct
        dip(224.0.0.0/3, 'ff00::/8') -> direct
        dip(geoip:private) -> direct
        pname(smartdns) && dport(53) -> must_direct

        dip(geoip:cn) -> direct
        domain(geosite:cn) -> direct
        # dip('64:ff9b::/96') -> direct

        fallback: proxy
      }
    '';
  };

  # Copy the NixOS configuration file and link it from the resulting system
  # (/run/current-system/configuration.nix). This is useful in case you
  # accidentally delete configuration.nix.
  # system.copySystemConfiguration = true;

  fileSystems = {
    "/nix".options = [ "compress=zstd" "noatime" ];
    "/swap".options = [ "noatime" "nodatacow" ];
  };

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  nix.settings.substituters = lib.mkForce [
    "https://mirror.sjtu.edu.cn/nix-channels/store"
    "https://nix-community.cachix.org"
  ];

  nix.settings.trusted-public-keys = [
    "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="
  ];

  # This option defines the first version of NixOS you have installed on this particular machine,
  # and is used to maintain compatibility with application data (e.g. databases) created on older NixOS versions.
  #
  # Most users should NEVER change this value after the initial install, for any reason,
  # even if you've upgraded your system to a new NixOS release.
  #
  # This value does NOT affect the Nixpkgs version your packages and OS are pulled from,
  # so changing it will NOT upgrade your system - see https://nixos.org/manual/nixos/stable/#sec-upgrading for how
  # to actually do that.
  #
  # This value being lower than the current NixOS release does NOT mean your system is
  # out of date, out of support, or vulnerable.
  #
  # Do NOT change this value unless you have manually inspected all the changes it would make to your configuration,
  # and migrated your data accordingly.
  #
  # For more information, see `man configuration.nix` or https://nixos.org/manual/nixos/stable/options#opt-system.stateVersion .
  system.stateVersion = "26.05"; # Did you read the comment?

}
