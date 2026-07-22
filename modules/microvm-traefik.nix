{ config, lib, pkgs, inputs, ... }:

let

  tapId = "vm-traefik";
in
{
  # QEMU MicroVM running Traefik, directly attached to the existing br-lan bridge.
  microvm.vms.traefik = {
    # Reuse the host's package set so /nix/store paths match the host's.
    inherit pkgs;

    config = {
      # Publish this VM as `traefik.local` on the LAN via mDNS, so other LAN
      # clients and the host can resolve it without a static entry.
      imports = [ inputs.mdns-publisher.nixosModules.default ];

      networking.usePredictableInterfaceNames = false;

      services.mdns-publisher = {
        enable = true;
        interface = "eth0";
        hostname = "traefik";
        openFirewall = true;
      };

      # microvm.nix disables systemd-networkd-wait-online for boot speed
      # (optimization.nix), which makes network-online.target fire almost
      # instantly — before eth0 actually exists. mdns-publisher's own
      # `after network-online.target` then races and fails with
      # "Link not found". Gate it on the eth0 device unit instead, which
      # only becomes active once udev has registered the interface.
      systemd.services.mdns-publisher = {
        after = [ "sys-subsystem-net-devices-eth0.device" ];
        bindsTo = [ "sys-subsystem-net-devices-eth0.device" ];
      };

      networking.hostName = "traefik";
      system.stateVersion = "26.05";
      time.timeZone = config.time.timeZone;

      # Fixed machine-id so the host can identify this VM's journals.
      microvm.machineId = "70aef1c0-0000-0000-0000-000000000000";

      # VSOCK needs the virtio transport loaded before systemd-ssh-generator
      # runs, otherwise AF_VSOCK is undetectable and sshd-vsock.socket never
      # gets created. microvm.nix's default initrd modules omit these.
      boot.initrd.kernelModules = [ "vsock" "vmw_vsock_virtio_transport" ];
      boot.kernelModules = [ "vsock" "vmw_vsock_virtio_transport" ];

      services.resolved.enable = false;
      networking.nameservers = [ "fdea:d:beef::1" ];

      # Minimal base: systemd-networkd is enabled via microvm.optimize by default,
      # but we declare it explicitly below.
      microvm = {
        hypervisor = "qemu";

        # SSH backdoor reachable from the host via `microvm -s traefik`
        # (= `ssh vsock/3`), no network path needed. CIDs 0-2 are reserved.
        vsock.cid = 3;
        vsock.ssh.enable = true;
        vcpu = 2;
        mem = 512;
        balloon = true;

        # With a virtiofs share of /nix/store we do not need to embed the store
        # in the boot squashfs. This makes the VM tiny and guarantees it reuses
        # the host's /nix/store byte-for-byte.
        storeOnDisk = false;

        shares = [
          {
            proto = "virtiofs";
            tag = "ro-store";
            source = "/nix/store";
            mountPoint = "/nix/.ro-store";
            readOnly = true;
          }
          {
            proto = "virtiofs";
            tag = "traefik-data";
            # Relative to /var/lib/microvms/traefik on the host.
            source = "traefik-data";
            mountPoint = "/var/lib/traefik";
            socket = "traefik-data.sock";
          }
          {
            proto = "virtiofs";
            tag = "journal";
            # Source path relative to microvm.stateDir
            source = "journal";
            mountPoint = "/var/log/journal";
            socket = "journal.sock";
          }
        ];

        interfaces = [
          {
            type = "tap";
            id = tapId;
            mac = "02:00:00:77:65:62";
            # vhost-net acceleration is qemu-only and gives ~10 Gbps vs ~1.5 Gbps.
            tap.vhost = true;
          }
        ];

        # The default tap-up script creates the tap and brings it up, but does not
        # attach it to any bridge. Attach it to br-lan so the VM shares the LAN
        # segment with wired clients.
        binScripts.tap-up = lib.mkAfter ''
          ${lib.getExe' pkgs.iproute2 "ip"} link set dev '${tapId}' master br-lan
        '';
      };

      systemd.network.enable = true;

      # Bring up the first virtio ethernet adapter with a static LAN IPv6 address.
      # IPv6 default routes and DNS come from radvd on the host (IPv6AcceptRA).
      systemd.network.wait-online.enable = true;
      systemd.network.networks."10-lan" = {
        matchConfig.Name = "eth0";
        linkConfig.RequiredForOnline = "routable";
        networkConfig = {
          DHCP = "no";
          IPv6AcceptRA = true;
          IPv6PrivacyExtensions = "no";
          DNS = [ "fdea:d:beef::1" ];
        };
      };

      # Traefik reverse proxy / edge router.
      services.traefik = {
        enable = true;
        group = "traefik";
        dataDir = "/var/lib/traefik";
        environmentFiles = [ "/var/lib/traefik/traefik-env" ];

        staticConfigOptions = {
          global = {
            checkNewVersion = false;
            sendAnonymousUsage = false;
          };

          api.dashboard = true;

          entryPoints.web = {
            address = ":80";
            http.redirections.entryPoint = {
              to = "websecure";
              scheme = "https";
              permanent = true;
            };
          };
          entryPoints.websecure = {
            address = ":443";
          };
          entryPoints.dashboard = {
            address = ":8443";
          };
          # UDP entry point for AmneziaWG
          entryPoints.awg-udp = {
            address = ":47999/udp";
          };

          certificatesResolvers.letsencrypt.acme = {
            email = "postmaster+traefik@c23.me";
            storage = "${config.services.traefik.dataDir}/acme.json";
            tlsChallenge = {};
          };

          log = {
            level = "INFO";
          };
        };

        # Dynamic configuration: add routers/services here as you add backends.
        dynamicConfigOptions = {
          http.routers.dashboard = {
            rule = "Host(`traefik.local`) && (PathPrefix(`/api`) || PathPrefix(`/dashboard`))";
            service = "api@internal";
            entryPoints = [ "dashboard" ];
          };
          udp.routers.awg = {
            entryPoints = [ "awg-udp" ];
            service = "awg-backend";
          };
          udp.services.awg-backend = {
            loadBalancer = {
              servers = [
                {
                  address = "[fdea:d:beef::1]:47999";
                }
              ];
            };
          };
        };
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/traefik 0750 traefik traefik -"
      ];

      users.users.traefik = {
        isSystemUser = true;
        group = "traefik";
      };
      users.groups.traefik = {};

      # microvm.vsock.ssh.enable turns on openssh. We want only the
      # vsock socket that systemd-ssh-generator creates at boot
      # (sshd-vsock.socket on vsock::22), not openssh's own network
      # listener. Disable both forms of it:
      #   - the long-running sshd.service (!startWhenNeeded path)
      #   - the TCP sshd.socket (startWhenNeeded path)
      # sshd-keygen.service and the sshd@ per-connection template stay,
      # which is what sshd-vsock@.service reuses.
      services.openssh.startWhenNeeded = true;
      systemd.services.sshd.enable = false;
      systemd.sockets.sshd.enable = false;

      # Empty-password root login for the VSOCK ssh path and the serial
      # console getty. Safe because no TCP listener is exposed.
      services.openssh.settings.PermitRootLogin = "yes";
      services.openssh.settings.PasswordAuthentication = true;
      services.openssh.settings.PermitEmptyPasswords = "yes";
      security.pam.services.sshd.allowNullPassword = true;
      users.users.root.password = "";
      services.getty.autologinUser = "root";

      networking.nftables.enable = true;
      networking.firewall = {
        enable = true;
        allowedTCPPorts = [ 80 443 8443 ];
        allowedUDPPorts = [ 47999 ];
      };

      services.cloudflare-ddns = {
        enable = true;
        domains = [ "." ];  # dummy, real domains come from env file
        credentialsFile = "/var/lib/traefik/cloudflare-ddns.env";
      };
      systemd.services.cloudflare-ddns.serviceConfig.Environment = lib.mkForce [];
      systemd.services.cloudflare-ddns.serviceConfig.EnvironmentFile = lib.mkForce [
        "/var/lib/traefik/cloudflare-ddns.env"
      ];

    };
  };

  # Decrypt directly to `path` (not a symlink into /run/agenix on the
  # host): the VM reads it through a virtiofs share of traefik-data/, and
  # virtiofsd can't follow symlinks pointing outside the shared directory.
  age.secrets.traefik-env = {
    file = ../secrets/traefik-env.age;
    path = "/var/lib/microvms/traefik/traefik-data/traefik-env";
    owner = "root";
    group = "root";
    mode = "0640";
    symlink = false;
  };

  age.secrets.traefik-ddns-env = {
    file = ../secrets/traefik-ddns-env.age;
    path = "/var/lib/microvms/traefik/traefik-data/cloudflare-ddns.env";
    owner = "root";
    group = "root";
    mode = "0640";
    symlink = false;
  };

  systemd.tmpfiles.rules = [
    "d ${config.microvm.stateDir}/traefik/journal 0755 root root -"
    # Symlink this VM's journal dir into the host's so `journalctl --merge` sees it.
    "L+ /var/log/journal/70aef1c0000000000000000000000000 - - - - ${config.microvm.stateDir}/traefik/journal/70aef1c0000000000000000000000000"
  ];

  # DMZ: allow all WAN traffic to the Traefik VM, matched by the EUI-64
  # interface ID derived from its MAC so it's independent of the delegated prefix.
  networking.nftables.firewall = {
    zones.traefik = {
      parent = "lan";
      ingressExpression = [
        "ip6 saddr & ::ffff:ffff:ffff:ffff == ::ff:fe77:6562"
      ];
      egressExpression = [
        "ip6 daddr & ::ffff:ffff:ffff:ffff == ::ff:fe77:6562"
      ];
    };
    rules.wan-to-traefik = {
      from = [ "wan" ];
      to = [ "traefik" ];
      verdict = "accept";
    };
    rules.wan-to-traefik-ban-dashboard = {
      ruleType = "ban";
      from = [ "wan" ];
      to = [ "traefik" ];
      extraLines = [ "tcp dport 8443 drop" ];
    };
    # Allow Traefik to forward UDP traffic to host's AmneziaWG
    rules.traefik-to-fw-awg = {
      from = [ "traefik" ];
      to = [ "fw" ];
      allowedUDPPorts = [ 47999 ];
    };
    rules.traefik-to-fw-dns = {
      from = [ "traefik" ];
      to = [ "fw" ];
      allowedUDPPorts = [ 53 ];
      allowedTCPPorts = [ 53 ];
    };
    rules.lan-to-traefik = {
      from = [ "lan" ];
      to = [ "traefik" ];
      verdict = "accept";
    };
    # Allow LAN clients to query the Traefik VM's mDNS responder.
    rules.lan-to-traefik-mdns = {
      from = [ "lan" ];
      to = [ "traefik" ];
      allowedUDPPorts = [ 5353 ];
      extraLines = [
        "ip6 daddr ff02::fb udp dport 5353 accept"
      ];
    };

    rules.traefik-to-fw-mdns = {
      from = [ "traefik" ];
      to = [ "fw" ];
      allowedUDPPorts = [ 5353 ];
      extraLines = [
        "ip6 daddr ff02::fb udp dport 5353 accept"
      ];
    };
  };

  # Start the Traefik MicroVM automatically on host boot.
  microvm.autostart = [ "traefik" ];
}
