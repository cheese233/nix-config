{ config, lib, pkgs, ... }:

# Declarative LXC domains for libvirt: Nix data in, libvirt domain XML out.
#
#   virtualisation.libvirtLxc.containers.ubuntu = {
#     vcpus  = 2;
#     memory = 5120;                            # MiB
#     swap   = 4096;                            # MiB on top of memory (see below)
#     rootfs = "/var/lib/lxc/ubuntu/rootfs";    # a directory with a real userland
#     mac    = "02:00:00:00:00:08";
#   };
#
# The Nix side produces a plain data tree (`tree`), `builtins.toXML` serialises
# it, and ./libvirt-domain.xsl maps that onto the libvirt schema at build time.
# No XML is written by hand: `nix eval --json ...tree` shows the data,
# `nix build ...xml` shows the result.
#
# The XML is installed as /var/lib/libvirt/lxc/<name>.xml with an autostart
# symlink beside it -- where libvirt's LXC driver looks
# (`virDomainObjListLoadAllConfigs` against `cfg->configDir`/`cfg->autostartDir`).
# Note it is not /etc/libvirt/lxc: nixpkgs builds libvirt with
# --sysconfdir=/var/lib.
#
# Deliberately not imported by hosts/nixos.nix at the time of writing: the
# incus variant lives in modules/lxc-ubuntu.nix and both would claim the same
# MAC on br-lan.  Enable one or the other.
let
  cfg = config.virtualisation.libvirtLxc;

  containerModule = { name, config, ... }: {
    options = {
      vcpus = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
        description = ''
          vCPUs advertised to the domain (`<vcpu placement='static'>`).
          Note this is *not* a limit for the LXC driver: libvirt only feeds
          `def->cpumask` into cpuset, and the cpumask comes from a `<vcpu
          cpuset='...'>` attribute, which this module does not emit.  Use
          `cpuQuota` to actually cap CPU time.
        '';
      };

      cpuQuota = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = 200000;
        example = 100000;
        description = ''
          CPU bandwidth cap in microseconds per `cpuPeriod`, emitted as
          `<cputune><quota>`.  quota / period = number of full CPUs worth of CPU
          time, so the default 200000/100000 means "2 CPUs" -- the same thing as
          podman's `--cpus=2`, landing in cgroup v2 as `cpu.max = 200000 100000`.

          This is a rate limit, not a pin: the container still sees every host
          CPU, a single-threaded task still runs at full speed, and only when it
          wants more than 2 CPUs' worth does it get throttled (visible in
          `cpu.stat` as nr_throttled).  Set to null to remove the cap.
        '';
      };

      cpuPeriod = lib.mkOption {
        type = lib.types.ints.positive;
        default = 100000;
        description = "Accounting window for `cpuQuota`, in microseconds.";
      };

      memory = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1024;
        description = "Memory limit in MiB; the stylesheet writes KiB.";
      };

      swap = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.unsigned;
        default = null;
        example = 4096;
        description = ''
          Extra swap allowance in MiB, emitted as <memtune><swap_hard_limit>.
          Verified on this host to reach the kernel: libvirt writes it to
          memory.swap.max in the domain's `libvirt` child cgroup on cgroup v2.
        '';
      };

      init = lib.mkOption {
        type = lib.types.str;
        default = "/sbin/init";
        description = "Init binary inside `rootfs`.";
      };

      rootfs = lib.mkOption {
        type = lib.types.str;
        description = ''
          Host directory holding the container's root filesystem.  It must
          contain a real userland; producing that is out of scope here
          (debootstrap, or unpacking a distro rootfs tarball).
        '';
      };

      bridge = lib.mkOption {
        type = lib.types.str;
        default = "br-lan";
        description = "Host bridge for the container interface.";
      };

      mac = lib.mkOption {
        type = lib.types.str;
        description = "Interface MAC (02: = locally administered).";
      };

      ifname = lib.mkOption {
        type = lib.types.str;
        default = "eth0";
        description = "Interface name inside the container.";
      };

      autostart = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Start the domain when libvirtd starts.";
      };

      extraXML = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "<hostdev mode='capabilities' type='misc'><source><char>net/tun</char></source></hostdev>" ];
        description = ''
          Raw XML appended inside <devices>, injected with
          disable-output-escaping.  This is the only unescaped channel, so keep
          it to markup you wrote yourself.
        '';
      };

      tree = lib.mkOption {
        type = lib.types.attrs;
        readOnly = true;
        description = ''
          The data handed to `builtins.toXML`.  Inspect with
          `nix eval --json ...tree`, feed to the stylesheet with
          `nix eval --json ...tree | toXML | xsltproc`.
        '';
      };

      xml = lib.mkOption {
        type = lib.types.package;
        readOnly = true;
        description = "Build result holding the generated libvirt domain XML.";
      };
    };

    # Null means "not set", and toXML would serialise it as <null/>, so drop it.
    config.tree = {
      inherit (config) vcpus memory init rootfs bridge mac ifname extraXML;
      inherit name;
    }
    // lib.optionalAttrs (config.swap != null) { inherit (config) swap; }
    // lib.optionalAttrs (config.cpuQuota != null) { inherit (config) cpuQuota cpuPeriod; };

    config.xml = pkgs.runCommand "libvirt-lxc-${name}.xml" {
      nativeBuildInputs = [ pkgs.libxslt ];
      treeXML = pkgs.writeText "libvirt-lxc-${name}-tree.xml" (builtins.toXML config.tree);
    } ''
      xsltproc ${./libvirt-domain.xsl} "$treeXML" > $out
    '';
  };
in
{
  options.virtualisation.libvirtLxc.containers = lib.mkOption {
    type = lib.types.attrsOf (lib.types.submodule containerModule);
    default = { };
    description = "LXC domains to define for libvirtd, keyed by domain name.";
  };

  config = lib.mkIf (cfg.containers != { }) {
    virtualisation.libvirtd.enable = true;

    # `virsh lxc-enter-namespace` cannot work on this host: it writes its own
    # process into the domain scope's cgroup.procs, which returns EBUSY on
    # cgroup v2 because the scope already delegates the controllers to its
    # `libvirt` child ("no internal process" rule).  This does the same job with
    # nsenter.
    #
    # Getting the pid right is the subtle part: for LXC domains `virsh domid`
    # returns the pid of libvirt_lxc, which lives in the *host* namespaces (it is
    # a child of pid 1).  The container's init is libvirt_lxc's child and is the
    # process that owns the container's mount/uts/ipc/net/pid namespaces.
    environment.systemPackages = [
      (pkgs.writeShellScriptBin "lxc-enter" ''
        set -eu
        export PATH=${lib.makeBinPath [ pkgs.coreutils pkgs.gnugrep pkgs.procps ]}:$PATH

        uri=lxc:///
        virsh=${config.virtualisation.libvirtd.package}/bin/virsh
        nsenter=${pkgs.util-linux}/bin/nsenter

        # First argument is the domain if it names one; everything else is the
        # command to run.  With no domain, use the single running LXC domain.
        if [ "$#" -gt 0 ] && "$virsh" -c "$uri" list --name | grep -qxF -- "$1"; then
          domain=$1
          shift
        else
          domain=$("$virsh" -c "$uri" list --name --state-running | head -n 1)
        fi

        if [ -z "''${domain:-}" ]; then
          echo "lxc-enter: no running LXC domain; pass one explicitly" >&2
          exit 2
        fi

        ctl=$("$virsh" -c "$uri" domid "$domain")
        init=
        for p in $(pgrep -P "$ctl" || true); do
          case "$(cat /proc/"$p"/comm 2>/dev/null)" in
            systemd|init) init=$p; break ;;
          esac
          init=''${init:-$p}
        done

        if [ -z "''${init:-}" ]; then
          echo "lxc-enter: no init process under $domain (pid $ctl)" >&2
          exit 1
        fi

        [ "$#" -gt 0 ] || set -- /bin/bash

        # The child inherits PATH, and the host's NixOS PATH is useless inside
        # the container, so hand it a normal FHS PATH.  --preserve-credentials
        # keeps us as root; an idmapped (unprivileged) domain would need -U.
        (
          PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
          export PATH
          exec "$nsenter" --target "$init" --mount --uts --ipc --net --pid \
            --preserve-credentials -- "$@"
        )
      '')
    ];

    # libvirtd only reads its config directory at startup, so the definitions
    # must exist before it starts.  When a definition changes the unit re-runs
    # and bounces libvirtd so the new XML is loaded; running domains survive a
    # libvirtd restart (libvirt reconnects to them).
    #
    # There is no reconciliation for removals: dropping a container from the
    # attrset leaves its XML behind, and autostart will keep starting it until
    # the file is removed (or `virsh -c lxc:/// undefine <name>`).
    systemd.services = lib.mapAttrs' (
      name: c:
      lib.nameValuePair "libvirt-lxc-${name}" {
        description = "Define libvirt LXC domain ${name}";
        before = [ "libvirtd.service" ];
        wantedBy = [ "multi-user.target" ];
        restartTriggers = [ c.xml ];
        path = [ config.systemd.package pkgs.coreutils ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -Dm644 ${c.xml} /var/lib/libvirt/lxc/${name}.xml
          install -d /var/lib/libvirt/lxc/autostart
        ''
        + lib.optionalString c.autostart ''
          ln -sfn ../${name}.xml /var/lib/libvirt/lxc/autostart/${name}.xml
        ''
        + lib.optionalString (!c.autostart) ''
          rm -f /var/lib/libvirt/lxc/autostart/${name}.xml
        ''
        + ''
          systemctl try-restart libvirtd.service
        '';
      }
    ) cfg.containers;
  };
}
